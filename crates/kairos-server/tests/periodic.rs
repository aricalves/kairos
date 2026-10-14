//! The periodic passes as jobs that add their next run (KAIROS-T-0376,
//! COLLIERY-I-0613).
//!
//! 1. Two workers: each pass runs once for each interval, and one run of
//!    a pass is open at a time.
//! 2. A worker that dies during a pass: the run is leased again and runs;
//!    the next run is added once.
//! 3. A worker that dies on the last try: the run fails with the reason,
//!    and the next run is added once.
//! 4. A server that does not run a pass gives the run back, with no try
//!    counted.
//! 5. The retention pass is a periodic job: it deletes an expired session
//!    and adds its next run.
//!
//! Runs against the compose Postgres (`angreal services up`). Owns the
//! scratch databases `kairos_periodic_t0376_*`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diesel::pg::PgConnection;
use diesel::prelude::*;
use kairos_db::jobs::{self as db, Job};
use kairos_db::run_public_migrations;
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::jobs::{JobHandle, JobSettings, Worker, kinds};
use kairos_server::periodic::{self, PassFn, PeriodicPayload};
use uuid::Uuid;

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

fn settings() -> JobSettings {
    JobSettings {
        heartbeat: Duration::from_millis(100),
        lease: Duration::from_millis(600),
        max_tries: 3,
        poll: Duration::from_millis(20),
        retry_after: Duration::ZERO,
        ..JobSettings::default()
    }
}

fn worker(url: &str, settings: JobSettings) -> Arc<Worker> {
    Arc::new(Worker::new(
        BlockingTenantPool::new(url, 8),
        Uuid::new_v4(),
        settings,
    ))
}

/// The runs of a test pass: (job, worker, start).
type Runs = Arc<Mutex<Vec<(Uuid, Uuid, Instant)>>>;

/// A pass that records its run, makes progress and ends after `time`.
fn pass(runs: &Runs, worker: Uuid, time: Duration) -> PassFn {
    let runs = Arc::clone(runs);
    Arc::new(move |handle: JobHandle| {
        let runs = Arc::clone(&runs);
        Box::pin(async move {
            runs.lock()
                .unwrap()
                .push((handle.job().id, worker, Instant::now()));
            handle.progress();
            tokio::time::sleep(time).await;
            handle.progress();
            Ok(())
        })
    })
}

/// The runs of the pass `name`, oldest first.
fn runs_of(url: &str, name: &str) -> Vec<Job> {
    db::list(&mut conn(url), kinds::PERIODIC)
        .expect("jobs")
        .into_iter()
        .filter(|job| job.dedupe_key.as_deref() == Some(periodic::key(name).as_str()))
        .collect()
}

fn open(jobs: &[Job]) -> Vec<&Job> {
    jobs.iter()
        .filter(|job| job.state == "queued" || job.state == "leased")
        .collect()
}

async fn wait_until(limit: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let until = tokio::time::Instant::now() + limit;
    while !check() {
        assert!(tokio::time::Instant::now() < until, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ===========================================================================
// 1. Two workers: each pass once for each interval
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_run_each_pass_once_for_each_interval() {
    let url = scratch("kairos_periodic_t0376_twice");
    const INTERVAL: Duration = Duration::from_millis(300);
    let (one, two): (Runs, Runs) = Default::default();
    let a = worker(&url, settings());
    let b = worker(&url, settings());
    let started = Instant::now();
    for w in [&a, &b] {
        w.add_periodic(
            "one",
            INTERVAL,
            pass(&one, w.instance(), Duration::from_millis(20)),
        )
        .await
        .expect("add one");
        w.add_periodic(
            "two",
            INTERVAL,
            pass(&two, w.instance(), Duration::from_millis(20)),
        )
        .await
        .expect("add two");
    }
    // Each server added its passes at its start: one open run of each.
    assert_eq!(open(&runs_of(&url, "one")).len(), 1);
    assert_eq!(open(&runs_of(&url, "two")).len(), 1);

    let loops = [
        tokio::spawn(Arc::clone(&a).run()),
        tokio::spawn(Arc::clone(&b).run()),
    ];
    // Watch the open runs while the workers run.
    let watch_until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < watch_until {
        for name in ["one", "two"] {
            let jobs = runs_of(&url, name);
            assert!(open(&jobs).len() <= 1, "{name}: 2 open runs: {jobs:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for l in loops {
        l.abort();
    }
    let elapsed = started.elapsed();

    for (name, runs) in [("one", &one), ("two", &two)] {
        let runs = runs.lock().unwrap().clone();
        let jobs = runs_of(&url, name);
        // The first run is due at the start, then one for each interval.
        let due = (elapsed.as_millis() / INTERVAL.as_millis()) as usize + 1;
        assert!(
            runs.len() + 1 >= due && runs.len() <= due,
            "{name}: {} runs in {elapsed:?}, {due} due",
            runs.len()
        );
        // Each run once: one job for each run.
        let mut ids: Vec<Uuid> = runs.iter().map(|r| r.0).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), runs.len(), "{name}: a job ran twice");
        // The next run is due one interval after the previous run was DUE,
        // not after it started (no drift). So when a run starts late (a slow
        // runner: PR #25 saw 169 ms for an interval of 300 ms), the gap
        // between two starts is shorter than the interval. A pass that ran
        // twice for one interval starts twice at almost the same time; the
        // count above proves one run for each interval.
        for pair in runs.windows(2) {
            let gap = pair[1].2 - pair[0].2;
            assert!(gap >= INTERVAL / 3, "{name}: 2 runs {gap:?} apart");
        }
        // One open run at the end, and the next run is due one interval
        // after the last.
        assert_eq!(open(&jobs).len(), 1, "{name}: {jobs:?}");
        assert!(jobs.iter().all(|job| job.state != "failed"));
    }
}

// ===========================================================================
// 2. A worker that dies during a pass
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pass_of_a_dead_worker_is_leased_again_and_runs() {
    let url = scratch("kairos_periodic_t0376_dead");
    const INTERVAL: Duration = Duration::from_secs(3600);
    let runs: Runs = Runs::default();
    // The dead server added the first run, took it, and stopped.
    let dead = worker(&url, settings());
    let first = dead
        .add_periodic(
            "pass",
            INTERVAL,
            pass(&runs, dead.instance(), Duration::ZERO),
        )
        .await
        .expect("add")
        .expect("the first run");
    let taken = db::take(
        &mut conn(&url),
        dead.instance(),
        &[kinds::PERIODIC.to_string()],
        0.3,
    )
    .expect("take")
    .expect("the run");
    assert_eq!(taken.id, first.id);

    // A live server starts: the run is open, so it adds none.
    let live = worker(&url, settings());
    let added = live
        .add_periodic(
            "pass",
            INTERVAL,
            pass(&runs, live.instance(), Duration::ZERO),
        )
        .await
        .expect("add");
    assert!(added.is_none(), "a run is open");
    let lp = tokio::spawn(Arc::clone(&live).run());
    wait_until(
        Duration::from_secs(10),
        "the run on the live server",
        || runs_of(&url, "pass").len() == 2,
    )
    .await;
    lp.abort();

    let ran = runs.lock().unwrap().clone();
    assert_eq!(ran.len(), 1);
    assert_eq!((ran[0].0, ran[0].1), (first.id, live.instance()));
    let jobs = runs_of(&url, "pass");
    let done = db::get(&mut conn(&url), first.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(done.tries, 2);
    assert!(
        done.last_error
            .unwrap_or_default()
            .contains("lease ran out"),
        "the reason of the second lease"
    );
    let next = open(&jobs);
    assert_eq!(next.len(), 1, "one next run: {jobs:?}");
    assert_eq!(next[0].state, "queued");
    let due = first.run_after + chrono::Duration::hours(1);
    assert!((next[0].run_after - due).num_seconds().abs() <= 1);
    let payload: PeriodicPayload = serde_json::from_str(&next[0].payload).unwrap();
    assert_eq!(payload.pass, "pass");
}

// ===========================================================================
// 3. A worker that dies on the last try
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pass_that_dies_on_its_last_try_fails_and_the_next_run_is_added() {
    let url = scratch("kairos_periodic_t0376_last");
    const INTERVAL: Duration = Duration::from_secs(3600);
    let runs: Runs = Runs::default();
    let dead = worker(
        &url,
        JobSettings {
            max_tries: 1,
            ..settings()
        },
    );
    let first = dead
        .add_periodic(
            "pass",
            INTERVAL,
            pass(&runs, dead.instance(), Duration::ZERO),
        )
        .await
        .expect("add")
        .expect("the first run");
    db::take(
        &mut conn(&url),
        dead.instance(),
        &[kinds::PERIODIC.to_string()],
        0.2,
    )
    .expect("take")
    .expect("the run");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let live = worker(&url, settings());
    live.add_periodic(
        "pass",
        INTERVAL,
        pass(&runs, live.instance(), Duration::ZERO),
    )
    .await
    .expect("add");
    live.drain().await;

    assert!(runs.lock().unwrap().is_empty(), "no try is left");
    let failed = db::get(&mut conn(&url), first.id).unwrap().unwrap();
    assert_eq!(failed.state, "failed");
    assert!(
        failed
            .last_error
            .unwrap_or_default()
            .contains("on the last try"),
    );
    let jobs = runs_of(&url, "pass");
    let next = open(&jobs);
    assert_eq!(next.len(), 1, "one next run: {jobs:?}");
    assert!(next[0].run_after > chrono::Utc::now() + chrono::Duration::minutes(59));
}

// ===========================================================================
// 4. A server that does not run a pass gives the run back
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_gives_back_a_pass_that_it_does_not_run() {
    let url = scratch("kairos_periodic_t0376_not_here");
    let runs: Runs = Runs::default();
    let other = worker(&url, settings());
    let theirs = other
        .add_periodic(
            "theirs",
            Duration::from_secs(60),
            pass(&runs, other.instance(), Duration::ZERO),
        )
        .await
        .expect("add")
        .expect("the first run");
    let here = worker(
        &url,
        JobSettings {
            retry_after: Duration::from_secs(3600),
            ..settings()
        },
    );
    here.add_periodic(
        "mine",
        Duration::from_secs(60),
        pass(&runs, here.instance(), Duration::ZERO),
    )
    .await
    .expect("add");
    here.drain().await;

    let ran = runs.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "only the pass of this server ran");
    let back = db::get(&mut conn(&url), theirs.id).unwrap().unwrap();
    assert_eq!(back.state, "queued");
    assert_eq!(back.tries, 0, "no try counted");
    assert!(
        back.last_error
            .unwrap_or_default()
            .contains("does not run the pass theirs")
    );
    assert!(back.run_after > chrono::Utc::now() + chrono::Duration::minutes(59));
}

// ===========================================================================
// 5. The retention pass is a periodic job
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retention_pass_runs_as_a_periodic_job() {
    use kairos_db::local_auth::{NewLocalSession, create_session};
    let url = scratch("kairos_periodic_t0376_retention");
    let mut c = conn(&url);
    kairos_db::provision_tenant(&mut c, "acme", "Acme").expect("provision");
    let robin = diesel::insert_into(kairos_db::schema::users::table)
        .values(kairos_db::models::NewUser {
            external_id: "local|robin".into(),
            user_name: "local|robin".into(),
            email: "robin@acme.test".into(),
            display_name: "Robin".into(),
        })
        .returning(kairos_db::schema::users::id)
        .get_result::<Uuid>(&mut c)
        .expect("robin");
    for (hash, expires_at) in [
        ("expired", chrono::Utc::now() - chrono::Duration::hours(1)),
        ("live", chrono::Utc::now() + chrono::Duration::hours(1)),
    ] {
        create_session(
            &mut c,
            NewLocalSession {
                user_id: robin,
                token_hash: hash.into(),
                expires_at,
            },
        )
        .expect("session");
    }

    let w = worker(&url, settings());
    w.add_periodic(
        periodic::RETENTION,
        periodic::RETENTION_INTERVAL,
        periodic::retention_pass(
            url.to_string(),
            kairos_core::retention::RetentionConfig::default(),
        ),
    )
    .await
    .expect("add")
    .expect("the first run");
    w.drain().await;

    let hashes: Vec<String> = kairos_db::schema::local_sessions::table
        .select(kairos_db::schema::local_sessions::token_hash)
        .load(&mut c)
        .expect("sessions");
    assert_eq!(hashes, vec!["live"]);
    let jobs = runs_of(&url, periodic::RETENTION);
    let states: Vec<&str> = jobs.iter().map(|j| j.state.as_str()).collect();
    assert_eq!(states, vec!["done", "queued"]);
    assert!(jobs[1].run_after > chrono::Utc::now() + chrono::Duration::hours(23));
}
