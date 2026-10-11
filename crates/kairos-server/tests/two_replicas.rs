//! Two servers on one database (KAIROS-T-0378, COLLIERY-I-0613): the proof
//! of the initiative, end to end.
//!
//! Two app states on one database, each with the background work of a
//! server: its job worker, the code-index builder with the periodic sweep,
//! the embedding refresher, the periodic retention pass and the webhook
//! deliveries. Then:
//!
//! 1. Each build, each embedding batch, each run of a periodic pass and each
//!    delivery runs once, and both servers take jobs.
//! 2. Failed logins on one server count on the other.
//!
//! A server killed during a build (its job is leased again after the lease
//! and ends ok with the summaries that it made) is in `code_index.rs`:
//! `a_build_stopped_on_one_server_finishes_on_another_with_an_empty_folder`
//! (a killed server and an empty `KAIROS_CODE_INDEX_DIR` on the other) and
//! `a_killed_servers_build_is_re_leased_and_ends_ok_with_its_summaries`.
//!
//! Runs against the compose Postgres (`angreal services up`). No Dex is
//! needed. Owns the scratch database `kairos_two_replicas_t0378`.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use kairos_db::items::{self, CreateTask};
use kairos_db::jobs::{self as queue, Job};
use kairos_db::models::repositories::NewRepository;
use kairos_db::models::{BoardLevel, Forge, NewUser, TaskType, WorkClass};
use kairos_db::{TenantPool, create_board, provision_tenant, run_public_migrations, schema};
use kairos_embed::{DeterministicProvider, EmbedError, Embedding, EmbeddingProvider, ModelId};
use kairos_index::{FakeSummarizer, SummaryRequest};
use kairos_server::app;
use kairos_server::code_index::{CodeIndexService, SummarizerSource, run_builder};
use kairos_server::embedding::EmbeddingService;
use kairos_server::jobs::{JobHandle, JobSettings, kinds};
use kairos_server::periodic::{self, PassFn};
use kairos_server::secrets::SecretsKey;
use kairos_server::webhooks::{WebhookSettings, new_target};
use tower::ServiceExt as _;
use uuid::Uuid;

const DB: &str = "kairos_two_replicas_t0378";
const TENANT: &str = "acme";
const REPOSITORIES: usize = 4;
const ITEMS: usize = 24;
const DELIVERIES: usize = 30;
const MAX_FAILURES: u32 = 3;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@kairos.test")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@kairos.test")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git repository of a small Python package with code of its own.
fn python_repository(dir: &Path, n: usize) {
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(dir.join("pkg/__init__.py"), "").unwrap();
    let code: String = (0..4)
        .map(|i| format!("def step_{n}_{i}(x):\n    return x * {i} + {n}\n\n\n"))
        .collect();
    std::fs::write(dir.join(format!("pkg/work_{n}.py")), code).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

fn pin(conn: &mut PgConnection) {
    sql_query(format!("SET search_path TO org_{TENANT}, public"))
        .execute(conn)
        .expect("pin");
}

fn conn(url: &str) -> PgConnection {
    PgConnection::establish(url).expect("connect")
}

fn key() -> SecretsKey {
    SecretsKey::from_bytes([42u8; 32])
}

/// The scratch database: the tenant, a board with tasks, the repositories
/// (with their git repositories in `work`), and a webhook target of the
/// board for `hook_url` with its deliveries. The URL of the database.
fn world(work: &Path, hook_url: &str) -> String {
    let _admin = common::recreate_scratch_db(DB);
    let url = common::with_database(&common::admin_database_url(), DB);
    let mut conn = conn(&url);
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, TENANT, "Acme Inc").expect("provision");
    pin(&mut conn);
    let robin = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: "dex|robin".into(),
            user_name: "robin".into(),
            email: "robin@acme.test".into(),
            display_name: "Robin".into(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(&mut conn)
        .expect("robin");
    let team = common::seed_team(&mut conn, "Platform", "platform");
    let board = create_board(
        &mut conn,
        BoardLevel::Delivery,
        "Platform",
        "platform",
        kairos_db::CodePrefix::FromSlug,
        Some(team),
        Some(robin),
    )
    .expect("board")
    .id;
    for n in 0..ITEMS {
        items::create_task(
            &mut conn,
            CreateTask {
                board_id: board,
                column_id: None,
                title: &format!("Task {n}"),
                content: &format!("## Objective\n\nThe work {n}.\n"),
                task_type: TaskType::Task,
                work_class: WorkClass::Planned,
                repository_id: None,
            },
            robin,
        )
        .expect("task");
    }
    for n in 0..REPOSITORIES {
        let slug = format!("service-{n}");
        python_repository(&work.join(&slug), n);
        kairos_db::repositories::create(
            &mut conn,
            NewRepository {
                slug: slug.clone(),
                forge: Forge::Github,
                repo_full_name: format!("acme/{slug}"),
                repo_url: format!("https://github.com/acme/{slug}"),
                default_branch: "main".into(),
                team_id: team,
                description: String::new(),
                created_by: robin,
                updated_by: robin,
            },
        )
        .expect("repository");
    }
    let (target, _) = new_target(&key(), TENANT, board, "Runner", hook_url, robin);
    let target = kairos_db::webhooks::insert_target(&mut conn, &target).expect("target");
    for n in 0..DELIVERIES {
        kairos_db::webhooks::queue_delivery(
            &mut conn,
            target.id,
            "ping",
            None,
            None,
            &serde_json::json!({ "n": n }),
        )
        .expect("queue")
        .expect("a target");
    }
    url
}

/// The fake summarizer, slowed down so that the builds of the 2 servers
/// overlap, which counts each summary by its key.
#[derive(Clone)]
struct CountingSummarizers {
    seen: Arc<Mutex<HashMap<String, usize>>>,
}

struct CountingSummarizer {
    inner: FakeSummarizer,
    seen: Arc<Mutex<HashMap<String, usize>>>,
}

impl kairos_index::Summarizer for CountingSummarizer {
    fn model(&self) -> String {
        self.inner.model()
    }

    fn summarize(&mut self, request: &SummaryRequest) -> Result<String, String> {
        std::thread::sleep(Duration::from_millis(20));
        *self
            .seen
            .lock()
            .unwrap()
            .entry(request.key.clone())
            .or_default() += 1;
        self.inner.summarize(request)
    }
}

impl SummarizerSource for CountingSummarizers {
    fn run(
        &self,
        job: &mut dyn FnMut(
            &mut dyn kairos_index::Summarizer,
        )
            -> Result<kairos_index::UpdateReport, kairos_index::IndexError>,
    ) -> Result<kairos_index::UpdateReport, kairos_index::IndexError> {
        job(&mut CountingSummarizer {
            inner: FakeSummarizer::default(),
            seen: Arc::clone(&self.seen),
        })
    }

    fn model_name(&self) -> String {
        FakeSummarizer::default().model
    }
}

/// The embedding provider of the refresher, which counts each text.
struct CountingProvider {
    inner: DeterministicProvider,
    seen: Arc<Mutex<HashMap<String, usize>>>,
}

impl EmbeddingProvider for CountingProvider {
    fn model_id(&self) -> &ModelId {
        self.inner.model_id()
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, EmbedError> {
        std::thread::sleep(Duration::from_millis(10));
        let mut seen = self.seen.lock().unwrap();
        for text in texts {
            *seen.entry(text.clone()).or_default() += 1;
        }
        drop(seen);
        self.inner.embed(texts)
    }
}

/// The retention pass, which records the job of each run and the server
/// that ran it.
fn counted(inner: PassFn, runs: &Arc<Mutex<Vec<(Uuid, Uuid)>>>, server: Uuid) -> PassFn {
    let runs = Arc::clone(runs);
    Arc::new(move |handle: JobHandle| {
        runs.lock().unwrap().push((handle.job().id, server));
        inner(handle)
    })
}

fn job_settings() -> JobSettings {
    let mut settings = JobSettings {
        heartbeat: Duration::from_millis(200),
        lease: Duration::from_secs(5),
        poll: Duration::from_millis(50),
        retry_after: Duration::ZERO,
        ..JobSettings::default()
    };
    // One of each kind at a time on a server, so the 2 servers share the
    // work.
    for kind in [kinds::EMBEDDING_BATCH, kinds::WEBHOOK_DELIVERY] {
        settings.limits.insert(kind.to_string(), 1);
    }
    settings
}

/// One server: its app state, with the background work of `serve`.
struct Server {
    router: Router,
    instance: Uuid,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[allow(clippy::too_many_arguments)]
async fn server(
    url: &str,
    work: &Path,
    folder: &Path,
    summaries: &Arc<Mutex<HashMap<String, usize>>>,
    texts: &Arc<Mutex<HashMap<String, usize>>>,
    retention_runs: &Arc<Mutex<Vec<(Uuid, Uuid)>>>,
) -> Server {
    let pool = TenantPool::new(url, 4).await.expect("pool");
    let auth = Arc::new(
        kairos_server::middleware::auth::Authenticator::with_static_keys(
            common::ISSUER,
            common::AUDIENCE,
            [],
        ),
    );
    let mut config = common::base_config(url);
    config.secrets_key = Some(key());
    config.local_auth = true;
    config.auth_max_failures = MAX_FAILURES;
    config.jobs = job_settings();
    config.webhooks = WebhookSettings {
        retry_base: Duration::from_millis(20),
        // The test receiver is on 127.0.0.1.
        allow_private: true,
        ..WebhookSettings::default()
    };
    let state = app::state_with(config, pool, auth);
    let instance = state.jobs.instance();
    let mut tasks = Vec::new();

    // The builder, with the periodic sweep (KAIROS-T-0374, KAIROS-T-0376).
    let root = work.to_path_buf();
    let service = Arc::new(
        CodeIndexService::new(
            folder.to_path_buf(),
            Arc::new(move |repo: &kairos_db::models::repositories::Repository| {
                root.join(&repo.slug).display().to_string()
            }),
        )
        .with_instance(instance)
        .with_secrets_key(Some(key())),
    );
    run_builder(
        Arc::clone(&state.jobs),
        state.blocking.clone(),
        service,
        Some(Arc::new(CountingSummarizers {
            seen: Arc::clone(summaries),
        })),
        Arc::new(DeterministicProvider::default()),
        Duration::from_millis(300),
    )
    .await;

    // The embedding refresher (KAIROS-T-0375).
    let embedding = Arc::new(EmbeddingService::new(Arc::new(CountingProvider {
        inner: DeterministicProvider::default(),
        seen: Arc::clone(texts),
    })));
    tasks.push(tokio::spawn(kairos_server::embedding::run_refresher(
        Arc::clone(&state.jobs),
        state.blocking.clone(),
        embedding,
        Duration::from_millis(100),
        4,
    )));

    // The retention pass, each 300 ms (KAIROS-T-0376).
    state
        .jobs
        .add_periodic(
            periodic::RETENTION,
            Duration::from_millis(300),
            counted(
                periodic::retention_pass(url.to_string(), Default::default()),
                retention_runs,
                instance,
            ),
        )
        .await
        .expect("the retention pass");

    // The worker of the server. The webhook deliveries are registered by
    // the app state (KAIROS-T-0384).
    tasks.push(tokio::spawn(Arc::clone(&state.jobs).run()));
    Server {
        router: app::router(state),
        instance,
        tasks,
    }
}

fn jobs_of(url: &str, kind: &str) -> Vec<Job> {
    queue::list(&mut conn(url), kind).expect("jobs")
}

fn ok_runs(url: &str) -> HashMap<Uuid, Vec<String>> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        repository_id: Uuid,
        #[diesel(sql_type = diesel::sql_types::Text)]
        outcome: String,
    }
    let mut conn = conn(url);
    pin(&mut conn);
    let rows: Vec<Row> = sql_query("SELECT repository_id, outcome FROM code_index_builds")
        .load(&mut conn)
        .expect("runs");
    let mut runs: HashMap<Uuid, Vec<String>> = HashMap::new();
    for row in rows {
        runs.entry(row.repository_id).or_default().push(row.outcome);
    }
    runs
}

fn embedded_items(url: &str) -> i64 {
    let mut conn = conn(url);
    pin(&mut conn);
    let model = EmbeddingService::new(Arc::new(DeterministicProvider::default()));
    kairos_db::embeddings::counts(&mut conn, model.model())
        .expect("counts")
        .embedded
}

fn delivered(url: &str) -> usize {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = conn(url);
    pin(&mut conn);
    sql_query("SELECT count(*) AS n FROM webhook_deliveries WHERE status = 'delivered'")
        .get_result::<Row>(&mut conn)
        .expect("deliveries")
        .n as usize
}

async fn login_as(router: &Router, email: &str, password: &str) -> StatusCode {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "email": email, "password": password }).to_string(),
        ))
        .expect("request");
    router
        .clone()
        .oneshot(request)
        .await
        .expect("response")
        .status()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_servers_on_one_database_run_each_job_once() {
    // Given one database with 4 repositories with no index, 24 tasks with
    // no vector, and 30 webhook deliveries
    let work = tempfile::tempdir().unwrap();
    let receiver = common::hook_http::serve().await;
    let url = world(work.path(), &receiver.url);

    // And 2 servers on it, each with the background work of a server and
    // a code-index folder of its own
    let summaries = Arc::new(Mutex::new(HashMap::new()));
    let texts = Arc::new(Mutex::new(HashMap::new()));
    let retention_runs = Arc::new(Mutex::new(Vec::new()));
    let mut servers = Vec::new();
    for folder in ["server-a", "server-b"] {
        servers.push(
            server(
                &url,
                work.path(),
                &work.path().join(folder),
                &summaries,
                &texts,
                &retention_runs,
            )
            .await,
        );
    }

    // When they run until each repository has an index, each task its
    // vectors, each delivery its answer, and retention has run 5 times
    let until = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let runs = ok_runs(&url);
        let built =
            runs.len() == REPOSITORIES && runs.values().all(|r| r.iter().any(|o| o == "ok"));
        let done = built
            && embedded_items(&url) == ITEMS as i64
            && delivered(&url) == DELIVERIES
            && retention_runs.lock().unwrap().len() >= 5;
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "timed out: runs {runs:?}, embedded {}, delivered {}, retention runs {}",
            embedded_items(&url),
            delivered(&url),
            retention_runs.lock().unwrap().len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // More passes: the sweep, the refresher and retention run again, and
    // find nothing new to build or embed.
    tokio::time::sleep(Duration::from_secs(1)).await;
    for server in &mut servers {
        for task in server.tasks.drain(..) {
            task.abort();
        }
    }

    // Then each repository was built once: one run, ok, of one job of one
    // try, and no summary was made twice
    let runs = ok_runs(&url);
    assert_eq!(runs.len(), REPOSITORIES, "{runs:?}");
    for (repository, outcomes) in &runs {
        assert_eq!(outcomes, &["ok"], "{repository}: {outcomes:?}");
    }
    let builds = jobs_of(&url, kinds::EMBEDDED_BUILD);
    assert_eq!(builds.len(), REPOSITORIES, "{builds:?}");
    assert!(builds.iter().all(|j| j.state == "done" && j.tries == 1));
    let twice: Vec<_> = summaries
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, n)| (k.clone(), *n))
        .collect();
    assert!(twice.is_empty(), "summaries made twice: {twice:?}");

    // And each task was embedded once, by batches of one try each
    let twice: Vec<_> = texts
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, n)| (k.clone(), *n))
        .collect();
    assert!(twice.is_empty(), "texts embedded twice: {twice:?}");
    let batches = jobs_of(&url, kinds::EMBEDDING_BATCH);
    assert!(!batches.is_empty());
    assert!(
        batches.iter().all(|j| j.state != "failed" && j.tries <= 1),
        "{batches:?}"
    );

    // And each run of a periodic pass ran once, and a pass has one open run
    // at most
    let runs = retention_runs.lock().unwrap().clone();
    let mut per_job: HashMap<Uuid, usize> = HashMap::new();
    for (job, _) in &runs {
        *per_job.entry(*job).or_default() += 1;
    }
    assert!(
        per_job.values().all(|n| *n == 1),
        "a run of retention ran twice: {per_job:?}"
    );
    let passes = jobs_of(&url, kinds::PERIODIC);
    for name in [periodic::RETENTION, periodic::CODE_INDEX_SWEEP] {
        let of_pass: Vec<_> = passes
            .iter()
            .filter(|j| j.dedupe_key.as_deref() == Some(periodic::key(name).as_str()))
            .collect();
        assert!(of_pass.len() >= 3, "{name}: {of_pass:?}");
        let open = of_pass
            .iter()
            .filter(|j| j.state == "queued" || j.state == "leased")
            .count();
        assert!(open <= 1, "{name}: {open} open runs");
        assert!(
            of_pass.iter().all(|j| j.state != "failed" && j.tries <= 1),
            "{name}: {of_pass:?}"
        );
    }

    // And each delivery arrived once
    let mut count: HashMap<String, usize> = HashMap::new();
    for request in receiver.received() {
        *count
            .entry(request.headers["x-kairos-delivery"].clone())
            .or_default() += 1;
    }
    assert_eq!(count.len(), DELIVERIES, "each delivery arrived");
    assert!(
        count.values().all(|n| *n == 1),
        "a delivery arrived twice: {count:?}"
    );

    // And both servers took jobs
    let mut all = Vec::new();
    for kind in [
        kinds::EMBEDDED_BUILD,
        kinds::EMBEDDING_BATCH,
        kinds::PERIODIC,
        kinds::WEBHOOK_DELIVERY,
    ] {
        all.extend(jobs_of(&url, kind));
    }
    for server in &servers {
        let taken = all
            .iter()
            .filter(|j| j.leased_by == Some(server.instance))
            .count();
        assert!(taken > 0, "server {} took no job", server.instance);
    }

    // And failed logins on one server count on the other: the attempts go
    // to the 2 servers in turn, and the lockout holds on both
    for attempt in 1..=MAX_FAILURES {
        let router = &servers[(attempt % 2) as usize].router;
        assert_eq!(
            login_as(router, "robin@acme.test", "not the password").await,
            StatusCode::UNAUTHORIZED,
            "login {attempt}"
        );
    }
    for server in &servers {
        assert_eq!(
            login_as(&server.router, "robin@acme.test", "not the password").await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        login_as(&servers[0].router, "sam@acme.test", "not the password").await,
        StatusCode::UNAUTHORIZED,
        "another account is not locked"
    );
}
