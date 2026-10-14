//! The roles of the server (KAIROS-T-0394, KAIROS-A-0027): a server with
//! `KAIROS_ROLE=api` takes no code index build, and a server with
//! `KAIROS_ROLE=worker` takes the builds. A worker that dies during a build
//! leaves the API serving, and the job records the failure.
//!
//! 1. An `api` server takes no build job and adds no sweep; its list of the
//!    runs says that no builder runs. A `worker` takes the build, runs the
//!    sweep, and then the note goes.
//! 2. A worker killed during a build (its runtime stops, as a process that
//!    runs out of memory stops): the API serves all the time; the next
//!    worker fails the job with the reason, and the run ends `failed`.
//!
//! Runs against the compose Postgres (`angreal services up`). No Dex is
//! needed: the API is called with a local account. Owns the scratch
//! databases `kairos_server_roles_t0394_*`.

mod common;

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
use http_body_util::BodyExt as _;
use kairos_db::jobs::{self as queue, Job};
use kairos_db::models::repositories::NewRepository;
use kairos_db::models::{Forge, NewOrganizationMember, NewUser, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_embed::{DeterministicProvider, EmbeddingProvider};
use kairos_index::FakeSummarizer;
use kairos_server::app;
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::code_index::{
    BuildHandler, CodeIndexService, FakeSummarizers, SummarizerSource, enqueue_first_builds,
    job_key, run_builder,
};
use kairos_server::config::ServerRole;
use kairos_server::jobs::{JobSettings, Worker, kinds};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const TENANT: &str = "acme";
const SLUG: &str = "service-0";
const EMAIL: &str = "robin@acme.test";
const PASSWORD: &str = "robin-password-t0394";

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

/// A git repository of a small Python package.
fn python_repository(dir: &Path) {
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(dir.join("pkg/__init__.py"), "").unwrap();
    let code: String = (0..6)
        .map(|i| format!("def step_{i}(x):\n    return x * {i} + 1\n\n\n"))
        .collect();
    std::fs::write(dir.join("pkg/work.py"), code).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

fn conn(url: &str) -> PgConnection {
    PgConnection::establish(url).expect("connect")
}

fn pin(conn: &mut PgConnection) {
    sql_query(format!("SET search_path TO org_{TENANT}, public"))
        .execute(conn)
        .expect("pin");
}

/// The scratch database `db`: the tenant, an organization admin with a
/// password, a team and one repository whose git repository is in `work`.
/// The guard of the database (it drops the database when the test ends,
/// KAIROS-T-0445: keep it to the end), its URL, and the id of the
/// repository.
fn world(db: &str, work: &Path) -> (common::ScratchConn, String, Uuid) {
    let admin = common::recreate_scratch_db(db);
    let url = common::with_database(&common::admin_database_url(), db);
    let mut conn = conn(&url);
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, TENANT, "Acme Inc").expect("provision");
    let org: Uuid = organizations::table
        .filter(organizations::slug.eq(TENANT))
        .select(organizations::id)
        .first(&mut conn)
        .expect("org");
    let robin: Uuid = diesel::insert_into(users::table)
        .values(NewUser {
            external_id: format!("local:{EMAIL}"),
            user_name: "robin".into(),
            email: EMAIL.into(),
            display_name: "Robin".into(),
        })
        .returning(users::id)
        .get_result(&mut conn)
        .expect("robin");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org,
            user_id: robin,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("membership");
    let hash = kairos_server::local_auth::hash_password(PASSWORD).expect("hash");
    kairos_db::local_auth::set_password(&mut conn, robin, &hash).expect("password");
    pin(&mut conn);
    let team = common::seed_team(&mut conn, "Platform", "platform");
    python_repository(&work.join(SLUG));
    let repo = kairos_db::repositories::create(
        &mut conn,
        NewRepository {
            slug: SLUG.into(),
            forge: Forge::Github,
            repo_full_name: format!("acme/{SLUG}"),
            repo_url: format!("https://github.com/acme/{SLUG}"),
            default_branch: "main".into(),
            team_id: team,
            description: String::new(),
            created_by: robin,
            updated_by: robin,
        },
    )
    .expect("repository");
    (admin, url, repo.id)
}

/// Short times, so that a dead worker shows in seconds.
fn quick_jobs() -> JobSettings {
    JobSettings {
        heartbeat: Duration::from_millis(250),
        lease: Duration::from_secs(3),
        poll: Duration::from_millis(100),
        retry_after: Duration::ZERO,
        ..JobSettings::default()
    }
}

/// The app state of a server with the role `role`.
async fn state(url: &str, role: ServerRole) -> app::AppState {
    let pool = TenantPool::new(url, 4).await.expect("pool");
    let auth = Arc::new(
        kairos_server::middleware::auth::Authenticator::with_static_keys(
            common::ISSUER,
            common::AUDIENCE,
            [],
        ),
    );
    let mut config = common::base_config(url);
    config.local_auth = true;
    config.role = role;
    config.jobs = quick_jobs();
    app::state_with(config, pool, auth)
}

/// The code index service of a server: its clones in `clones`, each
/// repository fetched from its folder in `work`.
fn service(work: &Path, clones: &Path, instance: Uuid) -> Arc<CodeIndexService> {
    let root = work.to_path_buf();
    Arc::new(
        CodeIndexService::new(
            clones.to_path_buf(),
            Arc::new(move |repo: &kairos_db::models::repositories::Repository| {
                root.join(&repo.slug).display().to_string()
            }),
        )
        .with_instance(instance),
    )
}

/// What `serve` starts for the builder (KAIROS-T-0374): the build handler
/// and the periodic sweep, on the job worker of `state`.
async fn start_builder(state: &app::AppState, service: Arc<CodeIndexService>) {
    run_builder(
        Arc::clone(&state.jobs),
        state.blocking.clone(),
        service,
        Some(Arc::new(FakeSummarizers)),
        Arc::new(DeterministicProvider::default()),
        Duration::from_millis(300),
    )
    .await;
}

fn build_jobs(url: &str) -> Vec<Job> {
    let mut conn = conn(url);
    let mut jobs = queue::list(&mut conn, kinds::EMBEDDED_BUILD).expect("jobs");
    jobs.extend(queue::list(&mut conn, kinds::HOSTED_BUILD).expect("jobs"));
    jobs
}

fn sweep_runs(url: &str) -> Vec<Job> {
    let key = kairos_server::periodic::key(kairos_server::periodic::CODE_INDEX_SWEEP);
    queue::list(&mut conn(url), kinds::PERIODIC)
        .expect("jobs")
        .into_iter()
        .filter(|job| job.dedupe_key.as_deref() == Some(key.as_str()))
        .collect()
}

async fn wait_until(limit: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !check() {
        assert!(start.elapsed() < limit, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}

/// Log in as the organization admin: the session bearer.
async fn login(router: &Router) -> String {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": EMAIL, "password": PASSWORD }).to_string(),
        ))
        .expect("request");
    let (status, body) = send(router, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["token"].as_str().expect("a token").to_string()
}

async fn get(router: &Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-tenant", TENANT)
        .body(Body::empty())
        .expect("request");
    send(router, request).await
}

/// The list of the runs of the repository, from the API.
async fn runs(router: &Router, token: &str) -> Value {
    let (status, body) = get(
        router,
        token,
        &format!("/api/repositories/{SLUG}/code-indexes/builds"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The API answers: the probe, and a read of the API.
async fn api_serves(router: &Router, token: &str) {
    let (status, body) = get(router, token, "/healthz").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = get(router, token, "/api/whoami").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ===========================================================================
// 1. An `api` server takes no build; a `worker` takes it
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_api_server_takes_no_build_and_a_worker_takes_it() {
    let work = tempfile::tempdir().unwrap();
    let (_db, url, repo) = world("kairos_server_roles_t0394_take", work.path());

    // Given a server with the role api, with what `serve` would start for
    // a builder
    let api = state(&url, ServerRole::Api).await;
    let api_service = service(work.path(), &work.path().join("api"), api.jobs.instance());
    start_builder(&api, Arc::clone(&api_service)).await;
    let api_jobs = tokio::spawn(Arc::clone(&api.jobs).run());
    let router = app::router(api.clone());
    let token = login(&router).await;

    // When the first build of the repository is added to the queue
    let notes = enqueue_first_builds(&api.blocking, &api_service, &api.jobs).await;
    assert!(notes.is_empty(), "{notes:?}");
    let key = job_key(TENANT, repo);
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Then the api server took no build job and added no sweep
    let jobs = build_jobs(&url);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].dedupe_key.as_deref(), Some(key.as_str()));
    assert_eq!(jobs[0].state, "queued", "{jobs:?}");
    assert_eq!(jobs[0].leased_by, None);
    assert_eq!(jobs[0].tries, 0);
    assert!(sweep_runs(&url).is_empty(), "{:?}", sweep_runs(&url));
    // And the list of the runs says that no builder runs
    let list = runs(&router, &token).await;
    let note = list["builder"].as_str().expect("a note of no builder");
    assert!(
        note.contains("KAIROS_ROLE=api") && note.contains("KAIROS_ROLE=worker"),
        "{note}"
    );

    // When a server with the role worker starts
    let worker = state(&url, ServerRole::Worker).await;
    let worker_service = service(
        work.path(),
        &work.path().join("worker"),
        worker.jobs.instance(),
    );
    start_builder(&worker, worker_service).await;
    let worker_jobs = tokio::spawn(Arc::clone(&worker.jobs).run());

    // Then the worker runs the build, and the sweep
    wait_until(Duration::from_secs(120), "the build ends", || {
        build_jobs(&url)
            .iter()
            .all(|job| job.state == "done" || job.state == "failed")
    })
    .await;
    let jobs = build_jobs(&url);
    assert!(
        jobs.iter().all(|job| job.state == "done"),
        "each build ends ok: {jobs:?}"
    );
    assert!(
        jobs.iter()
            .all(|job| job.leased_by == Some(worker.jobs.instance())),
        "the worker ran each build: {jobs:?}"
    );
    wait_until(
        Duration::from_secs(30),
        "a sweep of the worker ends",
        || sweep_runs(&url).iter().any(|job| job.state == "done"),
    )
    .await;
    let swept = sweep_runs(&url);
    assert!(
        swept
            .iter()
            .filter(|job| job.state == "done")
            .all(|job| job.leased_by == Some(worker.jobs.instance())),
        "only the worker runs the sweep: {swept:?}"
    );

    // And the API shows the run, and no note
    let list = runs(&router, &token).await;
    assert!(list.get("builder").is_none(), "{list}");
    assert_eq!(list["items"][0]["outcome"], "ok", "{list}");

    // And the worker serves the probes and no API
    let worker_router = app::worker_router(worker.clone());
    let (status, _) = get(&worker_router, &token, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&worker_router, &token, "/readyz").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get(&worker_router, &token, "/api/whoami").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("KAIROS_ROLE=worker"),
        "{body}"
    );
    api_jobs.abort();
    worker_jobs.abort();
}

/// A summarizer that makes `limit` summaries, then stops for ever: the
/// build hangs at that point, as a build that uses all the memory does.
#[derive(Clone)]
struct FrozenSummarizers {
    made: Arc<Mutex<usize>>,
    limit: usize,
}

struct FrozenSummarizer {
    inner: FakeSummarizer,
    made: Arc<Mutex<usize>>,
    limit: usize,
}

impl kairos_index::Summarizer for FrozenSummarizer {
    fn model(&self) -> String {
        self.inner.model()
    }

    fn summarize(&mut self, request: &kairos_index::SummaryRequest) -> Result<String, String> {
        let mut made = self.made.lock().unwrap();
        if *made == self.limit {
            drop(made);
            loop {
                std::thread::park();
            }
        }
        *made += 1;
        self.inner.summarize(request)
    }
}

impl SummarizerSource for FrozenSummarizers {
    fn run(
        &self,
        job: &mut dyn FnMut(
            &mut dyn kairos_index::Summarizer,
        )
            -> Result<kairos_index::UpdateReport, kairos_index::IndexError>,
    ) -> Result<kairos_index::UpdateReport, kairos_index::IndexError> {
        job(&mut FrozenSummarizer {
            inner: FakeSummarizer::default(),
            made: Arc::clone(&self.made),
            limit: self.limit,
        })
    }

    fn model_name(&self) -> String {
        FakeSummarizer::default().model
    }
}

/// A job worker with the role worker that runs the builds of `service` with
/// `summarizers`.
fn build_worker(
    url: &str,
    service: &Arc<CodeIndexService>,
    summarizers: Arc<dyn SummarizerSource>,
) -> Arc<Worker> {
    let blocking = BlockingTenantPool::new(url, 4);
    let worker = Arc::new(
        Worker::new(blocking.clone(), service.instance(), quick_jobs())
            .with_role(ServerRole::Worker),
    );
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(DeterministicProvider::default());
    BuildHandler::new(blocking, Arc::clone(service), Some(summarizers), embedder).register(&worker);
    worker
}

// ===========================================================================
// 2. A worker killed during a build leaves the API serving
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_killed_during_a_build_leaves_the_api_serving() {
    let work = tempfile::tempdir().unwrap();
    let (_db, url, repo) = world("kairos_server_roles_t0394_kill", work.path());

    // Given a server with the role api
    let api = state(&url, ServerRole::Api).await;
    let api_jobs = tokio::spawn(Arc::clone(&api.jobs).run());
    let router = app::router(api.clone());
    let token = login(&router).await;

    // And a worker, in a runtime of its own, whose build hangs after 2
    // summaries
    let frozen = FrozenSummarizers {
        made: Arc::new(Mutex::new(0)),
        limit: 2,
    };
    let dead = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dead_service = service(work.path(), &work.path().join("dead"), Uuid::new_v4());
    let dead_worker = build_worker(&url, &dead_service, Arc::new(frozen.clone()));
    {
        let (blocking, service, worker) = (
            BlockingTenantPool::new(&url, 4),
            Arc::clone(&dead_service),
            Arc::clone(&dead_worker),
        );
        dead.spawn(async move {
            enqueue_first_builds(&blocking, &service, &worker).await;
            worker.run().await;
        });
    }
    wait_until(Duration::from_secs(60), "2 summaries", || {
        *frozen.made.lock().unwrap() == 2
    })
    .await;
    let key = job_key(TENANT, repo);
    let leased = queue::open_by_key(&mut conn(&url), &key)
        .unwrap()
        .expect("the job of the worker");
    assert_eq!(leased.state, "leased");
    assert_eq!(leased.leased_by, Some(dead_service.instance()));
    let list = runs(&router, &token).await;
    assert_eq!(list["items"][0]["outcome"], "running", "{list}");

    // When the worker dies: no heartbeat, no end of its job
    dead.shutdown_background();

    // Then the API serves while the lease runs out
    for _ in 0..4 {
        api_serves(&router, &token).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // When the next worker starts
    let next_service = service(work.path(), &work.path().join("next"), Uuid::new_v4());
    let next = build_worker(&url, &next_service, Arc::new(FakeSummarizers));
    let next_jobs = tokio::spawn(Arc::clone(&next).run());

    // Then the job records the failure, and is not run again
    wait_until(Duration::from_secs(60), "the job fails", || {
        queue::get(&mut conn(&url), leased.id)
            .unwrap()
            .is_some_and(|job| job.state == "failed")
    })
    .await;
    let job = queue::get(&mut conn(&url), leased.id).unwrap().unwrap();
    assert_eq!(job.tries, 1, "{job:?}");
    let reason = job.last_error.clone().unwrap_or_default();
    assert!(
        reason.contains("The lease ran out on try 1 of 3")
            && reason.contains(&dead_service.instance().to_string()),
        "{reason}"
    );
    // And the API shows the run as failed, and still serves
    wait_until(Duration::from_secs(30), "the run ends", || {
        let mut conn = conn(&url);
        pin(&mut conn);
        kairos_db::code_index_builds::running(&mut conn, repo)
            .unwrap()
            .is_none()
    })
    .await;
    let list = runs(&router, &token).await;
    assert_eq!(list["items"][0]["outcome"], "failed", "{list}");
    api_serves(&router, &token).await;
    api_jobs.abort();
    next_jobs.abort();
}
