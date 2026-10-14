//! KAIROS-T-0403 — the `/pages` router of the docs sites (KAIROS-D-0013,
//! KAIROS-D-0012), against the booted production router:
//!
//! - the tenant comes from the path, and the host or `KAIROS_SINGLE_TENANT`
//!   must agree with it;
//! - `/pages/...` never returns the SPA;
//! - a reader with no session is sent to the login page with `next`;
//! - a repository site path reads no credential;
//! - `docs`, `adrs` and `repos` are refused as a team slug, and as the slug of
//!   a team page or folder at the top of the tree, not deeper.
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
use tower::ServiceExt as _;

use common::{AUDIENCE, ISSUER, base_config, recreate_scratch_db, with_database};
use kairos_db::local_auth as db_local_auth;
use kairos_db::models::{NewOrganizationMember, NewUser, OrgRole, User};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::config::AppConfig;
use kairos_server::local_auth::hash_password;
use kairos_server::middleware::auth::Authenticator;
use uuid::Uuid;

const PASSWORD: &str = "correct-horse-battery-staple";
const SCRATCH_DB: &str = "kairos_pages_router_t0403_test";
/// The bare base domain: no subdomain, so the host fixes no tenant.
const HOST: &str = "kairos.test";

struct Reply {
    status: StatusCode,
    location: Option<String>,
    content_type: Option<String>,
    body: String,
}

async fn get(router: &Router, uri: &str, host: &str, cookie: Option<&str>) -> Reply {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("host", host);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("kairos_session={cookie}"));
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().expect("ascii").to_string())
    };
    let status = response.status();
    let location = header("location");
    let content_type = header("content-type");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Reply {
        status,
        location,
        content_type,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

async fn api(
    router: &Router,
    method: Method,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST)
        .header("x-tenant", "acme")
        .header("authorization", format!("Bearer {token}"));
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        None => {
            builder = builder.header("content-length", "0");
            builder.body(Body::empty())
        }
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn add_user(conn: &mut PgConnection, org_id: Option<Uuid>, email: &str) {
    let user: User = diesel::insert_into(users::table)
        .values(NewUser {
            external_id: format!("local:{email}"),
            user_name: email.into(),
            email: email.into(),
            display_name: email.into(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("insert user");
    if let Some(org_id) = org_id {
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: user.id,
                role: OrgRole::Admin,
            })
            .execute(conn)
            .expect("membership");
    }
    let hash = hash_password(PASSWORD).expect("hash");
    db_local_auth::set_password(conn, user.id, &hash).expect("password");
}

fn fixture() -> common::ScratchUrl {
    let admin = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provision acme");
    provision_tenant(&mut conn, "other", "Other Inc").expect("provision other");
    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org id");
    add_user(&mut conn, Some(org_id), "robin@example.test");
    add_user(&mut conn, None, "sam@example.test");
    common::ScratchUrl::new(admin, scratch_url)
}

async fn router_with(scratch_url: &str, edit: impl FnOnce(&mut AppConfig)) -> Router {
    let pool = TenantPool::new(scratch_url, 4).await.expect("pool");
    let auth = Arc::new(Authenticator::with_static_keys(ISSUER, AUDIENCE, []));
    let mut config = base_config(scratch_url);
    config.local_auth = true;
    edit(&mut config);
    app::router(app::state_with(config, pool, auth))
}

async fn login(router: &Router, email: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": email, "password": PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    body["token"].as_str().expect("token").to_string()
}

fn assert_plain_404(reply: &Reply, what: &str) {
    assert_eq!(
        reply.status,
        StatusCode::NOT_FOUND,
        "{what}: {}",
        reply.body
    );
    assert_eq!(
        reply.content_type.as_deref(),
        Some("text/html; charset=utf-8"),
        "{what}"
    );
    assert!(reply.body.contains("Not found"), "{what}: {}", reply.body);
}

#[tokio::test]
async fn the_pages_router_against_live_stack() {
    let scratch_url = fixture();
    let router = router_with(&scratch_url, |_| {}).await;
    let robin = login(&router, "robin@example.test").await;
    let sam = login(&router, "sam@example.test").await;

    // --- no session: to the login page, with next ------------------------
    let reply = get(&router, "/pages/acme/", HOST, None).await;
    assert_eq!(reply.status, StatusCode::FOUND, "{}", reply.body);
    assert_eq!(reply.location.as_deref(), Some("/login?next=/pages/acme/"));
    let reply = get(
        &router,
        "/pages/acme/kairos/docs/KAIROS-D-0001/?x=1",
        HOST,
        None,
    )
    .await;
    assert_eq!(
        reply.location.as_deref(),
        Some("/login?next=/pages/acme/kairos/docs/KAIROS-D-0001/%3Fx%3D1")
    );
    // An expired or unknown session is the same.
    let reply = get(&router, "/pages/acme/", HOST, Some("kairos_ss_nonsense")).await;
    assert_eq!(reply.status, StatusCode::FOUND, "{}", reply.body);

    // --- with the session: the org landing ---------------------------------
    let reply = get(&router, "/pages/acme/", HOST, Some(&robin)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("<h1>acme</h1>"), "{}", reply.body);

    // --- a path without its trailing slash redirects -----------------------
    let reply = get(&router, "/pages/acme", HOST, Some(&robin)).await;
    assert_eq!(reply.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(reply.location.as_deref(), Some("/pages/acme/"));

    // --- /pages never returns the SPA ---------------------------------------
    for path in ["/pages/acme/nothing/here/", "/pages/", "/pages"] {
        let reply = get(&router, path, HOST, Some(&robin)).await;
        assert_plain_404(&reply, path);
    }
    let reply = get(&router, "/pages/_/nothing.js", HOST, None).await;
    assert_plain_404(&reply, "an unknown asset");
    let reply = get(&router, "/pages/_/pages.css", HOST, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.content_type.as_deref(),
        Some("text/css; charset=utf-8")
    );

    // --- a repository site reads no credential (KAIROS-T-0407 serves it) ----
    let reply = get(&router, "/pages/acme/kairos/repos/kairos/", HOST, None).await;
    assert_plain_404(&reply, "a repository site with no session");

    // --- a person who is not a member: no access ---------------------------
    let reply = get(&router, "/pages/acme/", HOST, Some(&sam)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert!(reply.body.contains("No access"), "{}", reply.body);

    // --- an unknown tenant and a bad slug ------------------------------------
    let reply = get(&router, "/pages/nobody/", HOST, Some(&robin)).await;
    assert_plain_404(&reply, "an unknown tenant");
    let reply = get(&router, "/pages/NOT_A_SLUG/", HOST, Some(&robin)).await;
    assert_plain_404(&reply, "a bad tenant slug");

    // --- the host fixes a tenant: the path must agree -----------------------
    let reply = get(&router, "/pages/acme/", "other.kairos.test", Some(&robin)).await;
    assert_plain_404(&reply, "a host of a different tenant");
    let reply = get(
        &router,
        "/pages/acme/kairos/repos/kairos/",
        "other.kairos.test",
        None,
    )
    .await;
    assert_plain_404(
        &reply,
        "a repository site under a host of a different tenant",
    );
    let reply = get(&router, "/pages/acme/", "acme.kairos.test", Some(&robin)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);

    // --- KAIROS_SINGLE_TENANT fixes a tenant: the path must agree -----------
    let single = router_with(&scratch_url, |config| {
        config.single_tenant = Some("other".to_string());
    })
    .await;
    let robin_single = login(&single, "robin@example.test").await;
    let reply = get(&single, "/pages/acme/", HOST, Some(&robin_single)).await;
    assert_plain_404(&reply, "a path that is not the single tenant");

    // --- reserved slugs -------------------------------------------------------
    for slug in ["docs", "adrs", "repos"] {
        let (status, body) = api(
            &router,
            Method::POST,
            "/api/teams",
            &robin,
            Some(json!({"name": "Reserved", "slug": slug, "code_prefix": "RES"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{slug}: {body}");
        assert_eq!(body["error"]["details"]["field"], "slug", "{body}");
    }
    let (status, team) = api(
        &router,
        Method::POST,
        "/api/teams",
        &robin,
        Some(json!({"name": "Platform", "slug": "platform", "code_prefix": "PLATFORM"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{team}");
    let team_id = team["id"].as_str().expect("team id").to_string();
    let (status, body) = api(
        &router,
        Method::PATCH,
        &format!("/api/teams/{team_id}"),
        &robin,
        Some(json!({"slug": "repos"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "rename: {body}");

    let pages = format!("/api/teams/{team_id}/pages");
    for slug in ["docs", "adrs", "repos"] {
        let (status, body) = api(
            &router,
            Method::POST,
            &pages,
            &robin,
            Some(json!({"kind": "folder", "slug": slug, "title": "X"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{slug}: {body}");
        assert_eq!(body["error"]["details"]["field"], "slug", "{body}");
    }
    let (status, folder) = api(
        &router,
        Method::POST,
        &pages,
        &robin,
        Some(json!({"kind": "folder", "slug": "runbooks", "title": "Runbooks"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{folder}");
    let folder_id = folder["id"].as_str().expect("folder id");
    let (status, nested) = api(
        &router,
        Method::POST,
        &pages,
        &robin,
        Some(json!({"kind": "page", "slug": "docs", "title": "Docs", "parent_id": folder_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "a nested docs page: {nested}");
    let nested_id = nested["id"].as_str().expect("page id");
    let (status, body) = api(
        &router,
        Method::PATCH,
        &format!("{pages}/{nested_id}"),
        &robin,
        Some(json!({"move_to_root": true})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a move to the top: {body}"
    );
    let (status, top) = api(
        &router,
        Method::POST,
        &pages,
        &robin,
        Some(json!({"kind": "page", "slug": "guide", "title": "Guide"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{top}");
    let top_id = top["id"].as_str().expect("page id");
    let (status, body) = api(
        &router,
        Method::PATCH,
        &format!("{pages}/{top_id}"),
        &robin,
        Some(json!({"slug": "adrs"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a rename at the top: {body}"
    );
}
