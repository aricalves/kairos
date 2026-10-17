//! The auth cache (KAIROS-T-0609): a write that makes an entry wrong takes
//! effect at the next request.
//!
//! 1. The cache serves: a key, a session and a role that change in the
//!    database behind the server are still accepted, until the entry is
//!    dropped or expires. So steps 2 to 5 test the drops, not a cold read.
//! 2. A revoked key is refused at the next request.
//! 3. A removed member is refused at the next request.
//! 4. A demoted admin loses the admin route at the next request.
//! 5. A logout ends the session at the next request.
//! 6. With `auth_cache_secs = 0` nothing is cached: a database-side revoke
//!    is refused at once.
//!
//! Runs against the dev compose Postgres (`angreal services up`). No Dex:
//! the people log in with local passwords. Owns the scratch database
//! `kairos_auth_cache_t0609_test`.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, error_code, recreate_scratch_db, request, with_database,
};
use kairos_db::api_keys;
use kairos_db::local_auth as db_local_auth;
use kairos_db::models::{NewOrganizationMember, NewUser, OrgRole, User};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::local_auth::{hash_password, hash_session_token};
use kairos_server::middleware::auth::Authenticator;
use kairos_server::service_accounts::auth::hash_key;

const SCRATCH_DB: &str = "kairos_auth_cache_t0609_test";
const PASSWORD: &str = "correct-horse-battery-staple";
const TENANT: (&str, &str) = ("x-tenant", "acme");

fn make_person(conn: &mut PgConnection, org_id: Uuid, email: &str, role: OrgRole) -> Uuid {
    let user: User = diesel::insert_into(users::table)
        .values(NewUser {
            external_id: format!("oidc:{email}"),
            user_name: email.to_string(),
            email: email.to_string(),
            display_name: email.to_string(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("insert user");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: user.id,
            role,
        })
        .execute(conn)
        .expect("insert membership");
    let hash = hash_password(PASSWORD).expect("hash");
    db_local_auth::set_password(conn, user.id, &hash).expect("set password");
    user.id
}

async fn login(router: &Router, email: &str) -> String {
    let (status, body) = request(
        router,
        Method::POST,
        "/api/login",
        None,
        &[],
        Some(json!({ "email": email, "password": PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login {email}: {body}");
    body["token"].as_str().expect("a token").to_string()
}

async fn get(router: &Router, token: &str, uri: &str) -> (StatusCode, Value) {
    request(router, Method::GET, uri, Some(token), &[TENANT], None).await
}

async fn whoami(router: &Router, token: &str) -> StatusCode {
    get(router, token, "/api/whoami").await.0
}

/// Create a service account and one key for it as `admin`. Returns the
/// account id, the key id and the secret.
async fn service_key(router: &Router, admin: &str, name: &str) -> (String, String, String) {
    let (status, sa) = request(
        router,
        Method::POST,
        "/api/service-accounts",
        Some(admin),
        &[TENANT],
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sa}");
    let sa_id = sa["id"].as_str().expect("id").to_string();
    let (status, key) = request(
        router,
        Method::POST,
        &format!("/api/service-accounts/{sa_id}/keys"),
        Some(admin),
        &[TENANT],
        Some(json!({ "name": name, "no_expiry": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    (
        sa_id,
        key["id"].as_str().expect("id").to_string(),
        key["key"].as_str().expect("key").to_string(),
    )
}

fn set_role(conn: &mut PgConnection, org_id: Uuid, user_id: Uuid, role: OrgRole) {
    diesel::update(
        organization_members::table
            .filter(organization_members::organization_id.eq(org_id))
            .filter(organization_members::user_id.eq(user_id)),
    )
    .set(organization_members::role.eq(role))
    .execute(conn)
    .expect("set role");
}

fn revoke_in_db(conn: &mut PgConnection, secret: &str) {
    let row = api_keys::find_by_hash(conn, &hash_key(secret))
        .expect("find key")
        .expect("key row");
    api_keys::revoke_key(conn, row.id).expect("revoke in the database");
}

#[tokio::test]
async fn each_write_that_makes_an_entry_wrong_takes_effect_at_the_next_request() {
    let _admin_conn = recreate_scratch_db(SCRATCH_DB);
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

    let _robin = make_person(&mut conn, org_id, "robin@example.test", OrgRole::Admin);
    let sam = make_person(&mut conn, org_id, "sam@example.test", OrgRole::Member);
    let jamie = make_person(&mut conn, org_id, "jamie@example.test", OrgRole::Admin);
    let casey = make_person(&mut conn, org_id, "casey@example.test", OrgRole::Admin);

    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(Authenticator::with_static_keys(ISSUER, AUDIENCE, []));
    let mut config = base_config(&scratch_url);
    config.local_auth = true;
    config.auth_cache_secs = 60;
    let router = app::router(app::state_with(config, pool, auth));

    let robin_token = login(&router, "robin@example.test").await;
    let sam_token = login(&router, "sam@example.test").await;
    let jamie_token = login(&router, "jamie@example.test").await;
    let casey_token = login(&router, "casey@example.test").await;
    let (sa_id, key_id, key) = service_key(&router, &robin_token, "ci").await;
    let (_, _, probe_key) = service_key(&router, &robin_token, "probe").await;
    // Gated on the role of the tenant layer, the role that the cache holds.
    let admin_route = "/api/org/read-tokens";

    // 1. The cache serves. Each entry is read once, then changed behind the
    //    server; the next request still sees the cached entry.
    assert_eq!(whoami(&router, &probe_key).await, StatusCode::OK);
    revoke_in_db(&mut conn, &probe_key);
    assert_eq!(
        whoami(&router, &probe_key).await,
        StatusCode::OK,
        "a key accepted in the last minute is served from the cache"
    );

    assert_eq!(
        get(&router, &casey_token, admin_route).await.0,
        StatusCode::OK
    );
    set_role(&mut conn, org_id, casey, OrgRole::Member);
    assert_eq!(
        get(&router, &casey_token, admin_route).await.0,
        StatusCode::OK,
        "a role read in the last minute is served from the cache"
    );

    assert_eq!(whoami(&router, &casey_token).await, StatusCode::OK);
    db_local_auth::revoke_session_by_hash(&mut conn, &hash_session_token(&casey_token))
        .expect("revoke the session in the database");
    assert_eq!(
        whoami(&router, &casey_token).await,
        StatusCode::OK,
        "a session accepted in the last minute is served from the cache"
    );

    // 2. A revoked key is refused at the next request.
    assert_eq!(whoami(&router, &key).await, StatusCode::OK);
    let (status, body) = request(
        &router,
        Method::DELETE,
        &format!("/api/service-accounts/{sa_id}/keys/{key_id}"),
        Some(&robin_token),
        &[TENANT],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revoke: {body}");
    let (status, body) = get(&router, &key, "/api/whoami").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked key: {body}");
    assert_eq!(error_code(&body), "UNAUTHORIZED");

    // 3. A removed member is refused at the next request.
    assert_eq!(whoami(&router, &sam_token).await, StatusCode::OK);
    let (status, body) = request(
        &router,
        Method::DELETE,
        &format!("/api/members/{sam}"),
        Some(&robin_token),
        &[TENANT],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "remove: {body}");
    let (status, body) = get(&router, &sam_token, "/api/whoami").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "removed member: {body}");
    assert_eq!(error_code(&body), "MEMBERSHIP_REQUIRED");

    // 4. A demoted admin loses the admin route at the next request.
    assert_eq!(
        get(&router, &jamie_token, admin_route).await.0,
        StatusCode::OK
    );
    let (status, body) = request(
        &router,
        Method::PATCH,
        &format!("/api/members/{jamie}"),
        Some(&robin_token),
        &[TENANT],
        Some(json!({ "role": "member" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "demote: {body}");
    let (status, body) = get(&router, &jamie_token, admin_route).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "demoted admin: {body}");

    // 5. A logout ends the session at the next request.
    assert_eq!(whoami(&router, &jamie_token).await, StatusCode::OK);
    let (status, _) = request(
        &router,
        Method::POST,
        "/api/logout",
        Some(&jamie_token),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        whoami(&router, &jamie_token).await,
        StatusCode::UNAUTHORIZED,
        "a logged-out session is refused"
    );

    // 6. With the cache off, a database-side revoke is refused at once.
    let pool = TenantPool::new(&scratch_url, 2).await.expect("pool");
    let auth = Arc::new(Authenticator::with_static_keys(ISSUER, AUDIENCE, []));
    let mut config = base_config(&scratch_url);
    config.local_auth = true;
    config.auth_cache_secs = 0;
    let uncached = app::router(app::state_with(config, pool, auth));
    let (_, _, cold_key) = service_key(&uncached, &robin_token, "cold").await;
    assert_eq!(whoami(&uncached, &cold_key).await, StatusCode::OK);
    revoke_in_db(&mut conn, &cold_key);
    assert_eq!(
        whoami(&uncached, &cold_key).await,
        StatusCode::UNAUTHORIZED,
        "no cache: the revoke is seen at once"
    );
}
