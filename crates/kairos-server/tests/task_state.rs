//! KAIROS-T-0506 — `PUT /api/tasks/{short_code}/state`: a task has one
//! live state (branch, commit, pushed, PR, next step), separate from its
//! notes. A write replaces the whole state and makes no new version. The
//! task endpoints and the board items carry it. The edit rule applies.
//! The MCP tool `set_state` is in `tests/mcp.rs`.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_task_state_t0506_test`.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, request, user_token,
    with_database,
};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_task_state_t0506_test";
const TENANT: [(&str, &str); 1] = [("x-tenant", "acme")];

/// The id of the column `name` of the board `board_slug`.
fn column_id(conn: &mut PgConnection, board_slug: &str, name: &str) -> String {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Uuid)]
        id: Uuid,
    }
    let row: Row = diesel::sql_query(
        "SELECT c.id FROM org_acme.board_columns c \
         JOIN org_acme.boards b ON b.id = c.board_id \
         WHERE b.slug = $1 AND c.name = $2",
    )
    .bind::<diesel::sql_types::Text, _>(board_slug)
    .bind::<diesel::sql_types::Text, _>(name)
    .get_result(conn)
    .expect("column");
    row.id.to_string()
}

#[tokio::test]
async fn a_task_has_one_live_state_that_a_write_replaces_against_live_stack() {
    let mut admin_conn = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    let org_id: Uuid = organizations::table
        .filter(organizations::slug.eq("acme"))
        .select(organizations::id)
        .first(&mut conn)
        .expect("acme org row");

    let http = reqwest::Client::new();
    let token = user_token(&http, "svc").await;
    let bob_token = user_token(&http, "bob").await;
    let pool = TenantPool::new(&scratch_url, 8).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let router = app::router(app::state_with(
        base_config(&scratch_url),
        pool.clone(),
        auth,
    ));
    for (user, user_token, role) in [
        ("svc", &token, OrgRole::Admin),
        ("bob", &bob_token, OrgRole::Member),
    ] {
        let _ = request(
            &router,
            Method::GET,
            "/api/whoami",
            Some(user_token),
            &TENANT,
            None,
        )
        .await;
        let user_id: Uuid = users::table
            .filter(users::email.eq(format!("{user}@kairos.test")))
            .select(users::id)
            .first(&mut conn)
            .expect("user provisioned");
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let svc_name: String = users::table
        .filter(users::email.eq("svc@kairos.test"))
        .select(users::display_name)
        .first(&mut conn)
        .expect("svc display name");

    let send = |method: Method, uri: String, token: String, body: Option<Value>| {
        let router = router.clone();
        async move { request(&router, method, &uri, Some(&token), &TENANT, body).await }
    };

    let (status, team) = send(
        Method::POST,
        "/api/teams".into(),
        token.clone(),
        Some(json!({"name": "Payments", "slug": "payments", "code_prefix": "PAYMENTS"})),
    )
    .await;
    assert!(status.is_success(), "{status} {team}");
    let (status, task) = send(
        Method::POST,
        "/api/tasks".into(),
        token.clone(),
        Some(json!({
            "board_id": "payments-delivery",
            "title": "Pay",
            "content": "# Pay\n\nThe body.\n",
        })),
    )
    .await;
    assert!(status.is_success(), "{status} {task}");
    let code = task["short_code"].as_str().expect("short code").to_string();
    assert!(
        task.get("state").is_none(),
        "a new task has no state: {task}"
    );
    let state_uri = format!("/api/tasks/{code}/state");

    // --- set: each field, no new version --------------------------------------
    let (status, answer) = send(
        Method::PUT,
        state_uri.clone(),
        token.clone(),
        Some(json!({
            "branch": "pay-1",
            "commit": "abc1234",
            "pushed": true,
            "pr_url": "https://example.com/pr/12",
            "next_step": "Wait for CI"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["short_code"], code.as_str());
    assert_eq!(answer["version"], 1, "the state is not content: {answer}");
    let state = &answer["state"];
    assert_eq!(state["branch"], "pay-1", "{answer}");
    assert_eq!(state["commit"], "abc1234");
    assert_eq!(state["pushed"], true);
    assert_eq!(state["pr_url"], "https://example.com/pr/12");
    assert_eq!(state["next_step"], "Wait for CI");
    assert_eq!(state["updated_by_name"], svc_name.as_str());
    assert!(state["updated_at"].as_str().is_some_and(|at| at.len() > 10));

    // --- read: the task, the list and the board items carry it -----------------
    let (_, got) = send(
        Method::GET,
        format!("/api/tasks/{code}"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(got["state"]["branch"], "pay-1", "{got}");
    let (_, list) = send(Method::GET, "/api/tasks".into(), token.clone(), None).await;
    let listed = list["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|t| t["short_code"] == code.as_str())
        .expect("the task is listed");
    assert_eq!(listed["state"]["next_step"], "Wait for CI", "{list}");
    let (status, board) = send(
        Method::GET,
        "/api/boards/payments-delivery/items".into(),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let board_text = board.to_string();
    assert!(
        board_text.contains("\"branch\":\"pay-1\""),
        "the board items carry the state: {board_text}"
    );

    // --- replace: a field that is not sent is cleared --------------------------
    let (status, answer) = send(
        Method::PUT,
        state_uri.clone(),
        token.clone(),
        Some(json!({"next_step": " Rebase on main "})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let state = &answer["state"];
    assert!(state["branch"].is_null(), "{answer}");
    assert!(state["commit"].is_null(), "{answer}");
    assert_eq!(state["pushed"], false);
    assert!(state["pr_url"].is_null(), "{answer}");
    assert_eq!(state["next_step"], "Rebase on main", "trimmed: {answer}");

    // --- refusals: no value, an unknown field, the edit rule -------------------
    let (status, answer) = send(
        Method::PUT,
        state_uri.clone(),
        token.clone(),
        Some(json!({"branch": "  "})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "VALIDATION", "{answer}");
    let (status, answer) = send(
        Method::PUT,
        state_uri.clone(),
        token.clone(),
        Some(json!({"branch": "x", "sha": "abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("sha")),
        "the refusal names the field: {answer}"
    );
    let (status, answer) = send(
        Method::PUT,
        state_uri.clone(),
        bob_token.clone(),
        Some(json!({"next_step": "Not mine."})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    let (status, answer) = send(
        Method::PUT,
        "/api/tasks/PAYMENTS-T-9999/state".into(),
        token.clone(),
        Some(json!({"next_step": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    let (_, got) = send(
        Method::GET,
        format!("/api/tasks/{code}"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(
        got["state"]["next_step"], "Rebase on main",
        "a refused write changes nothing: {got}"
    );

    // --- the state stays when the task reaches a done column ------------------
    for column in ["Todo", "Active", "Completed"] {
        let to_column_id = column_id(&mut conn, "payments-delivery", column);
        let (status, moved) = send(
            Method::POST,
            format!("/api/tasks/{code}/transition"),
            token.clone(),
            Some(json!({"to_column_id": to_column_id})),
        )
        .await;
        assert!(status.is_success(), "{column}: {status} {moved}");
    }
    let (_, got) = send(
        Method::GET,
        format!("/api/tasks/{code}"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(got["state"]["next_step"], "Rebase on main", "{got}");

    drop(pool);
    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
