//! The job queue and the worker loop (KAIROS-T-0373, COLLIERY-I-0613).
//!
//! 1. Two worker loops on one database, with many jobs, never run one job
//!    twice, and run each job.
//! 2. Order: priority, then age.
//! 3. The limits per kind of one server.
//! 4. A job that makes no progress is killed and re-leased; after its tries
//!    it fails with the reason, and the failed hook of its kind runs.
//! 5. Progress keeps the lease of a long job.
//! 6. The lease of a dead worker runs out, and another worker takes the job.
//! 7. A dedupe key keeps a second job out while the first is open.
//! 8. A job of the last try whose lease ran out is failed by another worker.
//! 10. A kind that is not run again after a stop (KAIROS-T-0399): a stalled
//!     job and a job whose lease ran out fail on their first try, and the
//!     stopped hook runs; a job of another kind is taken again.
//!
//! Runs against the compose Postgres (`angreal services up`). Owns the
//! scratch databases `kairos_jobs_t0373_*`.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diesel::Connection;
use diesel::pg::PgConnection;
use kairos_db::jobs::{self as db, Job, NewJob};
use kairos_db::run_public_migrations;
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::jobs::{
    JobError, JobFuture, JobHandle, JobHandler, JobSettings, UnitFuture, Worker,
};
use uuid::Uuid;

const KIND: &str = "test";
const OTHER: &str = "test_other";

/// A scratch database with the public schema, and its URL.
fn scratch(name: &str) -> common::ScratchUrl {
    let admin = common::recreate_scratch_db(name);
    let url = common::with_database(&common::admin_database_url(), name);
    let mut conn = PgConnection::establish(&url).expect("connect to the scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    common::ScratchUrl::new(admin, url)
}

fn conn(url: &str) -> PgConnection {
    PgConnection::establish(url).expect("connect")
}

/// Short times, so that a stall shows in a second.
fn settings(limit: usize) -> JobSettings {
    let mut settings = JobSettings {
        heartbeat: Duration::from_millis(100),
        lease: Duration::from_millis(600),
        max_tries: 3,
        poll: Duration::from_millis(50),
        retry_after: Duration::ZERO,
        ..JobSettings::default()
    };
    settings.limits.insert(KIND.to_string(), limit);
    settings.limits.insert(OTHER.to_string(), 1);
    settings
}

/// The settings of the tests of a stalled job (KAIROS-T-0449). The watchdog
/// kills a job with no progress one heartbeat before its lease runs out.
/// With the heartbeat of 100 ms of [`settings`], a late watchdog on a
/// loaded machine let a pass of the drain fail the job for its lease
/// first. A long heartbeat keeps the kill at 500 ms and the end of the
/// lease 4 s after it, so the watchdog ends each try. A stalled job never
/// progresses, so no heartbeat is written.
fn stall_settings() -> JobSettings {
    JobSettings {
        heartbeat: Duration::from_secs(4),
        lease: Duration::from_millis(4500),
        ..settings(1)
    }
}

fn worker(url: &str, settings: JobSettings) -> Arc<Worker> {
    Arc::new(Worker::new(
        BlockingTenantPool::new(url, 8),
        Uuid::new_v4(),
        settings,
    ))
}

/// The record of the runs of a test handler.
#[derive(Default)]
struct Record {
    /// (job, worker) for each run, in the order of the starts.
    runs: Mutex<Vec<(Uuid, Uuid)>>,
    /// The order of the starts, by the `n` of the payload.
    order: Mutex<Vec<i64>>,
    running: AtomicUsize,
    most: AtomicUsize,
    failed: Mutex<Vec<Job>>,
    /// The jobs of the stopped hook (KAIROS-T-0399).
    stopped: Mutex<Vec<Job>>,
}

/// What a test handler does.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Progress, sleep this long, and end well.
    Sleep(Duration),
    /// Never progress, and never end.
    Stall,
    /// Progress every 50 ms for this long, and end well.
    Busy(Duration),
}

struct TestHandler {
    worker: Uuid,
    record: Arc<Record>,
    behaviour: Behaviour,
    /// [`JobHandler::retry_stopped`] (KAIROS-T-0399).
    retry_stopped: bool,
}

/// Counts down `running` when the job ends, also when it is aborted.
struct Running(Arc<Record>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

impl JobHandler for TestHandler {
    fn run(&self, handle: JobHandle) -> JobFuture {
        let record = Arc::clone(&self.record);
        let worker = self.worker;
        let behaviour = self.behaviour;
        Box::pin(async move {
            let job = handle.job().clone();
            record.runs.lock().unwrap().push((job.id, worker));
            if let Some(n) = job.payload_json()["n"].as_i64() {
                record.order.lock().unwrap().push(n);
            }
            let now = record.running.fetch_add(1, Ordering::SeqCst) + 1;
            record.most.fetch_max(now, Ordering::SeqCst);
            let _running = Running(Arc::clone(&record));
            match behaviour {
                Behaviour::Sleep(time) => {
                    handle.progress();
                    tokio::time::sleep(time).await;
                }
                Behaviour::Stall => {
                    std::future::pending::<()>().await;
                }
                Behaviour::Busy(time) => {
                    let until = tokio::time::Instant::now() + time;
                    while tokio::time::Instant::now() < until {
                        if !handle.progress() {
                            return Err(JobError::fail("killed"));
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            Ok(())
        })
    }

    fn failed(&self, job: Job) -> UnitFuture {
        self.record.failed.lock().unwrap().push(job);
        Box::pin(async {})
    }

    fn retry_stopped(&self) -> bool {
        self.retry_stopped
    }

    fn stopped(&self, job: Job) -> UnitFuture {
        self.record.stopped.lock().unwrap().push(job);
        Box::pin(async {})
    }
}

fn register(worker: &Arc<Worker>, kind: &str, record: &Arc<Record>, behaviour: Behaviour) {
    register_with(worker, kind, record, behaviour, true);
}

fn register_with(
    worker: &Arc<Worker>,
    kind: &str,
    record: &Arc<Record>,
    behaviour: Behaviour,
    retry_stopped: bool,
) {
    worker.register(
        kind,
        Arc::new(TestHandler {
            worker: worker.instance(),
            record: Arc::clone(record),
            behaviour,
            retry_stopped,
        }),
    );
}

fn job(kind: &str, n: i64) -> NewJob {
    NewJob {
        payload: serde_json::json!({ "n": n }),
        ..NewJob::new(kind)
    }
}

fn jobs_of(url: &str, kind: &str) -> Vec<Job> {
    db::list(&mut conn(url), kind).expect("list the jobs")
}

/// Let go of the workers before the test returns, so that their pools
/// close their connections now (KAIROS-T-0449): a test process that
/// exited with them open died with SIGSEGV under nextest.
async fn shut_down<const N: usize>(workers: [Arc<Worker>; N]) {
    for w in workers {
        w.shutdown(Duration::from_secs(5)).await;
    }
}

/// Wait until `check` is true, or fail after `limit`.
async fn wait_until(limit: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let until = tokio::time::Instant::now() + limit;
    while !check() {
        assert!(tokio::time::Instant::now() < until, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ===========================================================================
// 1. Two worker loops on one database never run one job twice
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_run_each_job_once() {
    let url = scratch("kairos_jobs_t0373_twice");
    let record = Arc::new(Record::default());
    let a = worker(&url, settings(4));
    let b = worker(&url, settings(4));
    register(
        &a,
        KIND,
        &record,
        Behaviour::Sleep(Duration::from_millis(5)),
    );
    register(
        &b,
        KIND,
        &record,
        Behaviour::Sleep(Duration::from_millis(5)),
    );
    for n in 0..200 {
        a.enqueue(job(KIND, n)).await.unwrap().expect("added");
    }

    let loops = [
        tokio::spawn(Arc::clone(&a).run()),
        tokio::spawn(Arc::clone(&b).run()),
    ];
    wait_until(Duration::from_secs(60), "every job done", || {
        jobs_of(&url, KIND).iter().all(|j| j.state == "done")
    })
    .await;
    for l in loops {
        l.abort();
        let _ = l.await;
    }

    let runs = record.runs.lock().unwrap().clone();
    let mut per_job: HashMap<Uuid, usize> = HashMap::new();
    for (id, _) in &runs {
        *per_job.entry(*id).or_default() += 1;
    }
    assert_eq!(per_job.len(), 200, "each job ran");
    assert!(
        per_job.values().all(|n| *n == 1),
        "no job ran twice: {:?}",
        per_job.values().filter(|n| **n > 1).count()
    );
    let by_a = runs.iter().filter(|(_, w)| *w == a.instance()).count();
    let by_b = runs.iter().filter(|(_, w)| *w == b.instance()).count();
    assert!(
        by_a > 0 && by_b > 0,
        "both workers took jobs: {by_a} and {by_b}"
    );
    assert!(jobs_of(&url, KIND).iter().all(|j| j.tries == 1));
    shut_down([a, b]).await;
}

// ===========================================================================
// 2. Order: priority, then age
// ===========================================================================
#[tokio::test]
async fn jobs_run_by_priority_then_age() {
    let url = scratch("kairos_jobs_t0373_order");
    let record = Arc::new(Record::default());
    let w = worker(&url, settings(1));
    register(&w, KIND, &record, Behaviour::Sleep(Duration::ZERO));
    let now = chrono::Utc::now();
    let at = |secs: i64| Some(now - chrono::Duration::seconds(secs));
    // (n, priority, run_after): the expected order is 1, 2, 3, 4, 5.
    for (n, priority, run_after) in [
        (5, 0, at(10)),
        (3, 10, at(5)),
        (1, 20, at(1)),
        (4, 10, at(1)),
        (2, 10, at(50)),
    ] {
        w.enqueue(NewJob {
            priority,
            run_after,
            ..job(KIND, n)
        })
        .await
        .unwrap();
    }
    // Not before its time.
    w.enqueue(NewJob {
        priority: 100,
        run_after: Some(now + chrono::Duration::hours(1)),
        ..job(KIND, 99)
    })
    .await
    .unwrap();

    w.drain().await;
    assert_eq!(*record.order.lock().unwrap(), vec![1, 2, 3, 4, 5]);
    let later = jobs_of(&url, KIND)
        .into_iter()
        .find(|j| j.payload_json()["n"] == 99)
        .unwrap();
    assert_eq!(later.state, "queued", "a job is not taken before run_after");
    shut_down([w]).await;
}

// ===========================================================================
// 3. The limits per kind
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_runs_at_most_the_limit_of_each_kind() {
    let url = scratch("kairos_jobs_t0373_limits");
    let record = Arc::new(Record::default());
    let other = Arc::new(Record::default());
    let w = worker(&url, settings(2));
    register(
        &w,
        KIND,
        &record,
        Behaviour::Sleep(Duration::from_millis(150)),
    );
    register(
        &w,
        OTHER,
        &other,
        Behaviour::Sleep(Duration::from_millis(150)),
    );
    for n in 0..8 {
        w.enqueue(job(KIND, n)).await.unwrap();
    }
    for n in 0..3 {
        w.enqueue(job(OTHER, n)).await.unwrap();
    }

    // The first pass fills the free slots of each kind, and no more.
    assert_eq!(
        w.pass().await,
        3,
        "2 of the kind test, 1 of the kind test_other"
    );
    assert_eq!(w.pass().await, 0, "no free slot");
    w.drain().await;

    assert_eq!(record.most.load(Ordering::SeqCst), 2, "the limit of test");
    assert_eq!(
        other.most.load(Ordering::SeqCst),
        1,
        "the limit of test_other"
    );
    assert_eq!(record.runs.lock().unwrap().len(), 8);
    assert_eq!(other.runs.lock().unwrap().len(), 3);
    shut_down([w]).await;
}

// ===========================================================================
// 4. A stalled job is killed, re-leased, and fails after its tries
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_job_is_killed_and_fails_after_its_tries() {
    let url = scratch("kairos_jobs_t0373_stall");
    let record = Arc::new(Record::default());
    let w = worker(&url, stall_settings());
    register(&w, KIND, &record, Behaviour::Stall);
    let added = w
        .enqueue(NewJob {
            max_tries: 2,
            ..job(KIND, 1)
        })
        .await
        .unwrap()
        .unwrap();

    let started = std::time::Instant::now();
    w.drain().await;

    let row = db::get(&mut conn(&url), added.id).unwrap().unwrap();
    assert_eq!(row.state, "failed");
    assert_eq!(row.tries, 2);
    let reason = row.last_error.clone().unwrap_or_default();
    assert!(reason.contains("no progress"), "the reason: {reason}");
    assert_eq!(
        record.runs.lock().unwrap().len(),
        2,
        "killed once, re-leased once"
    );
    wait_until(Duration::from_secs(5), "each try was aborted", || {
        record.running.load(Ordering::SeqCst) == 0
    })
    .await;
    let failed = record.failed.lock().unwrap().clone();
    assert_eq!(failed.len(), 1, "the failed hook ran once");
    assert_eq!(failed[0].id, added.id);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "killed at the lease time, not later: {:?}",
        started.elapsed()
    );
    shut_down([w]).await;
}

// ===========================================================================
// 5. Progress keeps the lease of a long job
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_keeps_the_lease_of_a_long_job() {
    let url = scratch("kairos_jobs_t0373_progress");
    let record = Arc::new(Record::default());
    let w = worker(&url, settings(1));
    // 4 times the lease.
    register(
        &w,
        KIND,
        &record,
        Behaviour::Busy(Duration::from_millis(2400)),
    );
    let added = w.enqueue(job(KIND, 1)).await.unwrap().unwrap();

    w.drain().await;

    let row = db::get(&mut conn(&url), added.id).unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(row.tries, 1, "one lease, kept by the progress");
    assert!(row.heartbeat_at.unwrap() > row.created_at);
    shut_down([w]).await;
}

// ===========================================================================
// 6. The expired lease of a dead worker is taken by another
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_worker_takes_the_job_of_a_dead_worker() {
    let url = scratch("kairos_jobs_t0373_dead");
    let record = Arc::new(Record::default());
    let added = db::enqueue(&mut conn(&url), &job(KIND, 1))
        .unwrap()
        .unwrap();
    // A worker leases the job for 300 ms, and dies: no heartbeat, no end.
    let dead = Uuid::new_v4();
    let leased = db::take(&mut conn(&url), dead, &[KIND.to_string()], 0.3)
        .unwrap()
        .expect("the dead worker leased the job");
    assert_eq!(leased.leased_by, Some(dead));

    let w = worker(&url, settings(1));
    register(&w, KIND, &record, Behaviour::Sleep(Duration::ZERO));
    // While the lease holds, the job is not taken.
    assert_eq!(w.pass().await, 0, "the lease of the dead worker holds");
    tokio::time::sleep(Duration::from_millis(400)).await;
    w.drain().await;

    let row = db::get(&mut conn(&url), added.id).unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(row.tries, 2);
    assert_eq!(row.leased_by, Some(w.instance()));
    assert!(
        row.last_error
            .unwrap_or_default()
            .contains(&dead.to_string()),
        "the row names the server whose lease ran out"
    );
    assert_eq!(*record.runs.lock().unwrap(), vec![(added.id, w.instance())]);
    shut_down([w]).await;
}

// ===========================================================================
// 7. A dedupe key keeps a second job out while the first is open
// ===========================================================================
#[tokio::test]
async fn a_dedupe_key_keeps_a_second_job_out() {
    let url = scratch("kairos_jobs_t0373_dedupe");
    let record = Arc::new(Record::default());
    let w = worker(&url, settings(1));
    register(&w, KIND, &record, Behaviour::Sleep(Duration::ZERO));
    let keyed = |n| NewJob {
        dedupe_key: Some("acme:repo-1".into()),
        tenant: Some("acme".into()),
        ..job(KIND, n)
    };
    assert!(w.enqueue(keyed(1)).await.unwrap().is_some());
    assert!(
        w.enqueue(keyed(2)).await.unwrap().is_none(),
        "queued: not added"
    );
    // Leased: still not added.
    let leased = db::take(&mut conn(&url), Uuid::new_v4(), &[KIND.to_string()], 60.0)
        .unwrap()
        .unwrap();
    assert!(
        w.enqueue(keyed(3)).await.unwrap().is_none(),
        "leased: not added"
    );
    assert!(db::complete(&mut conn(&url), leased.id, leased.leased_by.unwrap()).unwrap());
    // Done: a new job with the key is added.
    let again = w
        .enqueue(keyed(4))
        .await
        .unwrap()
        .expect("added after the end");
    assert_eq!(again.tenant.as_deref(), Some("acme"));
    w.drain().await;
    assert_eq!(*record.order.lock().unwrap(), vec![4]);
    shut_down([w]).await;
}

// ===========================================================================
// 8. A job whose lease ran out on its last try is failed by another worker
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_ran_out_of_tries_fails_with_the_reason() {
    let url = scratch("kairos_jobs_t0373_last_try");
    let record = Arc::new(Record::default());
    let added = db::enqueue(
        &mut conn(&url),
        &NewJob {
            max_tries: 1,
            ..job(KIND, 1)
        },
    )
    .unwrap()
    .unwrap();
    let dead = Uuid::new_v4();
    db::take(&mut conn(&url), dead, &[KIND.to_string()], 0.2)
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let w = worker(&url, settings(1));
    register(&w, KIND, &record, Behaviour::Sleep(Duration::ZERO));
    w.drain().await;

    let row = db::get(&mut conn(&url), added.id).unwrap().unwrap();
    assert_eq!(row.state, "failed");
    let reason = row.last_error.unwrap_or_default();
    assert!(reason.contains("last try"), "{reason}");
    assert!(record.runs.lock().unwrap().is_empty(), "not run again");
    assert_eq!(
        record.failed.lock().unwrap().len(),
        1,
        "the failed hook ran"
    );
    shut_down([w]).await;
}

// ===========================================================================
// 9. A drain does not stop while a ready job waits for the slot of a job
//    that ends during the pass (KAIROS-T-0378)
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_runs_the_job_that_waits_for_a_slot_that_frees_during_a_pass() {
    // Given a kind with a limit of 1, and a second kind with no job: a pass
    // that finds the first kind full asks the database for a job of the
    // second kind, and the running job can end during that read
    let url = scratch("kairos_jobs_t0378_drain");
    let record = Arc::new(Record::default());
    let other = Arc::new(Record::default());
    let mut total = 0;
    for round in 0..60u64 {
        let w = worker(&url, settings(1));
        // A first job of a length that moves across the time of one pass.
        register(
            &w,
            KIND,
            &record,
            Behaviour::Sleep(Duration::from_micros(500 * (round % 30))),
        );
        register(&w, OTHER, &other, Behaviour::Sleep(Duration::ZERO));
        for n in 0..2 {
            w.enqueue(job(KIND, (round * 2 + n) as i64)).await.unwrap();
        }
        total += 2;

        // When the drain runs
        w.drain().await;

        // Then both jobs ran: the drain did not stop with the second one
        // queued
        let queued: Vec<_> = jobs_of(&url, KIND)
            .into_iter()
            .filter(|j| j.state == "queued")
            .collect();
        assert!(queued.is_empty(), "round {round}: left queued: {queued:?}");
        assert_eq!(record.runs.lock().unwrap().len(), total, "round {round}");
        shut_down([w]).await;
    }
}

// ===========================================================================
// 10. A kind that is not run again after a stop (KAIROS-T-0399): a stalled
//     job fails on its first try, and the stopped hook runs
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_job_of_a_no_retry_kind_fails_on_its_first_try() {
    let url = scratch("kairos_jobs_t0399_stall");
    let record = Arc::new(Record::default());
    let w = worker(&url, stall_settings());
    register_with(&w, KIND, &record, Behaviour::Stall, false);
    let added = w.enqueue(job(KIND, 1)).await.unwrap().unwrap();

    w.drain().await;

    let row = db::get(&mut conn(&url), added.id).unwrap().unwrap();
    assert_eq!(row.state, "failed", "{row:?}");
    assert_eq!(row.tries, 1, "not re-leased");
    let reason = row.last_error.clone().unwrap_or_default();
    assert!(reason.contains("no progress"), "the reason: {reason}");
    assert_eq!(record.runs.lock().unwrap().len(), 1, "killed once");
    let stopped = record.stopped.lock().unwrap().clone();
    assert_eq!(stopped.len(), 1, "the stopped hook ran once");
    assert_eq!(stopped[0].last_error.as_deref(), Some(reason.as_str()));
    assert_eq!(
        record.failed.lock().unwrap().len(),
        1,
        "the failed hook ran"
    );
    shut_down([w]).await;
}

// ===========================================================================
// 11. A job of a no-retry kind whose lease ran out on its first try is
//     failed by another worker; a job of another kind is taken again
//     (KAIROS-T-0399)
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_no_retry_job_of_a_dead_worker_fails_and_other_kinds_run_again() {
    let url = scratch("kairos_jobs_t0399_dead");
    let record = Arc::new(Record::default());
    let other = Arc::new(Record::default());
    let build = db::enqueue(&mut conn(&url), &job(KIND, 1))
        .unwrap()
        .unwrap();
    let plain = db::enqueue(&mut conn(&url), &job(OTHER, 2))
        .unwrap()
        .unwrap();
    // A worker leases both jobs for 200 ms, and dies.
    let dead = Uuid::new_v4();
    for kind in [KIND, OTHER] {
        db::take(&mut conn(&url), dead, &[kind.to_string()], 0.2)
            .unwrap()
            .expect("the dead worker leased the job");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let w = worker(&url, settings(1));
    register_with(&w, KIND, &record, Behaviour::Sleep(Duration::ZERO), false);
    register(&w, OTHER, &other, Behaviour::Sleep(Duration::ZERO));
    w.drain().await;

    // The no-retry job failed on try 1 of 3, and was not run again
    let row = db::get(&mut conn(&url), build.id).unwrap().unwrap();
    assert_eq!(row.state, "failed", "{row:?}");
    assert_eq!(row.tries, 1);
    let reason = row.last_error.clone().unwrap_or_default();
    assert!(
        reason.contains("try 1 of 3") && reason.contains(&dead.to_string()),
        "{reason}"
    );
    assert!(record.runs.lock().unwrap().is_empty(), "not run again");
    assert_eq!(
        record.stopped.lock().unwrap().len(),
        1,
        "the stopped hook ran"
    );
    assert_eq!(
        record.failed.lock().unwrap().len(),
        1,
        "the failed hook ran"
    );

    // The job of the other kind was taken again, and ended well
    let row = db::get(&mut conn(&url), plain.id).unwrap().unwrap();
    assert_eq!(row.state, "done", "{row:?}");
    assert_eq!(row.tries, 2);
    assert_eq!(other.runs.lock().unwrap().len(), 1);
    assert!(other.stopped.lock().unwrap().is_empty());
    shut_down([w]).await;
}
