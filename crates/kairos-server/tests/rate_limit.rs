//! Integration test for the failed-authentication throttle (KAIROS-T-0202):
//! a burst of bad credentials from one source is actually refused, the refusal
//! carries `Retry-After`, it says nothing about whether the account exists, and
//! it is scoped to the source that earned it.
//!
//! The first tests drive the real production router rather than
//! [`AuthThrottle`] directly: they prove that the throttle is WIRED IN — that
//! `require_auth` consults it before it checks the credential, and that a 429
//! leaves the stack in the S-0005 envelope. Every previous version of a bug
//! like this has been a mechanism that worked and was never called.
//!
//! KAIROS-T-0377: the counts are in `public.auth_failures`, shared by every
//! server. Two app states on one database share a lockout, by source (API
//! keys) and by account (`/api/login`). The rules of KAIROS-T-0202 (the decay,
//! the identity/source split, a success during a lockout, `Retry-After`
//! rounding) were unit tests of the in-memory throttle; they run here against
//! the table, with the time as a parameter.
//!
//! Runs against the LIVE compose Postgres (`angreal services up`). No Dex is
//! needed: the API-key path never validates a JWT.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, error_code, recreate_scratch_db, with_database,
};
use kairos_db::api_keys::{self, NewApiKey};
use kairos_db::models::{NewOrganizationMember, NewServiceAccountUser, OrgRole, User};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::service_accounts::auth::{generate_key, hash_key};
use uuid::Uuid;

const SCRATCH_DB: &str = "kairos_rate_limit_t0202_test";

/// After this many failures the throttle locks the source out. Deliberately
/// smaller than the production default so the test is short; the default itself
/// is asserted in the config unit tests.
const MAX_FAILURES: u32 = 3;

/// A `/api/whoami` request from a specific socket peer.
///
/// [`ConnectInfo`] is inserted by hand because `Router::oneshot` bypasses the
/// connection layer that normally supplies it — which is exactly why the
/// throttle treats a missing one as "no source to attribute" instead of
/// panicking.
async fn whoami_from(
    router: &Router,
    key: &str,
    peer: &str,
    extra: &[(&str, &str)],
) -> (StatusCode, Option<String>, Value) {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri("/api/whoami")
        .header("authorization", format!("Bearer {key}"));
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::empty()).expect("request");
    request.extensions_mut().insert(ConnectInfo(
        format!("{peer}:44444")
            .parse::<std::net::SocketAddr>()
            .expect("peer"),
    ));

    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().expect("ascii retry-after").to_string());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON body")
    };
    (status, retry_after, body)
}

async fn metrics_text(router: &Router) -> String {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .body(Body::empty())
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf-8 metrics")
}

#[tokio::test]
async fn a_burst_of_failed_authentications_is_refused() {
    // --- scratch DB + tenant + one valid key -----------------------------
    let mut admin_conn = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provision acme");
    diesel::sql_query("SET search_path TO \"org_acme\", public")
        .execute(&mut conn)
        .expect("pin search_path");

    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org id");

    let sa: User = diesel::insert_into(users::table)
        .values(NewServiceAccountUser::new(
            "svc:ci".to_string(),
            "ci@svc.acme.kairos".to_string(),
            "ci",
        ))
        .returning(User::as_returning())
        .get_result(&mut conn)
        .expect("insert service account");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: sa.id,
            role: OrgRole::Member,
        })
        .execute(&mut conn)
        .expect("insert membership");

    let good_key = generate_key("acme");
    api_keys::create_key(
        &mut conn,
        NewApiKey {
            user_id: sa.id,
            name: "ci".to_string(),
            token_hash: hash_key(&good_key),
            prefix: "ci".to_string(),
            created_by: sa.id,
            expires_at: None,
        },
    )
    .expect("create key");

    // --- the production router, with a small threshold -------------------
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        kairos_server::middleware::auth::Authenticator::with_static_keys(ISSUER, AUDIENCE, []),
    );
    let mut config = base_config(&scratch_url);
    config.auth_max_failures = MAX_FAILURES;
    config.auth_failure_window_secs = 300;
    config.auth_lockout_secs = 60;
    let router = app::router(app::state_with(config, pool, auth));

    let attacker = "203.0.113.9";
    let bystander = "198.51.100.4";
    let bad_key = generate_key("acme");

    // 1. The first MAX_FAILURES attempts are ordinary 401s: the throttle must
    //    not fire early, or a person who mistypes twice is locked out.
    for attempt in 1..=MAX_FAILURES {
        let (status, retry_after, body) = whoami_from(&router, &bad_key, attacker, &[]).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} should be a plain 401: {body}"
        );
        assert_eq!(error_code(&body), "UNAUTHORIZED");
        assert!(retry_after.is_none(), "no Retry-After before a lockout");
    }

    // 2. The next one is refused, with Retry-After.
    let (status, retry_after, body) = whoami_from(&router, &bad_key, attacker, &[]).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "burst refused: {body}"
    );
    assert_eq!(error_code(&body), "TOO_MANY_REQUESTS");
    assert_eq!(
        retry_after.as_deref(),
        Some("60"),
        "a client needs to be told how long to wait"
    );
    assert_eq!(body["error"]["details"]["retry_after_secs"], 60);

    // 3. The refusal reveals nothing about the credential. A key for a tenant
    //    that does not exist, and a well-formed key for one that does, must
    //    produce byte-identical refusals — otherwise the 429 is an enumeration
    //    oracle that says "keep guessing, this org is real".
    let (_, _, unknown_tenant) =
        whoami_from(&router, &generate_key("nosuchorg"), attacker, &[]).await;
    assert_eq!(
        unknown_tenant, body,
        "the 429 must not vary by what was tried"
    );

    // 4. ...including for the VALID key. Being locked out is about the source,
    //    not about which credential is presented.
    let (status, _, _) = whoami_from(&router, &good_key, attacker, &[]).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a locked-out source is refused even with a good key"
    );

    // 5. A different source is untouched. Source keying is the whole point: one
    //    attacker must not be able to lock everyone else out.
    let (status, _, body) = whoami_from(&router, &good_key, bystander, &[]).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the bystander is unaffected: {body}"
    );
    assert_eq!(body["user"]["email"], "ci@svc.acme.kairos");

    // 6. X-Forwarded-For is ignored unless KAIROS_TRUSTED_PROXY is on. If it
    //    were trusted here, a spoofed header would be a free identity per
    //    request and step 2 would never have happened.
    let (status, _, _) = whoami_from(
        &router,
        &good_key,
        attacker,
        &[("x-forwarded-for", bystander)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "an untrusted X-Forwarded-For must not launder a locked-out source"
    );

    // 7. The lockout is visible to an operator.
    let metrics = metrics_text(&router).await;
    assert!(
        metrics.contains("kairos_auth_lockouts_total{subject=\"source\"} 1"),
        "the lockout counter should be exposed:\n{metrics}"
    );

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}

#[tokio::test]
async fn throttling_off_lets_a_burst_through() {
    // The escape hatch has to work, because it is what an operator reaches for
    // when a throttle misfires and locks them out of their own deployment.
    let mut admin_conn = recreate_scratch_db("kairos_rate_limit_off_t0202_test");
    let scratch_url = with_database(
        &common::admin_database_url(),
        "kairos_rate_limit_off_t0202_test",
    );
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provision acme");

    let pool = TenantPool::new(&scratch_url, 2).await.expect("pool");
    let auth = Arc::new(
        kairos_server::middleware::auth::Authenticator::with_static_keys(ISSUER, AUDIENCE, []),
    );
    let mut config = base_config(&scratch_url);
    config.auth_max_failures = 0;
    let router = app::router(app::state_with(config, pool, auth));

    let bad_key = generate_key("acme");
    for attempt in 1..=(MAX_FAILURES + 5) {
        let (status, retry_after, _) = whoami_from(&router, &bad_key, "203.0.113.9", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
        assert!(retry_after.is_none(), "throttling is off");
    }

    drop_scratch_db(&mut admin_conn, "kairos_rate_limit_off_t0202_test");
}

// ===========================================================================
// KAIROS-T-0377: the counts are shared by every server
// ===========================================================================

/// A `/api/login` request with no socket peer: only the account is counted.
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

/// Two app states on one database: the failures on one count on the other.
#[tokio::test]
async fn failed_attempts_on_one_server_count_on_another() {
    const DB: &str = "kairos_rate_limit_shared_t0377_test";
    let mut admin_conn = recreate_scratch_db(DB);
    let scratch_url = with_database(&common::admin_database_url(), DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provision acme");

    let server = || async {
        let pool = TenantPool::new(&scratch_url, 2).await.expect("pool");
        let auth = Arc::new(
            kairos_server::middleware::auth::Authenticator::with_static_keys(ISSUER, AUDIENCE, []),
        );
        let mut config = base_config(&scratch_url);
        config.local_auth = true;
        config.auth_max_failures = MAX_FAILURES;
        config.auth_failure_window_secs = 300;
        config.auth_lockout_secs = 60;
        app::router(app::state_with(config, pool, auth))
    };
    let (one, two) = (server().await, server().await);

    // By source (an API key): the failures alternate between the servers,
    // and the lockout holds on both.
    let bad_key = generate_key("acme");
    for attempt in 1..=MAX_FAILURES {
        let router = if attempt % 2 == 1 { &one } else { &two };
        let (status, _, _) = whoami_from(router, &bad_key, "203.0.113.9", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    for router in [&one, &two] {
        let (status, retry_after, _) = whoami_from(router, &bad_key, "203.0.113.9", &[]).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(retry_after.is_some());
    }
    let (status, _, _) = whoami_from(&two, &bad_key, "198.51.100.4", &[]).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "another source is not locked"
    );

    // By account (/api/login, no source): the same.
    for attempt in 1..=MAX_FAILURES {
        let router = if attempt % 2 == 1 { &two } else { &one };
        assert_eq!(
            login_as(router, "robin@acme.test", "not the password").await,
            StatusCode::UNAUTHORIZED,
            "login {attempt}"
        );
    }
    for router in [&one, &two] {
        assert_eq!(
            login_as(router, "robin@acme.test", "not the password").await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        login_as(&one, "sam@acme.test", "not the password").await,
        StatusCode::UNAUTHORIZED,
        "another account is not locked"
    );

    // The rows hold no email and no address.
    let rows: Vec<(String, String)> =
        diesel::sql_query("SELECT kind, subject FROM public.auth_failures ORDER BY kind, subject")
            .load::<KindSubject>(&mut conn)
            .expect("rows")
            .into_iter()
            .map(|r| (r.kind, r.subject))
            .collect();
    assert!(!rows.is_empty());
    for (_, subject) in &rows {
        assert!(
            !subject.contains('@') && !subject.contains('.'),
            "{subject}"
        );
    }

    drop(conn);
    drop((one, two));
    drop_scratch_db(&mut admin_conn, DB);
}

#[derive(diesel::QueryableByName)]
struct KindSubject {
    #[diesel(sql_type = diesel::sql_types::Text)]
    kind: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    subject: String,
}

// ===========================================================================
// The rules of KAIROS-T-0202, against the table (the time is a parameter)
// ===========================================================================

mod rules {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use kairos_server::blocking::BlockingTenantPool;
    use kairos_server::rate_limit::{AuthThrottle, Subject, ThrottleConfig};

    /// A throttle of 3 failures in 60 s and a lockout of 30 s (or `lockout`)
    /// on a scratch database of its own.
    fn throttle(db: &str, lockout: i64) -> (AuthThrottle, common::ScratchUrl) {
        let admin = recreate_scratch_db(db);
        let url = with_database(&common::admin_database_url(), db);
        let mut conn = PgConnection::establish(&url).expect("connect scratch");
        run_public_migrations(&mut conn).expect("public migrations");
        let throttle = AuthThrottle::new(
            ThrottleConfig {
                max_failures: 3,
                window: std::time::Duration::from_secs(60),
                lockout: std::time::Duration::from_secs(lockout as u64),
            },
            BlockingTenantPool::new(&url, 2),
        );
        (throttle, common::ScratchUrl::new(admin, url))
    }

    fn subjects() -> Vec<Subject> {
        vec![
            Subject::Identity("someone@example.test".into()),
            Subject::Source("203.0.113.9".parse().unwrap()),
        ]
    }

    fn start() -> DateTime<Utc> {
        "2026-10-11T12:00:00Z".parse().unwrap()
    }

    #[tokio::test]
    async fn failures_below_the_threshold_do_not_lock_out() {
        let (t, _db) = throttle("kairos_rate_limit_rules_below_t0377", 30);
        let (now, s) = (start(), subjects());
        assert!(t.record_failure(&s, now).await.is_none());
        assert!(t.record_failure(&s, now).await.is_none());
        assert!(t.check(&s, now).await.is_ok());
    }

    #[tokio::test]
    async fn the_threshold_locks_out_and_the_lockout_expires() {
        let (t, _db) = throttle("kairos_rate_limit_rules_expires_t0377", 30);
        let (start, s) = (start(), subjects());
        for _ in 0..3 {
            t.record_failure(&s, start).await;
        }
        let locked = t.check(&s, start).await.expect_err("locked out");
        assert!(locked.retry_after_secs <= 31, "{locked:?}");
        assert!(t.check(&s, start + Duration::seconds(29)).await.is_err());
        assert!(t.check(&s, start + Duration::seconds(31)).await.is_ok());
    }

    #[tokio::test]
    async fn failures_decay_so_a_slow_typist_is_never_locked_out() {
        let (t, _db) = throttle("kairos_rate_limit_rules_decay_t0377", 30);
        let (start, s) = (start(), subjects());
        t.record_failure(&s, start).await;
        t.record_failure(&s, start + Duration::seconds(61)).await;
        t.record_failure(&s, start + Duration::seconds(122)).await;
        assert!(t.check(&s, start + Duration::seconds(122)).await.is_ok());
    }

    #[tokio::test]
    async fn identity_and_source_are_counted_separately() {
        let (t, _db) = throttle("kairos_rate_limit_rules_split_t0377", 30);
        let now = start();
        let source = Subject::Source("203.0.113.9".parse().unwrap());
        for i in 0..3 {
            let victim = Subject::Identity(format!("victim{i}@example.test"));
            t.record_failure(&[victim, source.clone()], now).await;
        }
        assert!(
            t.check(&[Subject::Identity("victim0@example.test".into())], now)
                .await
                .is_ok()
        );
        assert!(t.check(&[source], now).await.is_err());
    }

    #[tokio::test]
    async fn a_success_clears_the_count_but_not_a_live_lockout() {
        let (t, _db) = throttle("kairos_rate_limit_rules_success_t0377", 30);
        let (now, s) = (start(), subjects());
        t.record_failure(&s, now).await;
        t.record_failure(&s, now).await;
        t.record_success(&s, now).await;
        assert!(t.record_failure(&s, now).await.is_none());
        assert!(t.record_failure(&s, now).await.is_none());
        assert!(t.record_failure(&s, now).await.is_some());
        t.record_success(&s, now).await;
        assert!(
            t.check(&s, now).await.is_err(),
            "a live lockout must survive"
        );
    }

    #[tokio::test]
    async fn retry_after_rounds_up() {
        let (t, _db) = throttle("kairos_rate_limit_rules_round_t0377", 30);
        let (start, s) = (start(), subjects());
        for _ in 0..3 {
            t.record_failure(&s, start).await;
        }
        let at = start + Duration::milliseconds(500);
        let locked = t.check(&s, at).await.expect_err("locked");
        assert!(
            t.check(&s, at + Duration::seconds(locked.retry_after_secs as i64))
                .await
                .is_ok(),
            "waiting retry_after must be enough"
        );
    }

    /// The retention pass removes a row only when it can change no
    /// decision: a lockout that outlasts the window keeps its row.
    #[tokio::test]
    async fn the_retention_pass_removes_only_what_cannot_matter() {
        use kairos_db::auth_failures::{count, delete_expired};
        let (t, url) = throttle("kairos_rate_limit_rules_expired_t0377", 600);
        let start = start();
        for i in 0..10 {
            t.record_failure(&[Subject::Identity(format!("a{i}@example.test"))], start)
                .await;
        }
        let locked = vec![Subject::Identity("locked@example.test".into())];
        for _ in 0..3 {
            t.record_failure(&locked, start).await;
        }
        let mut conn = PgConnection::establish(&url).expect("connect");
        assert_eq!(count(&mut conn).unwrap(), 11);
        assert_eq!(
            delete_expired(&mut conn, start + Duration::seconds(1)).unwrap(),
            0
        );
        let at = start + Duration::seconds(61);
        assert_eq!(
            delete_expired(&mut conn, at).unwrap(),
            10,
            "the windows ended"
        );
        assert!(t.check(&locked, at).await.is_err(), "the lockout stays");
        assert_eq!(
            delete_expired(&mut conn, start + Duration::seconds(601)).unwrap(),
            1
        );
        assert_eq!(count(&mut conn).unwrap(), 0);
    }
}
