//! KAIROS-T-0613 — the production router against a stub issuer on a local
//! port:
//!
//! 1. `/api/config` and the token relay use the discovery document of the
//!    startup: the stub serves the document once.
//! 2. The request timeout of `/api`: a token endpoint that is slower than
//!    the timeout gives a 503 `REQUEST_TIMEOUT` at the timeout, not at the
//!    end of the slow call.
//!
//! The pool needs a real database (this binary's scratch database), as in
//! `tests/web.rs`. No route here reads it.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{AUDIENCE, base_config, drop_scratch_db, recreate_scratch_db, with_database};
use kairos_db::TenantPool;
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_idp_timeouts_t0613_test";

/// A public RSA modulus (base64url) for the JWKS of the stub. No token is
/// signed for it here: the stub only has to publish a usable key.
const STUB_N: &str = "2gKQdTOnMPT8zyKzbjydl3l9htFHvo09akZTYRCNGV_x0HrOH27OiVbcvL_1ZOKIv63bfgnGYxKmp74MNvNqg6K-HrTl5nccTgsCaN_TL6ISf3xasICBz451XwjM8qezSek1zwGzW0OzC7l7xcQzupHuF9VhqzpY9LgqV8-2KQy_8rwH5-r3n4le0gzuDZgbvBH_GFjWF55LJrmMJvt61iSdLn7mEq4YwF1UkRajp_sR1iaaDnTJuR3R2S46iD3DFMbrZPgyu-sC4qcWurEQBWKiTzkt91ge2HS8jjYZyPRxuojSqlQMbhlcJWYXEWjfEcHgo1Z8Iw_4TxU7oDzj6Q";

/// How long the token endpoint of the stub takes.
const SLOW_TOKEN: Duration = Duration::from_secs(3);

/// The stub issuer: its URL and the count of its discovery requests.
async fn stub_issuer() -> (String, Arc<AtomicU64>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let discovery_hits = Arc::new(AtomicU64::new(0));
    let document = json!({
        "issuer": url,
        "jwks_uri": format!("{url}/jwks"),
        "authorization_endpoint": format!("{url}/auth"),
        "token_endpoint": format!("{url}/token"),
        "scopes_supported": ["openid", "email", "profile"],
    });
    let hits = discovery_hits.clone();
    let stub = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move {
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(document)
            }),
        )
        .route(
            "/jwks",
            get(|| async {
                axum::Json(json!({
                    "keys": [{ "kty": "RSA", "kid": "stub", "n": STUB_N, "e": "AQAB" }]
                }))
            }),
        )
        .route(
            "/token",
            post(|| async {
                tokio::time::sleep(SLOW_TOKEN).await;
                axum::Json(json!({ "error": "invalid_grant" }))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, stub).await });
    (url, discovery_hits)
}

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    form: Option<&str>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match form {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
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

#[tokio::test]
async fn the_boot_discovery_is_reused_and_a_slow_request_times_out() {
    let mut admin_conn = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");

    let (issuer, discovery_hits) = stub_issuer().await;
    let auth = Arc::new(
        Authenticator::discover(&issuer, AUDIENCE)
            .await
            .expect("discovery against the stub"),
    );
    assert_eq!(discovery_hits.load(Ordering::SeqCst), 1, "the startup");

    let mut config = base_config(&scratch_url);
    config.oidc_issuer_url = Some(issuer.clone());
    config.request_timeout_secs = 1;
    let router = app::router(app::state_with(config, pool, auth));

    // --- 1. /api/config answers from the document of the startup --------
    for _ in 0..3 {
        let (status, body) = call(&router, Method::GET, "/api/config", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["authorization_endpoint"],
            format!("{issuer}/auth"),
            "{body}"
        );
        assert_eq!(body["scope"], "openid profile email", "{body}");
    }
    assert_eq!(
        discovery_hits.load(Ordering::SeqCst),
        1,
        "/api/config fetches no second discovery document"
    );

    // --- 2. a token relay slower than the timeout: 503 at the timeout ---
    let started = Instant::now();
    let (status, body) = call(
        &router,
        Method::POST,
        "/api/auth/token",
        Some("grant_type=refresh_token&refresh_token=stub"),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "REQUEST_TIMEOUT", "{body}");
    assert_eq!(body["error"]["details"]["timeout_secs"], 1.0, "{body}");
    assert!(
        elapsed < SLOW_TOKEN,
        "the answer comes at the timeout, not after the slow call: {elapsed:?}"
    );
    assert_eq!(
        discovery_hits.load(Ordering::SeqCst),
        1,
        "the token relay uses the document of the startup too"
    );

    // A path outside /api and /mcp is not in the timeout.
    let (status, _) = call(&router, Method::GET, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
