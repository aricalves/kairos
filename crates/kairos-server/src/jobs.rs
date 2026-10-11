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

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

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
}

impl JobError {
    /// A failure that a later try can mend.
    pub fn retry(reason: impl Into<String>) -> Self {
        JobError {
            reason: reason.into(),
            retry: true,
        }
    }

    /// A failure that a later try does not mend.
    pub fn fail(reason: impl Into<String>) -> Self {
        JobError {
            reason: reason.into(),
            retry: false,
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
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("instance", &self.instance)
            .field("settings", &self.settings)
            .finish()
    }
}

/// How a job ended on this server, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Done,
    Requeued,
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
        }
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
    /// replaces the first.
    pub fn register(&self, kind: &str, handler: Arc<dyn JobHandler>) {
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
            let taken = self
                .blocking
                .run_public(move |conn| {
                    db::take(conn, instance, &kinds, lease).map_err(ApiError::internal)
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

    /// Run passes until no job is queued for this worker and none runs
    /// here. For tests and for a one-shot command.
    pub async fn drain(self: &Arc<Self>) {
        loop {
            let started = self.pass().await;
            if started == 0 && self.running() == 0 {
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
        let failed = self
            .blocking
            .run_public(move |conn| db::fail_expired(conn, &kinds).map_err(ApiError::internal))
            .await;
        match failed {
            Ok(jobs) => {
                for job in jobs {
                    tracing::warn!(
                        job = %job.id,
                        kind = %job.kind,
                        tenant = ?job.tenant,
                        reason = ?job.last_error,
                        "job failed: its lease ran out on its last try"
                    );
                    if let Some(handler) = self.handler(&job.kind) {
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
        let mut ticker = tokio::time::interval(self.settings.heartbeat);
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
                    return self.finish(&job, &handler, result).await;
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
                        return self.finish(&job, &handler, Err(JobError::retry(reason))).await;
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
                    let count = handle.count();
                    if count == written {
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

    /// Write the end of `job`.
    async fn finish(
        &self,
        job: &Job,
        handler: &Arc<dyn JobHandler>,
        result: Result<(), JobError>,
    ) -> Ended {
        let (id, instance) = (job.id, self.instance);
        let retry_after = self.settings.retry_after.as_secs_f64();
        let written = match result {
            Ok(()) => self
                .blocking
                .run_public(move |conn| {
                    db::complete(conn, id, instance).map_err(ApiError::internal)
                })
                .await
                .map(|held| if held { Ended::Done } else { Ended::Lost }),
            Err(JobError {
                reason,
                retry: true,
            }) => {
                tracing::warn!(job = %job.id, kind = %job.kind, %reason, "job try failed");
                self.blocking
                    .run_public(move |conn| {
                        db::release(conn, id, instance, &reason, retry_after)
                            .map_err(ApiError::internal)
                    })
                    .await
                    .map(|row| match row {
                        Some(row) if row.state == "queued" => Ended::Requeued,
                        Some(_) => Ended::Failed,
                        None => Ended::Lost,
                    })
            }
            Err(JobError {
                reason,
                retry: false,
            }) => {
                tracing::warn!(job = %job.id, kind = %job.kind, %reason, "job failed");
                self.blocking
                    .run_public(move |conn| {
                        db::fail(conn, id, instance, &reason).map_err(ApiError::internal)
                    })
                    .await
                    .map(|held| if held { Ended::Failed } else { Ended::Lost })
            }
        };
        let ended = match written {
            Ok(ended) => ended,
            Err(e) => {
                // The lease runs out, and a server takes the job again.
                tracing::warn!(job = %job.id, error = ?e, "job end not written");
                return Ended::Lost;
            }
        };
        if ended == Ended::Failed {
            let row = self
                .blocking
                .run_public(move |conn| db::get(conn, id).map_err(ApiError::internal))
                .await;
            if let Ok(Some(row)) = row {
                handler.failed(row).await;
            }
        }
        ended
    }
}
