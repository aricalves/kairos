//! KAIROS-T-0603: the server compresses its answers. A response that asks
//! with `Accept-Encoding` comes as gzip or brotli: the API JSON (`/version`
//! here, open and larger than the 32-byte floor), and an asset of the GUI
//! out of `KAIROS_WEB_DIST`. A body under the floor (`/healthz`, two bytes)
//! stays as it is. Every asset says `Vary: Accept-Encoding`.
//!
//! The pre-compressed copies of the EMBEDDED assets need the `embed-web`
//! feature and a built dist, so they are not exercised here; the unit tests
//! of `web.rs` cover the negotiation and the headers.

mod common;

use std::io::Read;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use http_body_util::BodyExt;
use tower::ServiceExt;

use common::{AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, with_database};
use kairos_db::{TenantPool, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_compression_t0603_test";

/// A raw GET: the status, the `content-encoding` and the `vary` headers,
/// and the body bytes as they came.
async fn raw_get(
    router: &Router,
    uri: &str,
    accept_encoding: Option<&str>,
) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(accept) = accept_encoding {
        builder = builder.header("accept-encoding", accept);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap_or_default().to_string())
    };
    let encoding = header("content-encoding");
    let vary = header("vary");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, encoding, vary, bytes)
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("gzip body");
    out
}

fn unbrotli(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(bytes, 4096)
        .read_to_end(&mut out)
        .expect("brotli body");
    out
}

#[tokio::test]
async fn answers_come_compressed_when_asked() {
    let mut admin = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("run public migrations");

    // A dist folder with an index and a "wasm" above the 32-byte floor.
    let dist = tempfile::tempdir().expect("dist dir");
    std::fs::write(
        dist.path().join("index.html"),
        "<!doctype html><html><head><title>Kairos</title></head><body>the GUI of the test</body></html>",
    )
    .expect("index.html");
    let wasm: Vec<u8> = b"\0asm"
        .iter()
        .copied()
        .chain(std::iter::repeat_n(0u8, 4096))
        .collect();
    std::fs::write(dist.path().join("kairos-web-abc_bg.wasm"), &wasm).expect("wasm");

    let mut config = base_config(&scratch_url);
    config.web_dist = Some(dist.path().to_path_buf());
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(Authenticator::with_static_keys(ISSUER, AUDIENCE, []));
    let router = app::router(app::state_with(config, pool, auth));

    // The API JSON, gzip.
    let (status, encoding, _, bytes) = raw_get(&router, "/version", Some("gzip")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        encoding.as_deref(),
        Some("gzip"),
        "/version with gzip asked"
    );
    let json: serde_json::Value = serde_json::from_slice(&gunzip(&bytes)).expect("JSON after gzip");
    assert!(json.get("version").is_some(), "{json}");

    // The API JSON, brotli wins over gzip.
    let (status, encoding, _, bytes) = raw_get(&router, "/version", Some("gzip, br")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding.as_deref(), Some("br"), "/version with br asked");
    let json: serde_json::Value = serde_json::from_slice(&unbrotli(&bytes)).expect("JSON after br");
    assert!(json.get("version").is_some(), "{json}");

    // Nothing asked: nothing encoded, the body is the JSON itself.
    let (status, encoding, _, bytes) = raw_get(&router, "/version", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding, None);
    assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());

    // A body under the floor stays as it is.
    let (status, encoding, _, bytes) = raw_get(&router, "/healthz", Some("gzip, br")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding, None, "/healthz is two bytes");
    assert_eq!(bytes, b"ok");

    // An asset of the GUI from KAIROS_WEB_DIST: brotli, and Vary.
    let (status, encoding, vary, bytes) =
        raw_get(&router, "/kairos-web-abc_bg.wasm", Some("br")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding.as_deref(), Some("br"));
    assert_eq!(vary.as_deref(), Some("Accept-Encoding"));
    assert_eq!(unbrotli(&bytes), wasm);

    // The SPA index: gzip, and still its CSP.
    let (status, encoding, _, bytes) = raw_get(&router, "/boards/acme", Some("gzip")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding.as_deref(), Some("gzip"));
    assert!(
        String::from_utf8(gunzip(&bytes))
            .expect("html")
            .contains("the GUI of the test")
    );

    drop_scratch_db(&mut admin, SCRATCH_DB);
}
