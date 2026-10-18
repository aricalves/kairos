//! Signed webhook deliveries on the job queue (KAIROS-T-0384,
//! COLLIERY-I-0614).
//!
//! 1. A delivery is signed: a test receiver checks `X-Kairos-Signature`
//!    with the secret, and reads `X-Kairos-Delivery` and `X-Kairos-Event`.
//!    The row of the target holds no plain secret, and the rotation of
//!    `KAIROS_SECRETS_KEY` seals it again.
//! 2. A failing target is tried again with growing waits; the log shows
//!    each try.
//! 3. Failed tries in a row turn the target off, and its owners (the
//!    person who made it and the board managers, not a plain member) get a
//!    `delivery_failing` notification; a new delivery is `skipped`.
//! 4. Two servers deliver each event once.
//! 5. The guard of the addresses: with `KAIROS_WEBHOOK_ALLOW_PRIVATE` off,
//!    a target on 127.0.0.1 gets no request, and the log says why.
//! 6. KAIROS-T-0577: enable sends the skipped deliveries again, oldest
//!    first, with their ids, and keeps the log of an earlier try; a
//!    delivery older than the retention (30 days) does not go; `since`
//!    limits a redeliver.
//!
//! Runs against the compose Postgres (`angreal services up`). Owns the
//! scratch databases `kairos_webhooks_t0384_*` and
//! `kairos_webhooks_t0577_*`.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use kairos_db::models::{BoardLevel, NewUser};
use kairos_db::webhooks as db;
use kairos_db::{create_board, provision_tenant, run_public_migrations, schema};
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::jobs::{JobSettings, Worker, kinds};
use kairos_server::secrets::SecretsKey;
use kairos_server::webhooks::{WebhookService, WebhookSettings, new_target, signature};
use uuid::Uuid;

const TENANT: &str = "acme";

// ---------------------------------------------------------------------------
// The test receiver
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Received {
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Default)]
struct Receiver {
    received: Mutex<Vec<Received>>,
    /// The next requests that get a 500.
    fail_next: AtomicUsize,
    /// Each request gets a 500.
    always_fail: std::sync::atomic::AtomicBool,
}

async fn receive(
    State(receiver): State<Arc<Receiver>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    receiver.received.lock().unwrap().push(Received {
        headers: headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: body.to_vec(),
    });
    // A little time, so that 2 servers overlap.
    tokio::time::sleep(Duration::from_millis(5)).await;
    if receiver.always_fail.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let left = receiver.fail_next.load(Ordering::SeqCst);
    if left > 0 {
        receiver.fail_next.store(left - 1, Ordering::SeqCst);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::NO_CONTENT
}

async fn serve_receiver() -> (Arc<Receiver>, SocketAddr) {
    let receiver = Arc::new(Receiver::default());
    let app = Router::new()
        .route("/hook", post(receive))
        .with_state(Arc::clone(&receiver));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("receiver") });
    (receiver, addr)
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

struct Fixture {
    url: String,
    _db: common::ScratchConn,
    board: Uuid,
    /// The person who makes the targets.
    robin: Uuid,
    /// A board manager (an explicit grant of configure_boards).
    sam: Uuid,
    /// A member with no grant on the board.
    jamie: Uuid,
}

fn pin(conn: &mut PgConnection) {
    sql_query(format!("SET search_path TO org_{TENANT}, public"))
        .execute(conn)
        .expect("pin");
}

fn person(conn: &mut PgConnection, name: &str) -> Uuid {
    let id = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: format!("dex|{name}"),
            user_name: name.to_string(),
            email: format!("{name}@example.test"),
            display_name: name.to_string(),
        })
        .returning(schema::users::id)
        .get_result::<Uuid>(conn)
        .expect("user");
    sql_query(format!(
        "INSERT INTO public.organization_members (organization_id, user_id, role) \
         SELECT id, '{id}', 'member' FROM public.organizations WHERE slug = '{TENANT}'"
    ))
    .execute(conn)
    .expect("member");
    id
}

fn fixture(name: &str) -> Fixture {
    let admin = common::recreate_scratch_db(name);
    let url = common::with_database(&common::admin_database_url(), name);
    let mut conn = PgConnection::establish(&url).expect("connect to the scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, TENANT, TENANT).expect("provision");
    pin(&mut conn);
    let robin = person(&mut conn, "robin");
    let sam = person(&mut conn, "sam");
    let jamie = person(&mut conn, "jamie");
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
    sql_query(format!(
        "INSERT INTO board_member_capabilities (board_id, user_id, capability, granted_by) \
         VALUES ('{board}', '{sam}', 'configure_boards', '{robin}'), \
                ('{board}', '{jamie}', 'manage_tasks', '{robin}')"
    ))
    .execute(&mut conn)
    .expect("grants");
    Fixture {
        url,
        _db: admin,
        board,
        robin,
        sam,
        jamie,
    }
}

fn key() -> SecretsKey {
    SecretsKey::from_bytes([42u8; 32])
}

fn conn(url: &str) -> PgConnection {
    let mut conn = PgConnection::establish(url).expect("connect");
    pin(&mut conn);
    conn
}

/// Add a target on the board for `url`; its id and its plain secret.
fn add_target(f: &Fixture, url: &str) -> (Uuid, String) {
    let mut conn = conn(&f.url);
    let (row, secret) = new_target(&key(), TENANT, f.board, "Runner", url, f.robin);
    let target = db::insert_target(&mut conn, &row).expect("target");
    (target.id, secret)
}

fn queue(f: &Fixture, target: Uuid, n: usize) -> db::Delivery {
    let mut conn = conn(&f.url);
    db::queue_delivery(
        &mut conn,
        target,
        "ping",
        None,
        None,
        &serde_json::json!({ "n": n }),
    )
    .expect("queue")
    .expect("a target")
}

fn job_settings() -> JobSettings {
    job_settings_with(2)
}

/// The settings of the queue, with `limit` deliveries at one time.
fn job_settings_with(limit: usize) -> JobSettings {
    let mut settings = JobSettings {
        heartbeat: Duration::from_millis(100),
        lease: Duration::from_secs(5),
        poll: Duration::from_millis(50),
        retry_after: Duration::ZERO,
        ..JobSettings::default()
    };
    settings
        .limits
        .insert(kinds::WEBHOOK_DELIVERY.to_string(), limit);
    settings
}

fn hook_settings() -> WebhookSettings {
    WebhookSettings {
        max_tries: 4,
        retry_base: Duration::from_millis(20),
        timeout: Duration::from_secs(5),
        disable_after: 24,
        // The test receiver is on 127.0.0.1.
        allow_private: true,
        ..WebhookSettings::default()
    }
}

fn worker(url: &str, settings: WebhookSettings) -> Arc<Worker> {
    worker_with(url, settings, job_settings())
}

fn worker_with(url: &str, settings: WebhookSettings, jobs: JobSettings) -> Arc<Worker> {
    let blocking = BlockingTenantPool::new(url, 4);
    let worker = Arc::new(Worker::new(blocking.clone(), Uuid::new_v4(), jobs));
    WebhookService::new(blocking, Some(key()), settings).register(&worker);
    worker
}

fn delivery(url: &str, id: Uuid) -> db::Delivery {
    db::get_delivery(&mut conn(url), id)
        .expect("read")
        .expect("delivery")
}

fn tries(url: &str, id: Uuid) -> Vec<db::DeliveryTry> {
    db::tries_of(&mut conn(url), &[id]).expect("tries")
}

fn notified(url: &str, kind: &str) -> Vec<(Uuid, String)> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        user_id: Uuid,
        #[diesel(sql_type = diesel::sql_types::Text)]
        text: String,
    }
    let rows: Vec<Row> = sql_query(format!(
        "SELECT user_id, text FROM notifications WHERE kind = '{kind}' ORDER BY user_id"
    ))
    .load(&mut conn(url))
    .expect("notifications");
    rows.into_iter().map(|r| (r.user_id, r.text)).collect()
}

/// Run `worker` until no delivery job is open (a job that waits for its
/// next try is open), for at most `limit`.
async fn drain(worker: &Arc<Worker>, url: &str, limit: Duration) {
    let url = url.to_string();
    let run = async {
        loop {
            worker.pass().await;
            let open = {
                let mut conn = PgConnection::establish(&url).expect("connect");
                kairos_db::jobs::list(&mut conn, kinds::WEBHOOK_DELIVERY)
                    .expect("jobs")
                    .iter()
                    .any(|j| j.state == "queued" || j.state == "leased")
            };
            if !open && worker.running() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(limit, run)
        .await
        .expect("the worker drained in time");
}

// ===========================================================================
// 1. A signed delivery
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_is_signed_and_the_receiver_checks_it() {
    let f = fixture("kairos_webhooks_t0384_signed");
    let (receiver, addr) = serve_receiver().await;
    let (target, secret) = add_target(&f, &format!("http://{addr}/hook"));
    let queued = queue(&f, target, 1);
    assert_eq!(queued.status, "pending");

    // The job is on the queue, with the priority between the embeddings and
    // the periodic passes.
    let jobs = kairos_db::jobs::list(
        &mut PgConnection::establish(&f.url).unwrap(),
        kinds::WEBHOOK_DELIVERY,
    )
    .expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].priority, 15);
    assert_eq!(jobs[0].tenant.as_deref(), Some(TENANT));

    let worker = worker(&f.url, hook_settings());
    drain(&worker, &f.url, Duration::from_secs(20)).await;

    let received = receiver.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    let request = &received[0];
    // The receiver computes the HMAC with the secret that Kairos showed.
    assert_eq!(
        request.headers["x-kairos-signature"],
        signature(&secret, &request.body)
    );
    assert_ne!(
        request.headers["x-kairos-signature"],
        signature("a wrong secret", &request.body)
    );
    assert_eq!(request.headers["x-kairos-delivery"], queued.id.to_string());
    assert_eq!(request.headers["x-kairos-event"], "ping");
    assert_eq!(request.headers["content-type"], "application/json");
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("JSON");
    assert_eq!(body["id"], queued.id.to_string());
    assert_eq!(body["event"], "ping");
    assert_eq!(body["organization"], TENANT);
    assert_eq!(body["data"]["n"], 1);

    let done = delivery(&f.url, queued.id);
    assert_eq!(done.status, "delivered");
    assert_eq!((done.tries, done.response_code), (1, Some(204)));
    let log = tries(&f.url, queued.id);
    assert_eq!(log.len(), 1);
    assert!(log[0].ok);
    assert_eq!(log[0].response_code, Some(204));

    // The row holds the sealed secret only.
    let sealed = db::target_secret(&mut conn(&f.url), target)
        .expect("read")
        .expect("secret");
    assert!(
        !sealed
            .ciphertext
            .windows(secret.len())
            .any(|w| w == secret.as_bytes()),
        "the row must not hold the secret"
    );
    assert!(!format!("{sealed:?}").contains(&secret));

    // KAIROS_SECRETS_KEY rotates: the secret is sealed again, and opens.
    let ring = SecretsKey::from_bytes([43u8; 32]).with_old_bytes([42u8; 32]);
    let mut admin = PgConnection::establish(&f.url).unwrap();
    let report =
        kairos_server::secrets_rotation::rotate_tenant(&mut admin, TENANT, &ring).expect("rotate");
    assert_eq!(report.resealed, 1, "{report:?}");
    let sealed = db::target_secret(&mut conn(&f.url), target)
        .expect("read")
        .expect("secret");
    let opened = SecretsKey::from_bytes([43u8; 32])
        .open(
            &kairos_server::secrets::webhook_target_aad(TENANT, target),
            &sealed.key_id,
            &sealed.nonce,
            &sealed.ciphertext,
        )
        .expect("the new key opens");
    assert_eq!(opened, secret.as_bytes());
}

// ===========================================================================
// 2. Tries with growing waits
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_target_is_tried_again_and_the_log_shows_each_try() {
    let f = fixture("kairos_webhooks_t0384_retry");
    let (receiver, addr) = serve_receiver().await;
    receiver.fail_next.store(2, Ordering::SeqCst);
    let (target, _) = add_target(&f, &format!("http://{addr}/hook"));
    let queued = queue(&f, target, 1);

    let worker = worker(&f.url, hook_settings());
    drain(&worker, &f.url, Duration::from_secs(30)).await;

    let done = delivery(&f.url, queued.id);
    assert_eq!(done.status, "delivered", "{done:?}");
    assert_eq!(done.tries, 3);
    let log = tries(&f.url, queued.id);
    let codes: Vec<(i32, bool, Option<i32>)> = log
        .iter()
        .map(|t| (t.try_number, t.ok, t.response_code))
        .collect();
    assert_eq!(
        codes,
        vec![
            (1, false, Some(500)),
            (2, false, Some(500)),
            (3, true, Some(204))
        ]
    );
    assert!(log[0].error.as_deref().unwrap_or("").contains("500"));
    // The waits grow: the second gap is longer than the first.
    let first = log[1].at - log[0].at;
    let second = log[2].at - log[1].at;
    assert!(
        first >= chrono::Duration::milliseconds(20),
        "first wait {first}"
    );
    assert!(
        second >= chrono::Duration::milliseconds(40),
        "second wait {second}"
    );
    // A good try sets the failures in a row of the target to 0.
    let target = db::get_target(&mut conn(&f.url), target)
        .expect("read")
        .expect("target");
    assert_eq!(target.consecutive_failures, 0);
    assert!(target.enabled);
    assert_eq!(receiver.received.lock().unwrap().len(), 3);

    // A delivery that uses each try ends failed with the reason.
    receiver.always_fail.store(true, Ordering::SeqCst);
    let queued = queue(&f, target.id, 2);
    drain(&worker, &f.url, Duration::from_secs(30)).await;
    let done = delivery(&f.url, queued.id);
    assert_eq!(done.status, "failed", "{done:?}");
    assert_eq!(done.tries, 4);
    assert_eq!(tries(&f.url, queued.id).len(), 4);
    assert!(done.last_error.unwrap_or_default().contains("500"));
}

// ===========================================================================
// 3. Turned off after failures, and the owners are told
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_in_a_row_turn_the_target_off_and_tell_its_owners() {
    let f = fixture("kairos_webhooks_t0384_off");
    let (receiver, addr) = serve_receiver().await;
    receiver.always_fail.store(true, Ordering::SeqCst);
    let (target, _) = add_target(&f, &format!("http://{addr}/hook"));
    let settings = WebhookSettings {
        max_tries: 2,
        disable_after: 3,
        ..hook_settings()
    };
    let worker = worker(&f.url, settings);

    // First delivery: 2 failed tries (2 in a row).
    let first = queue(&f, target, 1);
    drain(&worker, &f.url, Duration::from_secs(20)).await;
    assert_eq!(delivery(&f.url, first.id).status, "failed");
    let row = db::get_target(&mut conn(&f.url), target).unwrap().unwrap();
    assert!(row.enabled);
    assert_eq!(row.consecutive_failures, 2);
    assert!(notified(&f.url, "delivery_failing").is_empty());

    // Second delivery: the third failed try turns the target off; no more
    // try of that delivery.
    let second = queue(&f, target, 2);
    drain(&worker, &f.url, Duration::from_secs(20)).await;
    let done = delivery(&f.url, second.id);
    assert_eq!(done.status, "failed");
    assert_eq!(done.tries, 1);
    assert!(
        done.last_error
            .unwrap_or_default()
            .contains("turned the target off"),
    );
    let row = db::get_target(&mut conn(&f.url), target).unwrap().unwrap();
    assert!(!row.enabled);
    assert!(row.disabled_at.is_some());
    assert!(
        row.disabled_reason
            .as_deref()
            .unwrap_or("")
            .contains("3 failed tries in a row"),
        "{row:?}"
    );

    // The owners are told: the person who made it and the board manager,
    // not the member with no configure grant.
    let told = notified(&f.url, "delivery_failing");
    let mut expected = vec![f.robin, f.sam];
    expected.sort();
    assert_eq!(told.iter().map(|t| t.0).collect::<Vec<_>>(), expected);
    assert!(!told.iter().any(|t| t.0 == f.jamie));
    assert!(told[0].1.contains("Runner"), "{}", told[0].1);
    assert!(told[0].1.contains("delivery"), "{}", told[0].1);

    // A new event to the target that is off is skipped, with no request.
    let before = receiver.received.lock().unwrap().len();
    let third = queue(&f, target, 3);
    assert_eq!(third.status, "skipped");
    drain(&worker, &f.url, Duration::from_secs(10)).await;
    assert_eq!(receiver.received.lock().unwrap().len(), before);

    // Turned on again: the failures in a row are 0, the skipped delivery
    // goes (KAIROS-T-0577), and a new delivery goes.
    receiver.always_fail.store(false, Ordering::SeqCst);
    let (on, redelivered) = db::enable_target(&mut conn(&f.url), target)
        .unwrap()
        .unwrap();
    assert!(on.enabled);
    assert_eq!(on.consecutive_failures, 0);
    assert_eq!(
        redelivered.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![third.id]
    );
    let fourth = queue(&f, target, 4);
    drain(&worker, &f.url, Duration::from_secs(20)).await;
    assert_eq!(delivery(&f.url, third.id).status, "delivered");
    assert_eq!(delivery(&f.url, fourth.id).status, "delivered");
    // The failed deliveries do not go again.
    assert_eq!(delivery(&f.url, first.id).status, "failed");
    assert_eq!(delivery(&f.url, second.id).status, "failed");
}

// ===========================================================================
// 6. KAIROS-T-0577: the skipped deliveries go again, oldest first
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enable_sends_the_skipped_deliveries_in_order_within_the_retention() {
    let f = fixture("kairos_webhooks_t0577_replay");
    let (receiver, addr) = serve_receiver().await;
    let (target, _) = add_target(&f, &format!("http://{addr}/hook"));
    let set_on = |on: bool| {
        sql_query(format!(
            "UPDATE webhook_targets SET enabled = {on} WHERE id = '{target}'"
        ))
        .execute(&mut conn(&f.url))
        .expect("set the target");
    };
    // One delivery at a time, so the order of the requests is the order
    // of the queue.
    let worker = worker_with(&f.url, hook_settings(), job_settings_with(1));

    // The target is off: 4 events are skipped, and nothing is sent.
    set_on(false);
    let skipped: Vec<db::Delivery> = (1..=4).map(|n| queue(&f, target, n)).collect();
    assert!(skipped.iter().all(|d| d.status == "skipped"));
    drain(&worker, &f.url, Duration::from_secs(10)).await;
    assert!(receiver.received.lock().unwrap().is_empty());
    // The first is older than the retention (30 days): it does not go.
    // The second had a try before the target went off: its log stays.
    sql_query(format!(
        "UPDATE webhook_deliveries SET created_at = now() - interval '31 days', \
             finished_at = now() - interval '31 days' WHERE id = '{}'",
        skipped[0].id
    ))
    .execute(&mut conn(&f.url))
    .expect("an old event");
    sql_query(format!(
        "UPDATE webhook_deliveries SET tries = 1 WHERE id = '{}'",
        skipped[1].id
    ))
    .execute(&mut conn(&f.url))
    .expect("an earlier try");
    sql_query(format!(
        "INSERT INTO webhook_delivery_tries (delivery_id, try, ok, response_code, error, \
             duration_ms) VALUES ('{}', 1, false, 500, 'The target answered 500.', 3)",
        skipped[1].id
    ))
    .execute(&mut conn(&f.url))
    .expect("the log of the earlier try");

    // A new event while the target is off removes the finished deliveries
    // older than the retention.
    let fifth = queue(&f, target, 5);
    assert_eq!(fifth.status, "skipped");
    assert!(
        db::get_delivery(&mut conn(&f.url), skipped[0].id)
            .unwrap()
            .is_none(),
        "the retention removed the old event"
    );

    // Turned on: the skipped deliveries go, oldest first, with their ids.
    let (on, redelivered) = db::enable_target(&mut conn(&f.url), target)
        .unwrap()
        .unwrap();
    assert!(on.enabled);
    let want: Vec<Uuid> = vec![skipped[1].id, skipped[2].id, skipped[3].id, fifth.id];
    assert_eq!(redelivered.iter().map(|d| d.id).collect::<Vec<_>>(), want);
    assert!(redelivered.iter().all(|d| d.status == "pending"));
    // A new event after the enable goes after them.
    let sixth = queue(&f, target, 6);
    drain(&worker, &f.url, Duration::from_secs(30)).await;
    let received = receiver.received.lock().unwrap().clone();
    let order: Vec<String> = received
        .iter()
        .map(|r| r.headers["x-kairos-delivery"].clone())
        .collect();
    let mut expected: Vec<String> = want.iter().map(Uuid::to_string).collect();
    expected.push(sixth.id.to_string());
    assert_eq!(order, expected);
    let ns: Vec<i64> = received
        .iter()
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["data"]["n"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert_eq!(ns, vec![2, 3, 4, 5, 6]);
    for id in &want {
        assert_eq!(delivery(&f.url, *id).status, "delivered");
    }
    // The earlier try stays in the log; the new try is the second.
    let log = tries(&f.url, skipped[1].id);
    assert_eq!(
        log.iter()
            .map(|t| (t.try_number, t.ok, t.response_code))
            .collect::<Vec<_>>(),
        vec![(1, false, Some(500)), (2, true, Some(204))]
    );
    assert_eq!(delivery(&f.url, skipped[1].id).tries, 2);

    // Nothing is left to send: a second replay queues nothing.
    assert!(
        db::redeliver_skipped(&mut conn(&f.url), target, None)
            .unwrap()
            .is_empty()
    );

    // An old row that the retention did not remove yet does not go
    // either: redeliver takes the last 30 days only, and `since` limits it
    // more.
    set_on(false);
    let stale = queue(&f, target, 7);
    let recent = queue(&f, target, 8);
    let newest = queue(&f, target, 9);
    for (id, age) in [(stale.id, "40 days"), (recent.id, "2 hours")] {
        sql_query(format!(
            "UPDATE webhook_deliveries SET created_at = now() - interval '{age}' \
             WHERE id = '{id}'"
        ))
        .execute(&mut conn(&f.url))
        .expect("an older event");
    }
    set_on(true);
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let again = db::redeliver_skipped(&mut conn(&f.url), target, Some(since)).unwrap();
    assert_eq!(
        again.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![newest.id]
    );
    let again = db::redeliver_skipped(&mut conn(&f.url), target, None).unwrap();
    assert_eq!(
        again.iter().map(|d| d.id).collect::<Vec<_>>(),
        vec![recent.id]
    );
    drain(&worker, &f.url, Duration::from_secs(20)).await;
    assert_eq!(delivery(&f.url, stale.id).status, "skipped");
    assert_eq!(delivery(&f.url, recent.id).status, "delivered");
    assert_eq!(delivery(&f.url, newest.id).status, "delivered");
}

// ===========================================================================
// 4. Two servers deliver each event once
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_servers_deliver_each_event_once() {
    const EVENTS: usize = 40;
    let f = fixture("kairos_webhooks_t0384_once");
    let (receiver, addr) = serve_receiver().await;
    let (target, _) = add_target(&f, &format!("http://{addr}/hook"));
    let mut ids = Vec::new();
    for n in 0..EVENTS {
        ids.push(queue(&f, target, n).id);
    }
    let a = worker(&f.url, hook_settings());
    let b = worker(&f.url, hook_settings());
    tokio::join!(
        drain(&a, &f.url, Duration::from_secs(60)),
        drain(&b, &f.url, Duration::from_secs(60)),
    );

    let received = receiver.received.lock().unwrap().clone();
    let mut count: HashMap<String, usize> = HashMap::new();
    for request in &received {
        *count
            .entry(request.headers["x-kairos-delivery"].clone())
            .or_default() += 1;
    }
    assert_eq!(count.len(), EVENTS, "each event arrived");
    assert!(
        count.values().all(|n| *n == 1),
        "an event arrived twice: {count:?}"
    );
    for id in ids {
        assert_eq!(delivery(&f.url, id).status, "delivered");
    }
}

// ===========================================================================
// 5. The guard of the addresses
// ===========================================================================
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_address_gets_no_request_unless_the_setting_allows_it() {
    let f = fixture("kairos_webhooks_t0384_ssrf");
    let (receiver, addr) = serve_receiver().await;
    let (target, _) = add_target(&f, &format!("http://{addr}/hook"));
    let queued = queue(&f, target, 1);
    let guarded = worker(
        &f.url,
        WebhookSettings {
            max_tries: 1,
            allow_private: false,
            ..hook_settings()
        },
    );
    drain(&guarded, &f.url, Duration::from_secs(20)).await;
    assert!(
        receiver.received.lock().unwrap().is_empty(),
        "no request reached the private address"
    );
    let done = delivery(&f.url, queued.id);
    assert_eq!(done.status, "failed");
    let log = tries(&f.url, queued.id);
    assert_eq!(log.len(), 1);
    assert!(
        log[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("KAIROS_WEBHOOK_ALLOW_PRIVATE"),
        "{log:?}"
    );
    // A name that resolves to loopback is refused too.
    let (named, _) = add_target(&f, &format!("http://localhost.:{}/hook", addr.port()));
    let queued = queue(&f, named, 2);
    drain(&guarded, &f.url, Duration::from_secs(20)).await;
    assert_eq!(delivery(&f.url, queued.id).status, "failed");
    assert!(receiver.received.lock().unwrap().is_empty());

    // With the setting on, the same target gets the request.
    let open = worker(&f.url, hook_settings());
    let queued = queue(&f, target, 3);
    drain(&open, &f.url, Duration::from_secs(20)).await;
    assert_eq!(delivery(&f.url, queued.id).status, "delivered");
    assert_eq!(receiver.received.lock().unwrap().len(), 1);
}
