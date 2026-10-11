//! The embedding refresh as jobs of the job queue (KAIROS-T-0375,
//! COLLIERY-I-0613).
//!
//! 1. Two servers on one database: each item is embedded once.
//! 2. A batch that fails is tried again, then ends failed with the reason;
//!    its items stay stale, and the next batch starts at the same place.
//!
//! Runs against the compose Postgres (`angreal services up`). Owns the
//! scratch databases `kairos_embedding_jobs_t0375_*`.

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
use kairos_server::embedding::{BatchHandler, BatchPayload, EmbeddingService, enqueue_batch};
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
fn scratch(name: &str, tenants: &[&str], items: usize) -> String {
    let _admin = common::recreate_scratch_db(name);
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
    url
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

    // One finished row of each organization keeps the cursor; no batch failed.
    let mut conn = PgConnection::establish(&url).expect("connect");
    let jobs = queue::list(&mut conn, kinds::EMBEDDING_BATCH).expect("jobs");
    for slug in ["acme", "beta"] {
        let done = jobs
            .iter()
            .filter(|j| j.tenant.as_deref() == Some(slug) && j.state == "done")
            .count();
        assert!(done <= 2, "{slug}: {done} finished rows");
    }
    assert!(
        jobs.iter().all(|j| j.state != "failed"),
        "no batch failed: {jobs:?}"
    );
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
    BatchHandler::new(blocking, Arc::clone(&service), 10).register(&worker);
    let mut conn = PgConnection::establish(&url).expect("connect");

    let first = enqueue_batch(&mut conn, "acme", Duration::ZERO, 2)
        .expect("enqueue")
        .expect("added");
    worker.drain().await;
    let failed = queue::get(&mut conn, first.id).expect("get").expect("row");
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.tries, 2, "tried again before it failed");
    let reason = failed.last_error.clone().unwrap_or_default();
    assert!(
        reason.contains("at item 0") && reason.contains("the model is away"),
        "the reason: {reason}"
    );
    assert_eq!(embedded(&url, "acme", &service), 0, "the items wait");

    // The model is back: the next batch starts at the same place.
    fail.store(false, Ordering::SeqCst);
    let second = enqueue_batch(&mut conn, "acme", Duration::ZERO, 2)
        .expect("enqueue")
        .expect("added");
    let payload: BatchPayload = serde_json::from_str(&second.payload).expect("payload");
    assert_eq!(payload.offset, 0);
    worker.drain().await;
    assert_eq!(embedded(&url, "acme", &service), ITEMS as i64);
    let done = queue::get(&mut conn, second.id).expect("get").expect("row");
    assert_eq!(done.state, "done");
    let payload: BatchPayload = serde_json::from_str(&done.payload).expect("payload");
    assert_eq!(payload.next, Some(0), "the page reached the end and wraps");

    // The next batch waits for the interval after the last one, and the
    // older finished rows go: one row keeps the cursor.
    let third = enqueue_batch(&mut conn, "acme", Duration::ZERO, 2)
        .expect("enqueue")
        .expect("added");
    worker.drain().await;
    let fourth = enqueue_batch(&mut conn, "acme", Duration::from_secs(3600), 2)
        .expect("enqueue")
        .expect("added");
    assert!(fourth.run_after > chrono::Utc::now() + chrono::Duration::minutes(59));
    assert!(
        enqueue_batch(&mut conn, "acme", Duration::ZERO, 2)
            .expect("enqueue")
            .is_none(),
        "one open batch for each organization"
    );
    let rows = queue::list(&mut conn, kinds::EMBEDDING_BATCH).expect("jobs");
    let states: Vec<(Uuid, &str)> = rows.iter().map(|j| (j.id, j.state.as_str())).collect();
    assert_eq!(
        states,
        vec![
            (first.id, "failed"),
            (third.id, "done"),
            (fourth.id, "queued")
        ]
    );
}
