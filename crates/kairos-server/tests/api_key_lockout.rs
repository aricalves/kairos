//! KAIROS-T-0539: one stale key does not lock the other keys behind the same
//! address.
//!
//! The throttle counts an API-key attempt by the key, and by its address at a
//! second, higher limit (`KEY_ADDRESS_FACTOR` times `KAIROS_AUTH_MAX_FAILURES`).
//! These tests send two keys from one address through the production router:
//!
//! - a stale (expired) key in a loop locks itself out, and the good key from
//!   the same address still works;
//! - a flood of different keys from one address locks the address, and the
//!   429 says which limit it is.
//!
//! The auth cache is off (`auth_cache_secs = 0`): a cached key skips the
//! throttle (KAIROS-T-0609), and these tests check the throttle itself.
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
    AUDIENCE, ISSUER, ScratchConn, base_config, drop_scratch_db, error_code, recreate_scratch_db,
    with_database,
};
use kairos_db::api_keys::{self, NewApiKey};
use kairos_db::models::{NewOrganizationMember, NewServiceAccountUser, OrgRole, User};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::rate_limit::KEY_ADDRESS_FACTOR;
use kairos_server::service_accounts::auth::{generate_key, hash_key};
use uuid::Uuid;

/// Failures of one key before its lockout. Small, so the tests are short.
const MAX_FAILURES: u32 = 3;

const ADDRESS: &str = "203.0.113.9";
const OTHER_ADDRESS: &str = "198.51.100.4";

/// A `/api/whoami` request from `peer`: the status, `Retry-After` and the body.
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
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf-8 metrics")
}

/// A scratch database with the tenant `acme`, one service account, a good
/// key and a stale (expired) key of it, and the production router.
struct Fixture {
    admin: ScratchConn,
    db: &'static str,
    router: Router,
    good_key: String,
    stale_key: String,
}

async fn fixture(db: &'static str) -> Fixture {
    let admin = recreate_scratch_db(db);
    let url = with_database(&common::admin_database_url(), db);
    let mut conn = PgConnection::establish(&url).expect("connect scratch");
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
    let key = |conn: &mut PgConnection, name: &str, expires_at| {
        let key = generate_key("acme");
        api_keys::create_key(
            conn,
            NewApiKey {
                user_id: sa.id,
                name: name.to_string(),
                token_hash: hash_key(&key),
                prefix: name.to_string(),
                created_by: sa.id,
                expires_at,
            },
        )
        .expect("create key");
        key
    };
    let good_key = key(&mut conn, "good", None);
    let stale_key = key(
        &mut conn,
        "stale",
        Some(chrono::Utc::now() - chrono::Duration::days(1)),
    );

    let pool = TenantPool::new(&url, 4).await.expect("pool");
    let auth = Arc::new(
        kairos_server::middleware::auth::Authenticator::with_static_keys(ISSUER, AUDIENCE, []),
    );
    let mut config = base_config(&url);
    config.auth_max_failures = MAX_FAILURES;
    config.auth_failure_window_secs = 300;
    config.auth_lockout_secs = 60;
    config.auth_cache_secs = 0;
    let router = app::router(app::state_with(config, pool, auth));
    Fixture {
        admin,
        db,
        router,
        good_key,
        stale_key,
    }
}

#[tokio::test]
async fn a_stale_key_in_a_loop_does_not_lock_the_good_key() {
    let mut f = fixture("kairos_api_key_lockout_stale_t0539_test").await;

    // 1. The stale key fails MAX_FAILURES times with a plain 401.
    for attempt in 1..=MAX_FAILURES {
        let (status, retry_after, body) = whoami_from(&f.router, &f.stale_key, ADDRESS, &[]).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}: {body}"
        );
        assert!(retry_after.is_none());
    }

    // 2. Then the stale key is locked out, and the 429 says it is the key.
    let (status, retry_after, body) = whoami_from(&f.router, &f.stale_key, ADDRESS, &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(error_code(&body), "KEY_LOCKED_OUT");
    assert_eq!(body["error"]["details"]["limit"], "key");
    assert_eq!(body["error"]["details"]["retry_after_secs"], 60);
    assert_eq!(retry_after.as_deref(), Some("60"));

    // 3. The loop goes on, many times over the limit of the address. The
    //    refused attempts are not counted, so the address never locks.
    for _ in 0..(MAX_FAILURES * KEY_ADDRESS_FACTOR + 5) {
        let (status, _, body) = whoami_from(&f.router, &f.stale_key, ADDRESS, &[]).await;
        assert_eq!(error_code(&body), "KEY_LOCKED_OUT", "{status}");
    }

    // 4. The good key from the SAME address works, each time.
    for _ in 0..3 {
        let (status, _, body) = whoami_from(&f.router, &f.good_key, ADDRESS, &[]).await;
        assert_eq!(status, StatusCode::OK, "the good key is not locked: {body}");
        assert_eq!(body["user"]["email"], "ci@svc.acme.kairos");
    }

    // 5. A locked key reads the same whether its tenant exists or not: the
    //    429 is no enumeration oracle.
    let unknown = generate_key("nosuchorg");
    for _ in 0..MAX_FAILURES {
        whoami_from(&f.router, &unknown, ADDRESS, &[]).await;
    }
    let (_, _, unknown_body) = whoami_from(&f.router, &unknown, ADDRESS, &[]).await;
    assert_eq!(unknown_body, body, "the 429 does not vary by the key");

    // 6. The operator sees a key lockout, and no address lockout.
    let metrics = metrics_text(&f.router).await;
    assert!(
        metrics.contains("kairos_auth_lockouts_total{subject=\"key\"} 2"),
        "{metrics}"
    );
    assert!(!metrics.contains("subject=\"key_address\""), "{metrics}");

    drop_scratch_db(&mut f.admin, f.db);
}

#[tokio::test]
async fn a_flood_of_keys_from_one_address_locks_the_address() {
    let mut f = fixture("kairos_api_key_lockout_flood_t0539_test").await;
    let address_limit = MAX_FAILURES * KEY_ADDRESS_FACTOR;

    // 1. A different bad key each time: no key reaches its own limit. The
    //    good key verifies in between, and that does not clear the count of
    //    the address.
    for attempt in 1..=address_limit {
        let (status, _, body) = whoami_from(&f.router, &generate_key("acme"), ADDRESS, &[]).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}: {body}"
        );
        if attempt == address_limit / 2 {
            let (status, _, _) = whoami_from(&f.router, &f.good_key, ADDRESS, &[]).await;
            assert_eq!(status, StatusCode::OK);
        }
    }

    // 2. The address is locked out, and the 429 says it is the address.
    let (status, retry_after, body) =
        whoami_from(&f.router, &generate_key("acme"), ADDRESS, &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(error_code(&body), "ADDRESS_LOCKED_OUT");
    assert_eq!(body["error"]["details"]["limit"], "address");
    assert_eq!(retry_after.as_deref(), Some("60"));

    // 3. It reads the same for a key of a tenant that does not exist.
    let (_, _, unknown_body) =
        whoami_from(&f.router, &generate_key("nosuchorg"), ADDRESS, &[]).await;
    assert_eq!(unknown_body, body, "the 429 does not vary by the key");

    // 4. The flood stops the good key from that address too (it is not in
    //    the auth cache).
    let (status, _, body) = whoami_from(&f.router, &f.good_key, ADDRESS, &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(error_code(&body), "ADDRESS_LOCKED_OUT");

    // 5. An untrusted X-Forwarded-For does not move the address.
    let (status, _, _) = whoami_from(
        &f.router,
        &f.good_key,
        ADDRESS,
        &[("x-forwarded-for", OTHER_ADDRESS)],
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // 6. Another address is not locked.
    let (status, _, body) = whoami_from(&f.router, &f.good_key, OTHER_ADDRESS, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let metrics = metrics_text(&f.router).await;
    assert!(
        metrics.contains("kairos_auth_lockouts_total{subject=\"key_address\"} 1"),
        "{metrics}"
    );

    drop_scratch_db(&mut f.admin, f.db);
}
