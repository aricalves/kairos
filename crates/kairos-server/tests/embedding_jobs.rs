//! The embedding refresh as jobs of the job queue (KAIROS-T-0375,
//! COLLIERY-I-0613, KAIROS-T-0636).
//!
//! 1. Two servers on one database: each item is embedded once, and each
//!    organization keeps one job row that queues itself again.
//! 2. A batch that fails is tried again, then ends failed with the reason;
//!    its items keep their marks, and the next row embeds them.
//! 3. An item that a write changes while the provider runs stays marked, and
//!    the next run embeds its new text.
//! 4. A run with no mark reads no item table.
//!
//! Runs against the compose Postgres (`angreal services up`). Owns the
//! scratch databases `kairos_embedding_jobs_t0375_*` and
//! `kairos_embedding_jobs_t0636_*`.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use kairos_db::embeddings::counts;
use kairos_db::items::{self, CreateTask};
use kairos_db::jobs as queue;
use kairos_db::models::{BoardLevel, NewUser, TaskType, WorkClass};
use kairos_db::{create_board, provision_tenant, run_public_migrations, schema};
use kairos_embed::{DeterministicProvider, EmbedError, Embedding, EmbeddingProvider, ModelId};
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::embedding::{BatchHandler, EmbeddingService, enqueue_batch};
use kairos_server::jobs::{JobSettings, Worker, kinds};
use uuid::Uuid;

/// A provider that counts each text it embeds, can fail on demand, and can
/// be slow, so that 2 servers overlap.
struct CountingProvider {
    inner: DeterministicProvider,
    seen: Arc<Mutex<HashMap<String, usize>>>,
    calls: AtomicUsize,
    fail: Arc<AtomicBool>,
    delay: Duration,
}

impl CountingProvider {
    fn new(
        seen: Arc<Mutex<HashMap<String, usize>>>,
        fail: Arc<AtomicBool>,
        delay: Duration,
    ) -> Self {
        CountingProvider {
            inner: DeterministicProvider::default(),
            seen,
            calls: AtomicUsize::new(0),
            fail,
            delay,
        }
    }
}

impl EmbeddingProvider for CountingProvider {
    fn model_id(&self) -> &ModelId {
        self.inner.model_id()
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(EmbedError::Failed("the model is away".into()));
        }
        std::thread::sleep(self.delay);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut seen = self.seen.lock().unwrap();
        for text in texts {
            *seen.entry(text.clone()).or_default() += 1;
        }
        drop(seen);
        self.inner.embed(texts)
    }
}

/// A scratch database with the public schema and the organizations
/// `tenants`, each with `items` tasks. Its URL.
fn scratch(name: &str, tenants: &[&str], items: usize) -> common::ScratchUrl {
    let admin = common::recreate_scratch_db(name);
    let url = common::with_database(&common::admin_database_url(), name);
    let mut conn = PgConnection::establish(&url).expect("connect to the scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    for slug in tenants {
        provision_tenant(&mut conn, slug, slug).expect("provision");
        pin(&mut conn, slug);
        let robin = diesel::insert_into(schema::users::table)
            .values(NewUser {
                external_id: format!("dex|robin-{slug}"),
                user_name: format!("dex|robin-{slug}"),
                email: format!("robin@{slug}.test"),
                display_name: "Robin".into(),
            })
            .returning(schema::users::id)
            .get_result::<Uuid>(&mut conn)
            .expect("robin");
        let team = common::seed_team(&mut conn, "Delivery Team", "delivery-team");
        let board = create_board(
            &mut conn,
            BoardLevel::Delivery,
            "Delivery",
            "delivery",
            kairos_db::CodePrefix::FromSlug,
            Some(team),
            Some(robin),
        )
        .expect("board")
        .id;
        for n in 0..items {
            items::create_task(
                &mut conn,
                CreateTask {
                    board_id: board,
                    column_id: None,
                    title: &format!("Task {n} of {slug}"),
                    content: &format!("## Objective\n\nThe work {n} of {slug}.\n"),
                    task_type: TaskType::Task,
                    work_class: WorkClass::Planned,
                    repository_id: None,
                },
                robin,
            )
            .expect("task");
        }
        sql_query("SET search_path TO public")
            .execute(&mut conn)
            .expect("reset");
    }
    common::ScratchUrl::new(admin, url)
}

fn pin(conn: &mut PgConnection, slug: &str) {
    sql_query(format!("SET search_path TO org_{slug}, public"))
        .execute(conn)
        .expect("pin");
}

fn embedded(url: &str, slug: &str, service: &EmbeddingService) -> i64 {
    let mut conn = PgConnection::establish(url).expect("connect");
    pin(&mut conn, slug);
    counts(&mut conn, service.model()).expect("counts").embedded
}

/// The number of marked items of `slug`.
fn dirty(url: &str, slug: &str) -> i64 {
    #[derive(QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }
    let mut conn = PgConnection::establish(url).expect("connect");
    pin(&mut conn, slug);
    sql_query("SELECT count(*)::bigint AS count FROM embedding_dirty")
        .get_result::<Count>(&mut conn)
        .expect("count")
        .count
}

/// Run the open batch row of `slug` now, as if its wait ended.
fn due_now(conn: &mut PgConnection, slug: &str) {
    sql_query(
        "UPDATE public.jobs SET run_after = now() \
         WHERE tenant = $1 AND kind = 'embedding_batch' AND state = 'queued'",
    )
    .bind::<diesel::sql_types::Text, _>(slug)
    .execute(conn)
    .expect("due now");
}

/// The wait of a re-armed row in the tests that drain the worker: long, so
/// that a drain ends.
const HOUR: Duration = Duration::from_secs(3600);

fn settings() -> JobSettings {
    let mut settings = JobSettings {
        heartbeat: Duration::from_millis(100),
        lease: Duration::from_secs(5),
        max_tries: 2,
        poll: Duration::from_millis(50),
        retry_after: Duration::ZERO,
        ..JobSettings::default()
    };
    // One batch at a time on each server, so the 2 servers share the work.
    settings
        .limits
        .insert(kinds::EMBEDDING_BATCH.to_string(), 1);
    settings
}

async fn wait_until(limit: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let until = tokio::time::Instant::now() + limit;
    while !check() {
        assert!(tokio::time::Instant::now() < until, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ===========================================================================
// 1. Two servers: each item is embedded once
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_servers_embed_each_item_once() {
    const ITEMS: usize = 30;
    let url = scratch("kairos_embedding_jobs_t0375_once", &["acme", "beta"], ITEMS);
    let seen = Arc::new(Mutex::new(HashMap::new()));
    let fail = Arc::new(AtomicBool::new(false));

    let mut servers = Vec::new();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let provider = Arc::new(CountingProvider::new(
            Arc::clone(&seen),
            Arc::clone(&fail),
            Duration::from_millis(20),
        ));
        let service = Arc::new(EmbeddingService::new(
            Arc::clone(&provider) as Arc<dyn EmbeddingProvider>
        ));
        let blocking = BlockingTenantPool::new(&url, 4);
        let worker = Arc::new(Worker::new(blocking.clone(), Uuid::new_v4(), settings()));
        tasks.push(tokio::spawn(Arc::clone(&worker).run()));
        tasks.push(tokio::spawn(kairos_server::embedding::run_refresher(
            Arc::clone(&worker),
            blocking,
            Arc::clone(&service),
            Duration::from_millis(100),
            10,
        )));
        servers.push((provider, service));
    }

    let service = Arc::clone(&servers[0].1);
    wait_until(Duration::from_secs(60), "every item embedded", || {
        ["acme", "beta"]
            .iter()
            .all(|slug| embedded(&url, slug, &service) == ITEMS as i64)
    })
    .await;
    // More passes over the current pages: they embed nothing.
    tokio::time::sleep(Duration::from_secs(2)).await;
    for task in tasks {
        task.abort();
    }

    let seen = seen.lock().unwrap();
    let twice: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert!(twice.is_empty(), "texts embedded more than once: {twice:?}");
    assert!(
        seen.len() >= 2 * ITEMS,
        "a primary vector of each item: {} texts",
        seen.len()
    );
    for (i, (provider, _)) in servers.iter().enumerate() {
        assert!(
            provider.calls.load(Ordering::SeqCst) > 0,
            "server {i} embedded a batch"
        );
    }

    // One row of each organization, queued again after each run: no
    // finished rows, and no batch failed.
    let mut conn = PgConnection::establish(&url).expect("connect");
    let jobs = queue::list(&mut conn, kinds::EMBEDDING_BATCH).expect("jobs");
    for slug in ["acme", "beta"] {
        let rows: Vec<_> = jobs
            .iter()
            .filter(|j| j.tenant.as_deref() == Some(slug))
            .collect();
        assert_eq!(rows.len(), 1, "{slug}: one row: {rows:?}");
        assert!(
            rows[0].state == "queued" || rows[0].state == "leased",
            "{slug}: the row is open: {rows:?}"
        );
    }
    assert!(
        jobs.iter().all(|j| j.state != "failed"),
        "no batch failed: {jobs:?}"
    );
    assert_eq!(dirty(&url, "acme") + dirty(&url, "beta"), 0, "no mark left");
}

// ===========================================================================
// 2. A failed batch: tried again, failed with the reason, its items wait
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_batch_is_tried_again_then_fails_and_its_items_wait() {
    const ITEMS: usize = 5;
    let url = scratch("kairos_embedding_jobs_t0375_failed", &["acme"], ITEMS);
    let seen = Arc::new(Mutex::new(HashMap::new()));
    let fail = Arc::new(AtomicBool::new(true));
    let provider = Arc::new(CountingProvider::new(
        Arc::clone(&seen),
        Arc::clone(&fail),
        Duration::ZERO,
    ));
    let service = Arc::new(EmbeddingService::new(
        provider as Arc<dyn EmbeddingProvider>,
    ));
    let blocking = BlockingTenantPool::new(&url, 4);
    let worker = Arc::new(Worker::new(blocking.clone(), Uuid::new_v4(), settings()));
    BatchHandler::new(blocking, Arc::clone(&service), 10, HOUR).register(&worker);
    let mut conn = PgConnection::establish(&url).expect("connect");
    assert_eq!(
        dirty(&url, "acme"),
        ITEMS as i64,
        "each create marked its item"
    );

    let first = enqueue_batch(&mut conn, "acme", 2)
        .expect("enqueue")
        .expect("added");
    worker.drain().await;
    let failed = queue::get(&mut conn, first.id).expect("get").expect("row");
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.tries, 2, "tried again before it failed");
    let reason = failed.last_error.clone().unwrap_or_default();
    assert!(
        reason.contains("embedding batch failed") && reason.contains("the model is away"),
        "the reason: {reason}"
    );
    assert_eq!(embedded(&url, "acme", &service), 0, "the items wait");
    assert_eq!(
        dirty(&url, "acme"),
        ITEMS as i64,
        "the items keep their marks"
    );

    // The model is back: a new row replaces the failed one and embeds them.
    fail.store(false, Ordering::SeqCst);
    let second = enqueue_batch(&mut conn, "acme", 2)
        .expect("enqueue")
        .expect("added");
    worker.drain().await;
    assert_eq!(embedded(&url, "acme", &service), ITEMS as i64);
    assert_eq!(dirty(&url, "acme"), 0, "the run cleared the marks");
    let row = queue::get(&mut conn, second.id).expect("get").expect("row");
    assert_eq!(row.state, "queued", "the row queued itself again: {row:?}");
    assert_eq!(row.tries, 0);
    assert_eq!(row.last_error, None);
    assert!(
        row.run_after > chrono::Utc::now() + chrono::Duration::minutes(59),
        "a page that was not full waits for the interval: {row:?}"
    );
    assert!(
        enqueue_batch(&mut conn, "acme", 2)
            .expect("enqueue")
            .is_none(),
        "one open row for each organization"
    );
    let rows = queue::list(&mut conn, kinds::EMBEDDING_BATCH).expect("jobs");
    let states: Vec<(Uuid, &str)> = rows.iter().map(|j| (j.id, j.state.as_str())).collect();
    assert_eq!(
        states,
        vec![(second.id, "queued")],
        "the failed row went when the new row came"
    );

    // A second run with nothing marked embeds nothing and keeps the row.
    let calls = seen.lock().unwrap().len();
    due_now(&mut conn, "acme");
    worker.drain().await;
    assert_eq!(seen.lock().unwrap().len(), calls, "no text to the provider");
    let rows = queue::list(&mut conn, kinds::EMBEDDING_BATCH).expect("jobs");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, second.id, "the same row, not a new one");
    assert_eq!(rows[0].state, "queued");
}

// ===========================================================================
// 3. An item changed while the provider runs stays marked
// ===========================================================================

/// A provider that runs `during` once, on its first call, before it embeds:
/// a write that lands while the provider has the texts.
struct WriteDuringProvider {
    inner: DeterministicProvider,
    seen: Arc<Mutex<Vec<String>>>,
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl EmbeddingProvider for WriteDuringProvider {
    fn model_id(&self) -> &ModelId {
        self.inner.model_id()
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError> {
        if let Some(during) = self.during.lock().unwrap().take() {
            during();
        }
        self.seen.lock().unwrap().extend(texts.iter().cloned());
        self.inner.embed(texts)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_item_changed_while_the_provider_runs_stays_marked() {
    use kairos_core::short_code::ItemType;
    #[derive(QueryableByName)]
    struct Task {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        id: Uuid,
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        created_by: Uuid,
    }
    let url = scratch("kairos_embedding_jobs_t0636_changed", &["acme"], 1);
    let mut conn = PgConnection::establish(&url).expect("connect");
    pin(&mut conn, "acme");
    let task: Task = sql_query("SELECT id, created_by FROM tasks")
        .get_result(&mut conn)
        .expect("the task");

    let write_url = url.to_string();
    let (id, actor) = (task.id, task.created_by);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(WriteDuringProvider {
        inner: DeterministicProvider::default(),
        seen: Arc::clone(&seen),
        during: Mutex::new(Some(Box::new(move || {
            let mut conn = PgConnection::establish(&write_url).expect("connect");
            pin(&mut conn, "acme");
            items::update_item_content(
                &mut conn,
                ItemType::Task,
                id,
                items::ContentUpdate {
                    new_title: None,
                    new_content: "## Objective\n\nThe new text, written during the call.\n",
                    expected_version: 1,
                },
                actor,
            )
            .expect("the write during the call");
        }))),
    });
    let service = Arc::new(EmbeddingService::new(
        provider as Arc<dyn EmbeddingProvider>,
    ));
    let blocking = BlockingTenantPool::new(&url, 4);
    let worker = Arc::new(Worker::new(blocking.clone(), Uuid::new_v4(), settings()));
    BatchHandler::new(blocking, Arc::clone(&service), 10, HOUR).register(&worker);
    sql_query("SET search_path TO public")
        .execute(&mut conn)
        .expect("reset");
    enqueue_batch(&mut conn, "acme", 2)
        .expect("enqueue")
        .expect("added");

    // The first run embeds the old text. The write during the call gave a
    // new mark, so the clear keeps it.
    worker.drain().await;
    assert_eq!(
        embedded(&url, "acme", &service),
        1,
        "the old text has a vector"
    );
    assert_eq!(dirty(&url, "acme"), 1, "the item stays marked");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|t| !t.contains("written during")),
        "the first run had the old text"
    );

    // The next run reads the new text.
    due_now(&mut conn, "acme");
    worker.drain().await;
    assert_eq!(dirty(&url, "acme"), 0, "the second run cleared the mark");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|t| t.contains("written during")),
        "the second run embedded the new text"
    );
}

// ===========================================================================
// 4. A run with no mark reads no item table
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_with_no_mark_reads_no_item_table() {
    let url = scratch("kairos_embedding_jobs_t0636_idle", &["acme"], 3);
    let seen = Arc::new(Mutex::new(HashMap::new()));
    let provider = Arc::new(CountingProvider::new(
        Arc::clone(&seen),
        Arc::new(AtomicBool::new(false)),
        Duration::ZERO,
    ));
    let service = Arc::new(EmbeddingService::new(
        provider as Arc<dyn EmbeddingProvider>,
    ));
    let blocking = BlockingTenantPool::new(&url, 4);
    let worker = Arc::new(Worker::new(blocking.clone(), Uuid::new_v4(), settings()));
    BatchHandler::new(blocking, Arc::clone(&service), 10, HOUR).register(&worker);
    let mut conn = PgConnection::establish(&url).expect("connect");
    let row = enqueue_batch(&mut conn, "acme", 2)
        .expect("enqueue")
        .expect("added");
    worker.drain().await;
    assert_eq!(embedded(&url, "acme", &service), 3);
    assert_eq!(dirty(&url, "acme"), 0);

    // Another session holds the item tables and the vectors. A run that read
    // any of them would wait for the lock, or fail on the lock timeout.
    let mut locker = PgConnection::establish(&url).expect("connect");
    sql_query("BEGIN").execute(&mut locker).expect("begin");
    sql_query(
        "LOCK TABLE org_acme.strategies, org_acme.initiatives, org_acme.tasks, \
         org_acme.documents, org_acme.adrs, org_acme.item_embeddings, org_acme.item_chunks \
         IN ACCESS EXCLUSIVE MODE",
    )
    .execute(&mut locker)
    .expect("lock");
    due_now(&mut conn, "acme");
    tokio::time::timeout(Duration::from_secs(20), worker.drain())
        .await
        .expect("the run ended while the tables were locked");
    sql_query("ROLLBACK")
        .execute(&mut locker)
        .expect("rollback");

    let after = queue::get(&mut conn, row.id).expect("get").expect("row");
    assert_eq!(after.state, "queued", "the run ended well: {after:?}");
    assert_eq!(after.tries, 0);
    assert_eq!(after.last_error, None, "{after:?}");
    assert!(after.run_after > chrono::Utc::now() + chrono::Duration::minutes(59));
}
