//! The worker loop of the job queue (KAIROS-T-0373, COLLIERY-I-0613).
//!
//! Each server runs one [`Worker`]. It takes jobs from `public.jobs`
//! ([`kairos_db::jobs`]) for the kinds that it has a [`JobHandler`] for,
//! and runs each job as a task of its own. The worker of each server reads
//! one table, so 2 or more servers share the work: `SKIP LOCKED` gives each
//! job to one server.
//!
//! # Order and limits
//!
//! The next job is the one with the highest priority, then the oldest
//! `run_after`. Each kind has a limit of jobs that one server runs at a
//! time ([`JobSettings::limits`]); the worker asks only for the kinds that
//! have a free slot.
//!
//! # Lease, progress and the watchdog
//!
//! A job that a server takes has a lease of [`JobSettings::lease`]. The job
//! tells the worker that it made progress with [`JobHandle::progress`]: a
//! store in memory, cheap enough for each summary. At each
//! [`JobSettings::heartbeat`], the watchdog of the job writes the heartbeat
//! and moves the lease forward, when the job made progress since the last
//! write. So the lease follows the progress of the job, not a timer.
//!
//! When the job made no progress for the lease time, the watchdog kills it
//! (the task is aborted, and [`JobHandle::cancelled`] becomes true for a
//! step that runs on a thread) and releases it: the job is queued again,
//! or it fails when it has had its tries. The watchdog kills the job one
//! heartbeat before the lease in the database runs out, so no other server
//! takes the job while it runs here.
//!
//! When a server stops, its leases run out, and any server takes those
//! jobs again. A job whose lease ran out on its last try is failed by the
//! next worker that looks ([`kairos_db::jobs::fail_expired`]); the
//! [`JobHandler::failed`] of its kind then runs.
//!
//! # A kind that is not run again after a stop
//!
//! A kind whose [`JobHandler::retry_stopped`] is false (the code index
//! builds, KAIROS-T-0399) is not run again when its server stopped or the
//! job stalled: a job that took its server down would take it down again
//! (GitHub #34). The next worker that looks fails a job of that kind whose
//! lease ran out, on any try, and no server takes it again
//! ([`kairos_db::jobs::fail_expired_with`]). A job that the watchdog kills
//! fails too. [`JobHandler::stopped`], then [`JobHandler::failed`], run.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use diesel::Connection;
use kairos_db::jobs::{self as db, Job, NewJob};

use crate::blocking::BlockingTenantPool;
use crate::error::ApiError;

/// The kinds of job that have a default limit (COLLIERY-I-0613).
pub mod kinds {
    /// A code-index build with the embedded summary model.
    pub const EMBEDDED_BUILD: &str = "embedded_build";
    /// A code-index build with a hosted summary provider.
    pub const HOSTED_BUILD: &str = "hosted_build";
    /// A batch of the embedding refresh.
    pub const EMBEDDING_BATCH: &str = "embedding_batch";
    /// A periodic pass (retention, the new-commits sweep).
    pub const PERIODIC: &str = "periodic";
    /// A signed webhook delivery (KAIROS-T-0384).
    pub const WEBHOOK_DELIVERY: &str = kairos_db::webhooks::JOB_KIND;
    /// A GitHub write-back of one pull request (KAIROS-T-0386).
    pub const GITHUB_WRITEBACK: &str = kairos_db::writeback::JOB_KIND;
    /// The import of the open issues of a repository (KAIROS-T-0487).
    pub const ISSUE_IMPORT: &str = crate::forge::import::KIND;
    /// The write-back of one issue that became a task (KAIROS-T-0575).
    pub const ISSUE_WRITEBACK: &str = kairos_db::issue_writeback::JOB_KIND;
}

/// The priorities of the jobs (COLLIERY-I-0613): a higher one runs first.
/// The order: a requested rebuild, an update, a first build, an embedding
/// batch, a webhook delivery, a periodic pass.
pub mod priority {
    /// A code-index build that a person asked for.
    pub const REQUESTED_BUILD: i32 = 50;
    /// A code-index update after a push.
    pub const UPDATE: i32 = 40;
    /// The first code-index build of a repository.
    pub const FIRST_BUILD: i32 = 30;
    /// A batch of the embedding refresh.
    pub const EMBEDDING_BATCH: i32 = 20;
    /// A signed webhook delivery (KAIROS-T-0384).
    pub const WEBHOOK_DELIVERY: i32 = kairos_db::webhooks::JOB_PRIORITY;
    /// A GitHub write-back (KAIROS-T-0386).
    pub const GITHUB_WRITEBACK: i32 = kairos_db::writeback::JOB_PRIORITY;
    /// The import of the open issues of a repository (KAIROS-T-0487).
    pub const ISSUE_IMPORT: i32 = crate::forge::import::PRIORITY;
    /// The write-back of an issue (KAIROS-T-0575).
    pub const ISSUE_WRITEBACK: i32 = kairos_db::issue_writeback::JOB_PRIORITY;
    /// A periodic pass.
    pub const PERIODIC: i32 = 10;
}

/// The limit of a kind that has no setting.
pub const DEFAULT_LIMIT: usize = 1;

/// The time after which the worker removes a finished job.
pub const KEEP_FINISHED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The settings of the job queue (`KAIROS_JOB_*`).
#[derive(Debug, Clone, PartialEq)]
pub struct JobSettings {
    /// `KAIROS_JOB_HEARTBEAT_SECS` (30): how often the watchdog of a job
    /// writes its progress.
    pub heartbeat: Duration,
    /// `KAIROS_JOB_LEASE_SECS` (300): a job with no progress for this time
    /// is killed and released.
    pub lease: Duration,
    /// `KAIROS_JOB_MAX_TRIES` (3): the leases of a job before it fails.
    pub max_tries: i32,
    /// `KAIROS_JOB_LIMIT_<KIND>`: the jobs of each kind that one server
    /// runs at a time.
    pub limits: BTreeMap<String, usize>,
    /// How often the worker looks for jobs when nothing wakes it.
    pub poll: Duration,
    /// The wait before a job that failed with a retry is taken again.
    pub retry_after: Duration,
}

impl Default for JobSettings {
    fn default() -> Self {
        JobSettings {
            heartbeat: Duration::from_secs(30),
            lease: Duration::from_secs(300),
            max_tries: 3,
            limits: [
                (kinds::EMBEDDED_BUILD, 1),
                (kinds::HOSTED_BUILD, 4),
                (kinds::EMBEDDING_BATCH, 2),
                (kinds::PERIODIC, 1),
                (kinds::WEBHOOK_DELIVERY, 4),
                (kinds::GITHUB_WRITEBACK, 2),
                (kinds::ISSUE_IMPORT, 2),
                (kinds::ISSUE_WRITEBACK, 2),
            ]
            .into_iter()
            .map(|(kind, limit)| (kind.to_string(), limit))
            .collect(),
            poll: Duration::from_secs(2),
            retry_after: Duration::from_secs(30),
        }
    }
}

impl JobSettings {
    /// The limit of `kind` on one server.
    pub fn limit(&self, kind: &str) -> usize {
        self.limits.get(kind).copied().unwrap_or(DEFAULT_LIMIT)
    }

    /// The time without progress after which the watchdog kills a job: one
    /// heartbeat before the lease runs out, so that the lease in the
    /// database is still held when the job stops.
    fn kill_after(&self) -> Duration {
        self.lease.saturating_sub(self.heartbeat)
    }
}

/// Why a job did not end well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    pub reason: String,
    /// True: the job is queued again while it has tries. False: it fails
    /// now.
    pub retry: bool,
    /// True: the job gave its slot to a job of a higher priority
    /// ([`JobHandle::should_yield`]). It is queued again, and no try is
    /// counted.
    pub yielded: bool,
    /// True: this server cannot run the job, and another server can
    /// ([`JobError::not_here`]). It is queued again after
    /// [`JobSettings::retry_after`], and no try is counted.
    pub deferred: bool,
    /// The wait before the next try of a [`JobError::retry`]; `None`:
    /// [`JobSettings::retry_after`]. A kind with growing waits sets it
    /// ([`JobError::retry_in`], KAIROS-T-0384).
    pub retry_after: Option<Duration>,
}

impl JobError {
    /// A failure that a later try can mend.
    pub fn retry(reason: impl Into<String>) -> Self {
        JobError {
            reason: reason.into(),
            retry: true,
            yielded: false,
            deferred: false,
            retry_after: None,
        }
    }

    /// A failure that a later try can mend, with the next try after `wait`.
    pub fn retry_in(reason: impl Into<String>, wait: Duration) -> Self {
        JobError {
            retry_after: Some(wait),
            ..JobError::retry(reason)
        }
    }

    /// A failure that a later try does not mend.
    pub fn fail(reason: impl Into<String>) -> Self {
        JobError {
            reason: reason.into(),
            retry: false,
            yielded: false,
            deferred: false,
            retry_after: None,
        }
    }

    /// The job stopped at a step boundary because
    /// [`JobHandle::should_yield`] said so.
    pub fn yielded() -> Self {
        JobError {
            reason: "The job gave its slot to a job of a higher priority.".into(),
            retry: true,
            yielded: true,
            deferred: false,
            retry_after: None,
        }
    }

    /// This server cannot run the job now (KAIROS-T-0376: a periodic pass
    /// that it does not run, or not yet). The job is queued again for any
    /// server, after [`JobSettings::retry_after`], and no try is counted.
    pub fn not_here(reason: impl Into<String>) -> Self {
        JobError {
            reason: reason.into(),
            retry: true,
            yielded: false,
            deferred: true,
            retry_after: None,
        }
    }
}

/// A boxed future of a job.
pub type JobFuture = Pin<Box<dyn Future<Output = Result<(), JobError>> + Send>>;

/// A boxed future with no result.
pub type UnitFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What runs the jobs of one kind.
pub trait JobHandler: Send + Sync {
    /// Run the job of `handle`. Call [`JobHandle::progress`] as the work
    /// goes on.
    fn run(&self, handle: JobHandle) -> JobFuture;

    /// The job ended `failed`, with the reason in `job.last_error`: after
    /// its last try, or with a failure that a try does not mend. It runs on
    /// the server that saw the failure, which is not always the server of
    /// the last try.
    fn failed(&self, job: Job) -> UnitFuture {
        let _ = job;
        Box::pin(async {})
    }

    /// False: a job of this kind whose lease ran out (its server stopped or
    /// stalled), or that the watchdog killed, is not run again: it fails,
    /// and [`JobHandler::stopped`] runs (KAIROS-T-0399). True (the
    /// default): it is queued again while it has tries.
    fn retry_stopped(&self) -> bool {
        true
    }

    /// The job ended `failed` because its lease ran out or the watchdog
    /// killed it, with the reason in `job.last_error`. It runs before
    /// [`JobHandler::failed`], on the server that saw the failure.
    fn stopped(&self, job: Job) -> UnitFuture {
        let _ = job;
        Box::pin(async {})
    }

    /// The job that follows `job` when `job` ends, done or failed: a
    /// periodic pass adds its next run (KAIROS-T-0376). The worker adds it
    /// in the transaction that writes the end, so the next job exists
    /// exactly when the end is written. `None`: no job follows.
    fn next(&self, job: &Job) -> Option<NewJob> {
        let _ = job;
        None
    }
}

/// The handle of a running job: its row, and its progress.
#[derive(Clone)]
pub struct JobHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    job: Job,
    started: Instant,
    /// Milliseconds after `started` of the last progress.
    last_progress: AtomicU64,
    /// The number of progress calls.
    count: AtomicU64,
    cancelled: AtomicBool,
    /// The job can give its slot to a job of a higher priority.
    yieldable: AtomicBool,
    /// A job of a higher priority waits for the slot of this job.
    yield_now: AtomicBool,
}

impl std::fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("job", &self.inner.job.id)
            .field("kind", &self.inner.job.kind)
            .finish()
    }
}

impl JobHandle {
    fn new(job: Job) -> Self {
        JobHandle {
            inner: Arc::new(HandleInner {
                job,
                started: Instant::now(),
                last_progress: AtomicU64::new(0),
                count: AtomicU64::new(0),
                cancelled: AtomicBool::new(false),
                yieldable: AtomicBool::new(false),
                yield_now: AtomicBool::new(false),
            }),
        }
    }

    /// The row of the job, as it was leased.
    pub fn job(&self) -> &Job {
        &self.inner.job
    }

    /// The job made progress: the watchdog moves the lease forward at the
    /// next heartbeat. Cheap: no database write. False when the watchdog
    /// killed the job: the caller stops.
    pub fn progress(&self) -> bool {
        let ms = self.inner.started.elapsed().as_millis() as u64;
        self.inner.last_progress.store(ms, Ordering::Relaxed);
        self.inner.count.fetch_add(1, Ordering::Relaxed);
        !self.cancelled()
    }

    /// True when the watchdog killed the job, or another server took it. A
    /// step that runs on a thread checks this and stops.
    pub fn cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Relaxed)
    }

    /// The job can stop at a step boundary and give its slot to a job of
    /// the same kind with a higher priority (KAIROS-T-0374: a first build
    /// of hours lets an update go first). The watchdog then sets
    /// [`Self::should_yield`] when such a job waits and this server has no
    /// free slot of the kind.
    pub fn allow_yield(&self) {
        self.inner.yieldable.store(true, Ordering::Relaxed);
    }

    /// True when a job of a higher priority waits for the slot of this job.
    /// The job stops at its next step boundary, keeps what it made, and
    /// ends with [`JobError::yielded`].
    pub fn should_yield(&self) -> bool {
        self.inner.yield_now.load(Ordering::Relaxed)
    }

    fn yieldable(&self) -> bool {
        self.inner.yieldable.load(Ordering::Relaxed)
    }

    fn since_progress(&self) -> Duration {
        let now = self.inner.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.inner.last_progress.load(Ordering::Relaxed)))
    }

    fn count(&self) -> u64 {
        self.inner.count.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Relaxed);
    }
}

/// The worker loop of one server.
pub struct Worker {
    blocking: BlockingTenantPool,
    instance: uuid::Uuid,
    settings: JobSettings,
    handlers: RwLock<HashMap<String, Arc<dyn JobHandler>>>,
    /// The running jobs of each kind on this server.
    running: Mutex<HashMap<String, usize>>,
    wake: tokio::sync::Notify,
    last_purge: Mutex<Option<Instant>>,
    /// The periodic passes of this server (KAIROS-T-0376).
    pub(crate) periodic: Arc<crate::periodic::Passes>,
    /// The role of this server (KAIROS-T-0394): the kinds of job that it
    /// takes, and the periodic passes that it runs.
    role: crate::config::ServerRole,
    /// The metrics of the server (KAIROS-T-0576): the failures and the
    /// kills of the watchdog.
    metrics: Arc<crate::metrics::Metrics>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("instance", &self.instance)
            .field("settings", &self.settings)
            .field("role", &self.role)
            .finish()
    }
}

/// How a job ended on this server, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Done,
    Requeued,
    /// Queued again with no try counted: it gave its slot.
    Yielded,
    /// Queued again for another server, with no try counted.
    Deferred,
    Failed,
    /// Another server holds the lease now.
    Lost,
}

impl Worker {
    /// A worker of the server `instance` (the id of the start of the
    /// server, KAIROS-T-0354), with no handler.
    pub fn new(blocking: BlockingTenantPool, instance: uuid::Uuid, settings: JobSettings) -> Self {
        Worker {
            blocking,
            instance,
            settings,
            handlers: RwLock::default(),
            running: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            last_purge: Mutex::default(),
            periodic: Arc::default(),
            role: crate::config::ServerRole::All,
            metrics: Arc::new(crate::metrics::Metrics::new()),
        }
    }

    /// The worker that counts its failures and kills in `metrics`
    /// (KAIROS-T-0576): the registry of the state of the server. The
    /// default is a registry of its own.
    pub fn with_metrics(mut self, metrics: Arc<crate::metrics::Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// The metrics registry of the worker.
    pub fn metrics(&self) -> &Arc<crate::metrics::Metrics> {
        &self.metrics
    }

    /// The worker of a server with the role `role` (KAIROS-T-0394). It
    /// registers only the kinds of job that the role takes
    /// ([`crate::config::ServerRole::takes`]), and adds only the periodic
    /// passes that the role runs. The default is `all`.
    pub fn with_role(mut self, role: crate::config::ServerRole) -> Self {
        self.role = role;
        self
    }

    /// The role of the server.
    pub fn role(&self) -> crate::config::ServerRole {
        self.role
    }

    /// The id of the server that leases the jobs.
    pub fn instance(&self) -> uuid::Uuid {
        self.instance
    }

    /// The settings of the worker.
    pub fn settings(&self) -> &JobSettings {
        &self.settings
    }

    /// Run the jobs of `kind` with `handler`. A second handler of a kind
    /// replaces the first. A kind that the role of the server does not take
    /// is not registered (KAIROS-T-0394): a server with the role `api` takes
    /// no code index build, and a `worker` takes only the builds.
    pub fn register(&self, kind: &str, handler: Arc<dyn JobHandler>) {
        if !self.role.takes(kind) {
            tracing::info!(
                kind,
                role = self.role.as_str(),
                "job worker: this role does not take this kind of job"
            );
            return;
        }
        self.handlers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(kind.to_string(), handler);
        self.wake.notify_one();
    }

    /// Add a job, and wake the worker. `None`: a queued or leased job has
    /// the same dedupe key, and nothing was added.
    pub async fn enqueue(&self, job: NewJob) -> Result<Option<Job>, ApiError> {
        let added = self
            .blocking
            .run_public(move |conn| db::enqueue(conn, &job).map_err(ApiError::internal))
            .await?;
        if added.is_some() {
            self.wake.notify_one();
        }
        Ok(added)
    }

    /// Let go of the worker: wait (at most `limit`) until no task of it
    /// holds it any more, then drop it, and with it its pool, so that its
    /// database connections close now. For tests and a one-shot command,
    /// after [`Worker::drain`]: a process that exits while the tasks of a
    /// worker still hold connections can crash at exit (KAIROS-T-0449). A
    /// worker whose loop ([`Worker::run`]) still runs is dropped here only
    /// as one reference.
    pub async fn shutdown(self: Arc<Self>, limit: Duration) {
        let until = Instant::now() + limit;
        while Arc::strong_count(&self) > 1 && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(self);
    }

    /// Wake the worker: it looks for jobs now.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// The number of jobs that run on this server now.
    pub fn running(&self) -> usize {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .sum()
    }

    fn handler(&self, kind: &str) -> Option<Arc<dyn JobHandler>> {
        self.handlers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(kind)
            .cloned()
    }

    /// The kinds of this worker that are not run again after a stop
    /// ([`JobHandler::retry_stopped`]).
    fn no_retake(&self) -> Vec<String> {
        let handlers = self.handlers.read().unwrap_or_else(|e| e.into_inner());
        let mut kinds: Vec<String> = handlers
            .iter()
            .filter(|(_, handler)| !handler.retry_stopped())
            .map(|(kind, _)| kind.clone())
            .collect();
        kinds.sort();
        kinds
    }

    fn kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self
            .handlers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        kinds.sort();
        kinds
    }

    /// The kinds with a free slot on this server.
    fn free_kinds(&self) -> Vec<String> {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        self.kinds()
            .into_iter()
            .filter(|kind| running.get(kind).copied().unwrap_or(0) < self.settings.limit(kind))
            .collect()
    }

    /// Run the worker for ever.
    pub async fn run(self: Arc<Self>) {
        tracing::info!(
            instance = %self.instance,
            heartbeat_secs = self.settings.heartbeat.as_secs_f64(),
            lease_secs = self.settings.lease.as_secs_f64(),
            max_tries = self.settings.max_tries,
            "job worker started"
        );
        loop {
            self.pass().await;
            let _ = tokio::time::timeout(self.settings.poll, self.wake.notified()).await;
        }
    }

    /// One pass: fail the jobs whose lease ran out on their last try, then
    /// take jobs while a kind has a free slot. The number of jobs started.
    pub async fn pass(self: &Arc<Self>) -> usize {
        self.fail_expired().await;
        self.purge().await;
        let mut started = 0;
        loop {
            let kinds = self.free_kinds();
            if kinds.is_empty() {
                break;
            }
            let instance = self.instance;
            let lease = self.settings.lease.as_secs_f64();
            let no_retake = self.no_retake();
            // KAIROS-T-0466: the runs of the periodic passes of this server
            // only, so no run is leased to be given back.
            let keyed = db::Keyed {
                kinds: vec![crate::jobs::kinds::PERIODIC.to_string()],
                keys: self.periodic.keys(),
            };
            let taken = self
                .blocking
                .run_public(move |conn| {
                    db::take_with(conn, instance, &kinds, &no_retake, &keyed, lease)
                        .map_err(ApiError::internal)
                })
                .await;
            match taken {
                Ok(Some(job)) => {
                    self.start(job);
                    started += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = ?e, "job worker could not take a job");
                    break;
                }
            }
        }
        started
    }

    /// True when this server has a free slot of `kind`.
    fn has_free_slot(&self, kind: &str) -> bool {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        running.get(kind).copied().unwrap_or(0) < self.settings.limit(kind)
    }

    /// Run passes until no job is queued for this worker and none runs
    /// here. For tests and for a one-shot command.
    ///
    /// The drain stops only after a pass that began with no job running
    /// and started none (KAIROS-T-0378). A pass that began while a job ran
    /// did not ask for the kind of that job, and the job can end during the
    /// pass: a job of that kind that waits for the slot is then not taken,
    /// and the drain must look again.
    pub async fn drain(self: &Arc<Self>) {
        loop {
            let busy = self.running();
            let started = self.pass().await;
            if busy == 0 && started == 0 && self.running() == 0 {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), self.wake.notified()).await;
        }
    }

    async fn fail_expired(&self) {
        let kinds = self.kinds();
        if kinds.is_empty() {
            return;
        }
        let handlers: HashMap<String, Arc<dyn JobHandler>> = kinds
            .iter()
            .filter_map(|kind| self.handler(kind).map(|h| (kind.clone(), h)))
            .collect();
        let no_retake = self.no_retake();
        // The job that follows each failed job is added in the transaction
        // of the failure (KAIROS-T-0376).
        let failed = self
            .blocking
            .run_public(move |conn| {
                conn.transaction::<_, diesel::result::Error, _>(|conn| {
                    let jobs = db::fail_expired_with(conn, &kinds, &no_retake)?;
                    for job in &jobs {
                        if let Some(next) = handlers.get(&job.kind).and_then(|h| h.next(job)) {
                            db::enqueue(conn, &next)?;
                        }
                    }
                    Ok(jobs)
                })
                .map_err(ApiError::internal)
            })
            .await;
        match failed {
            Ok(jobs) => {
                for job in jobs {
                    self.metrics.record_job_failure(&job.kind);
                    tracing::warn!(
                        job = %job.id,
                        kind = %job.kind,
                        tenant = ?job.tenant,
                        reason = ?job.last_error,
                        "job failed: its lease ran out"
                    );
                    if let Some(handler) = self.handler(&job.kind) {
                        handler.stopped(job.clone()).await;
                        handler.failed(job).await;
                    }
                }
            }
            Err(e) => tracing::warn!(error = ?e, "job worker could not fail the expired jobs"),
        }
    }

    /// Remove the jobs that ended more than [`KEEP_FINISHED`] ago, once an
    /// hour.
    async fn purge(&self) {
        {
            let mut last = self.last_purge.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(3600)) {
                return;
            }
            *last = Some(Instant::now());
        }
        let before = chrono::Utc::now()
            - chrono::Duration::from_std(KEEP_FINISHED).unwrap_or(chrono::Duration::days(7));
        if let Err(e) = self
            .blocking
            .run_public(move |conn| db::purge_finished(conn, before).map_err(ApiError::internal))
            .await
        {
            tracing::warn!(error = ?e, "job worker could not remove the finished jobs");
        }
    }

    /// Run `job` in a task of its own, with its watchdog.
    fn start(self: &Arc<Self>, job: Job) {
        let Some(handler) = self.handler(&job.kind) else {
            return;
        };
        *self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(job.kind.clone())
            .or_default() += 1;
        tracing::info!(
            job = %job.id,
            kind = %job.kind,
            tenant = ?job.tenant,
            try_ = job.tries,
            max_tries = job.max_tries,
            "job started"
        );
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let kind = job.kind.clone();
            let id = job.id;
            let ended = worker.supervise(job, handler).await;
            tracing::info!(job = %id, kind = %kind, ended = ?ended, "job ended");
            if let Some(n) = worker
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&kind)
            {
                *n = n.saturating_sub(1);
            }
            worker.wake.notify_one();
        });
    }

    /// Run the job of `handler`, write its heartbeat while it makes
    /// progress, kill it when it stalls, and write its end.
    async fn supervise(&self, job: Job, handler: Arc<dyn JobHandler>) -> Ended {
        let handle = JobHandle::new(job.clone());
        let mut task = tokio::spawn(handler.run(handle.clone()));
        // The watchdog looks at each heartbeat, and at each poll for a job
        // that can give its slot.
        let mut ticker = tokio::time::interval(self.settings.heartbeat.min(self.settings.poll));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        let mut written = handle.count();
        // The last write of the lease: at the take, then at each heartbeat.
        let mut lease_written = Instant::now();
        loop {
            tokio::select! {
                result = &mut task => {
                    let result = match result {
                        Ok(result) => result,
                        Err(e) if e.is_panic() => Err(JobError::retry(format!("The job stopped with a panic: {e}"))),
                        Err(e) => Err(JobError::retry(format!("The job stopped: {e}"))),
                    };
                    return self.finish(&job, &handler, result, false).await;
                }
                _ = ticker.tick() => {
                    if handle.since_progress() >= self.settings.kill_after() {
                        handle.cancel();
                        task.abort();
                        let reason = format!(
                            "The job made no progress for {} s, and the server {} killed it.",
                            handle.since_progress().as_secs(),
                            self.instance
                        );
                        tracing::warn!(job = %job.id, kind = %job.kind, %reason, "job stalled");
                        self.metrics.record_job_lease_kill(&job.kind);
                        // KAIROS-T-0399: a kind that is not run again after
                        // a stop fails now.
                        let error = if handler.retry_stopped() {
                            JobError::retry(reason)
                        } else {
                            JobError::fail(reason)
                        };
                        return self.finish(&job, &handler, Err(error), true).await;
                    }
                    // The database did not take a heartbeat for the lease
                    // time: the lease is about to run out, and another server
                    // can take the job. It stops here.
                    if lease_written.elapsed() >= self.settings.kill_after() {
                        handle.cancel();
                        task.abort();
                        tracing::warn!(job = %job.id, kind = %job.kind, "job heartbeat not written for the lease time; stopped here");
                        return Ended::Lost;
                    }
                    if handle.yieldable() && !handle.should_yield() && !self.has_free_slot(&job.kind) {
                        let (kind, priority) = (job.kind.clone(), job.priority);
                        if let Ok(true) = self
                            .blocking
                            .run_public(move |conn| db::higher_waits(conn, &kind, priority).map_err(ApiError::internal))
                            .await
                        {
                            tracing::info!(job = %job.id, kind = %job.kind, "a job of a higher priority waits: this job gives its slot at its next step");
                            handle.inner.yield_now.store(true, Ordering::Relaxed);
                        }
                    }
                    let count = handle.count();
                    if count == written || lease_written.elapsed() < self.settings.heartbeat {
                        continue;
                    }
                    written = count;
                    let (id, instance, lease) = (job.id, self.instance, self.settings.lease.as_secs_f64());
                    match self
                        .blocking
                        .run_public(move |conn| db::heartbeat(conn, id, instance, lease).map_err(ApiError::internal))
                        .await
                    {
                        Ok(true) => lease_written = Instant::now(),
                        Ok(false) => {
                            handle.cancel();
                            task.abort();
                            tracing::warn!(job = %job.id, kind = %job.kind, "job lost its lease to another server; stopped here");
                            return Ended::Lost;
                        }
                        Err(e) => tracing::warn!(job = %job.id, error = ?e, "job heartbeat not written"),
                    }
                }
            }
        }
    }

    /// Write the end of `job`, and add the job that follows it
    /// ([`JobHandler::next`]) in the same transaction when the job ended.
    /// `stopped`: the watchdog killed the job; when it fails,
    /// [`JobHandler::stopped`] runs before [`JobHandler::failed`].
    async fn finish(
        &self,
        job: &Job,
        handler: &Arc<dyn JobHandler>,
        result: Result<(), JobError>,
        stopped: bool,
    ) -> Ended {
        let (id, instance) = (job.id, self.instance);
        let deferred_after = self.settings.retry_after.as_secs_f64();
        let retry_after = match &result {
            Err(JobError {
                retry_after: Some(wait),
                ..
            }) => wait.as_secs_f64(),
            _ => deferred_after,
        };
        let next = handler.next(job);
        if let Err(JobError { reason, .. }) = &result {
            tracing::warn!(job = %job.id, kind = %job.kind, %reason, "job try ended with an error");
        }
        let written = self
            .blocking
            .run_public(move |conn| {
                conn.transaction::<_, diesel::result::Error, _>(|conn| {
                    let ended = match result {
                        Ok(()) => {
                            if db::complete(conn, id, instance)? {
                                Ended::Done
                            } else {
                                Ended::Lost
                            }
                        }
                        Err(JobError { yielded: true, .. }) => {
                            if db::requeue(conn, id, instance)? {
                                Ended::Yielded
                            } else {
                                Ended::Lost
                            }
                        }
                        Err(JobError {
                            reason,
                            deferred: true,
                            ..
                        }) => {
                            if db::defer(conn, id, instance, &reason, deferred_after)? {
                                Ended::Deferred
                            } else {
                                Ended::Lost
                            }
                        }
                        Err(JobError {
                            reason,
                            retry: true,
                            ..
                        }) => match db::release(conn, id, instance, &reason, retry_after)? {
                            Some(row) if row.state == "queued" => Ended::Requeued,
                            // A waiting job of its key does the work
                            // (KAIROS-T-0637).
                            Some(row) if row.state == "done" => Ended::Done,
                            Some(_) => Ended::Failed,
                            None => Ended::Lost,
                        },
                        Err(JobError {
                            reason,
                            retry: false,
                            ..
                        }) => {
                            if db::fail(conn, id, instance, &reason)? {
                                Ended::Failed
                            } else {
                                Ended::Lost
                            }
                        }
                    };
                    if let (Ended::Done | Ended::Failed, Some(next)) = (ended, &next) {
                        db::enqueue(conn, next)?;
                    }
                    Ok(ended)
                })
                .map_err(ApiError::internal)
            })
            .await;
        let ended = match written {
            Ok(ended) => ended,
            Err(e) => {
                // The lease runs out, and a server takes the job again.
                tracing::warn!(job = %job.id, error = ?e, "job end not written");
                return Ended::Lost;
            }
        };
        if ended == Ended::Failed {
            self.metrics.record_job_failure(&job.kind);
            let row = self
                .blocking
                .run_public(move |conn| db::get(conn, id).map_err(ApiError::internal))
                .await;
            if let Ok(Some(row)) = row {
                if stopped {
                    handler.stopped(row.clone()).await;
                }
                handler.failed(row).await;
            }
        }
        ended
    }
}
