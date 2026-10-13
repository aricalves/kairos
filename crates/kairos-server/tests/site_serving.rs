//! KAIROS-T-0407 — Kairos serves the uploaded site of a repository
//! (KAIROS-D-0012 "Serving", "When the base path changes"), against the
//! booted production router:
//!
//! - no login; the exact header set (sandbox CSP, nosniff, content type,
//!   ETag, Cache-Control); no Set-Cookie; 304; HEAD;
//! - `dir/` serves `dir/index.html`, `dir` redirects to `dir/`, a missing
//!   path serves the site's `404.html` with 404, or Kairos's plain 404;
//! - an encoded `/` and `..` are 404;
//! - a file the store lost is 503;
//! - a move to another team: the old path serves, the new path redirects
//!   to it, and after a publish the old path redirects to the new root;
//! - the team landing links to the site; the site switch off is 404.
//!
//! Runs against the LIVE compose Postgres (`angreal services up`). No Dex.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;
use uuid::Uuid;

use common::{AUDIENCE, ISSUER, base_config, recreate_scratch_db, with_database};
use kairos_db::local_auth as db_local_auth;
use kairos_db::models::{NewOrganizationMember, NewUser, OrgRole, User};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::local_auth::hash_password;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::site_store::StoreConfig;

const PASSWORD: &str = "correct-horse-battery-staple";
const SCRATCH_DB: &str = "kairos_site_serving_t0407_test";
const HOST: &str = "kairos.test";

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().expect("ascii"))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

async fn fetch(router: &Router, method: Method, uri: &str, extra: &[(&str, &str)]) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST);
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

async fn api(
    router: &Router,
    method: Method,
    uri: &str,
    token: &str,
    json_body: Option<Value>,
    gz: Option<Vec<u8>>,
) -> Value {
    let builder = Request::builder()
        .method(method.clone())
        .uri(uri)
        .header("host", HOST)
        .header("x-tenant", "acme")
        .header("authorization", format!("Bearer {token}"));
    let request = match (json_body, gz) {
        (Some(json), _) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        (None, Some(bytes)) => builder
            .header("content-type", "application/gzip")
            .body(Body::from(bytes)),
        (None, None) => builder.body(Body::empty()),
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
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert!(status.is_success(), "{method} {uri}: {status} {value}");
    value
}

fn site(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            builder
                .append_data(&mut header, path, *data)
                .expect("append");
        }
        builder.into_inner().expect("tar").finish().expect("gz");
    }
    out
}

fn fixture() -> String {
    drop(recreate_scratch_db(SCRATCH_DB));
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provision acme");
    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org id");
    let user: User = diesel::insert_into(users::table)
        .values(NewUser {
            external_id: "local:robin".into(),
            user_name: "robin@example.test".into(),
            email: "robin@example.test".into(),
            display_name: "Robin".into(),
        })
        .returning(User::as_returning())
        .get_result(&mut conn)
        .expect("insert user");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: user.id,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("membership");
    let hash = hash_password(PASSWORD).expect("hash");
    db_local_auth::set_password(&mut conn, user.id, &hash).expect("password");
    scratch_url
}

async fn login(router: &Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": "robin@example.test", "password": PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    body["token"].as_str().expect("token").to_string()
}

const ROOT: &str = "/pages/acme/platform/repos/payments/";
const SANDBOX: &str =
    "sandbox allow-scripts allow-forms allow-popups allow-downloads; frame-ancestors 'none'";

#[tokio::test]
async fn kairos_serves_a_repository_site_against_live_stack() {
    let scratch_url = fixture();
    let store_dir = tempfile::tempdir().expect("tempdir");
    let store_root = store_dir.path().join("pages");
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(Authenticator::with_static_keys(ISSUER, AUDIENCE, []));
    let mut config = base_config(&scratch_url);
    config.local_auth = true;
    config.pages.store = Some(StoreConfig::Disk {
        dir: store_root.clone(),
    });
    let router = app::router(app::state_with(config, pool, auth));
    let t = login(&router).await;

    api(
        &router,
        Method::POST,
        "/api/teams",
        &t,
        Some(json!({"name": "Platform", "slug": "platform", "code_prefix": "PLAT"})),
        None,
    )
    .await;
    api(
        &router,
        Method::POST,
        "/api/repositories",
        &t,
        Some(
            json!({"slug": "payments", "forge": "other", "repo_full_name": "payments",
                    "repo_url": "https://example.com/payments", "team": "platform"}),
        ),
        None,
    )
    .await;
    api(
        &router,
        Method::PATCH,
        "/api/repositories/payments",
        &t,
        Some(json!({"site_enabled": true})),
        None,
    )
    .await;
    let index: &[u8] = b"<h1>Payments</h1>";
    let first = api(
        &router,
        Method::PUT,
        "/api/repositories/payments/site",
        &t,
        None,
        Some(site(&[
            ("index.html", index),
            ("guide/index.html", b"<h1>Guide</h1>"),
            ("style.css", b"body{}"),
            ("404.html", b"<h1>Lost</h1>"),
        ])),
    )
    .await;
    let publish_id = first["publish_id"].as_str().unwrap().to_string();

    // --- the root, with no login, and its headers ----------------------------------
    let root = fetch(
        &router,
        Method::GET,
        ROOT,
        &[("cookie", "kairos_session=kairos_ss_anything")],
    )
    .await;
    assert_eq!(root.status, StatusCode::OK, "{}", root.text());
    assert_eq!(root.body, index);
    assert_eq!(root.header("content-security-policy"), Some(SANDBOX));
    assert_eq!(root.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        root.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(root.header("cache-control"), Some("public, max-age=60"));
    let sha: String = Sha256::digest(index)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let etag = format!("\"{sha}\"");
    assert_eq!(root.header("etag"), Some(etag.as_str()));
    assert!(root.header("set-cookie").is_none(), "no cookie is set");
    let again = fetch(&router, Method::GET, ROOT, &[("if-none-match", &etag)]).await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    let head = fetch(&router, Method::HEAD, ROOT, &[]).await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    let css = fetch(&router, Method::GET, &format!("{ROOT}style.css"), &[]).await;
    assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));

    // --- folders and 404s ---------------------------------------------------------------
    let guide = fetch(&router, Method::GET, &format!("{ROOT}guide/"), &[]).await;
    assert_eq!(guide.text(), "<h1>Guide</h1>");
    let no_slash = fetch(&router, Method::GET, &format!("{ROOT}guide"), &[]).await;
    assert_eq!(no_slash.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        no_slash.header("location"),
        Some(format!("{ROOT}guide/").as_str())
    );
    let lost = fetch(&router, Method::GET, &format!("{ROOT}nothing.html"), &[]).await;
    assert_eq!(lost.status, StatusCode::NOT_FOUND);
    assert_eq!(lost.text(), "<h1>Lost</h1>", "the site's own 404 page");
    assert_eq!(lost.header("content-security-policy"), Some(SANDBOX));
    for bad in [
        format!("{ROOT}..%2Fsecret"),
        format!("{ROOT}guide%2Findex.html"),
        format!("{ROOT}%2e%2e/x"),
    ] {
        let reply = fetch(&router, Method::GET, &bad, &[]).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{bad}");
        assert_ne!(
            reply.text(),
            "<h1>Lost</h1>",
            "{bad} is refused before the site"
        );
    }

    // --- the team landing links to the site --------------------------------------
    let landing = fetch(
        &router,
        Method::GET,
        "/pages/acme/platform/",
        &[("cookie", &format!("kairos_session={t}"))],
    )
    .await;
    assert!(
        landing.text().contains(&format!(r#"href="{ROOT}""#)),
        "{}",
        landing.text()
    );

    // --- a file the store lost: 503 ------------------------------------------------
    let org_id: String = {
        let mut conn = PgConnection::establish(&scratch_url).expect("connect");
        organizations::table
            .filter(organizations::slug.eq("acme"))
            .select(organizations::id)
            .first::<Uuid>(&mut conn)
            .expect("org")
            .to_string()
    };
    let repo = api(
        &router,
        Method::GET,
        "/api/repositories/payments",
        &t,
        None,
        None,
    )
    .await;
    let repo_id = repo["id"].as_str().unwrap().to_string();
    std::fs::remove_file(
        store_root
            .join(&org_id)
            .join(&repo_id)
            .join(&publish_id)
            .join("style.css"),
    )
    .expect("remove a stored file");
    let gone = fetch(&router, Method::GET, &format!("{ROOT}style.css"), &[]).await;
    assert_eq!(gone.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(gone.text().contains("Publish it again"), "{}", gone.text());

    // --- a move to another team -----------------------------------------------------
    api(
        &router,
        Method::POST,
        "/api/teams",
        &t,
        Some(json!({"name": "Web", "slug": "web", "code_prefix": "WEB"})),
        None,
    )
    .await;
    api(
        &router,
        Method::PATCH,
        "/api/repositories/payments",
        &t,
        Some(json!({"team": "web"})),
        None,
    )
    .await;
    const NEW_ROOT: &str = "/pages/acme/web/repos/payments/";
    let old = fetch(&router, Method::GET, ROOT, &[]).await;
    assert_eq!(old.status, StatusCode::OK, "the old path still serves");
    let new = fetch(&router, Method::GET, &format!("{NEW_ROOT}guide/"), &[]).await;
    assert_eq!(new.status, StatusCode::FOUND);
    assert_eq!(
        new.header("location"),
        Some(format!("{ROOT}guide/").as_str())
    );
    api(
        &router,
        Method::PUT,
        "/api/repositories/payments/site",
        &t,
        None,
        Some(site(&[("index.html", b"<h1>Payments v2</h1>")])),
    )
    .await;
    let new = fetch(&router, Method::GET, NEW_ROOT, &[]).await;
    assert_eq!(new.status, StatusCode::OK);
    assert_eq!(new.text(), "<h1>Payments v2</h1>");
    let old = fetch(&router, Method::GET, &format!("{ROOT}guide/"), &[]).await;
    assert_eq!(old.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(old.header("location"), Some(NEW_ROOT));
    // No 404.html in this publish: Kairos's plain 404.
    let lost = fetch(
        &router,
        Method::GET,
        &format!("{NEW_ROOT}nothing.html"),
        &[],
    )
    .await;
    assert_eq!(lost.status, StatusCode::NOT_FOUND);
    assert!(lost.text().contains("Not found"), "{}", lost.text());

    // --- the site switch off --------------------------------------------------------
    api(
        &router,
        Method::PATCH,
        "/api/repositories/payments",
        &t,
        Some(json!({"site_enabled": false})),
        None,
    )
    .await;
    let off = fetch(&router, Method::GET, NEW_ROOT, &[]).await;
    assert_eq!(off.status, StatusCode::NOT_FOUND);
}
