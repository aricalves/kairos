//! KAIROS-T-0557 — `get_history` ends with the activity of the item, and
//! `GET /api/{entity_type}/{short_code}/activity` gives the same rows.
//!
//! 1. A task moves to Todo and to Active (the move gives the claim), the
//!    claim goes to bob and is released, the lane changes, a metadata value
//!    is set (MCP) and cleared (REST), the state is written, and a different
//!    task gets a `blocks` link to it. `get_history` lists each, newest
//!    first, with the names of the people, not their ids.
//! 2. `limit` caps the activity too, and a line says how many older rows
//!    the list does not show.
//! 3. The REST list has the same text, the name of each actor, a total, and
//!    pages with `limit` and `offset`. An unknown query parameter is refused.
//!    The version list has the name of each editor.
//! 4. The link is in the activity of the other task too.
//! 5. An item with no activity rows says so.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_item_activity_t0557_test`.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, request, spawn_server,
    user_token, with_database,
};
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_item_activity_t0557_test";
const TENANT: [(&str, &str); 1] = [("x-tenant", "acme")];

fn user(conn: &mut PgConnection, email: &str) -> (Uuid, String) {
    users::table
        .filter(users::email.eq(email))
        .select((users::id, users::display_name))
        .first(conn)
        .expect("user row")
}

/// Minimal MCP session over HTTP (initialize → initialized → tools/call);
/// the same shape as the one in tests/rename_on_move.rs.
struct McpSession {
    http: reqwest::Client,
    url: String,
    token: String,
    next_id: u64,
}

impl McpSession {
    async fn open(base_url: &str, token: &str) -> Self {
        let http = reqwest::Client::new();
        let url = format!("{base_url}/mcp");
        let response = http
            .post(&url)
            .bearer_auth(token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "kairos-t0557-test", "version": "0.0.0"},
                },
            }))
            .send()
            .await
            .expect("initialize");
        assert_eq!(response.status(), 200, "initialize failed");
        let session = Self {
            http,
            url,
            token: token.to_string(),
            next_id: 0,
        };
        let response = session
            .http
            .post(&session.url)
            .bearer_auth(&session.token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .expect("initialized");
        assert_eq!(response.status(), 202, "initialized notify");
        session
    }

    async fn call_ok(&mut self, tool: &str, arguments: Value) -> String {
        self.next_id += 1;
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .send()
            .await
            .expect("tools/call");
        assert_eq!(response.status(), 200, "tools/call HTTP status");
        let body = response.text().await.expect("body");
        let message: Value = serde_json::from_str::<Value>(&body)
            .ok()
            .filter(|v| v.get("jsonrpc").is_some())
            .or_else(|| {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                    .find(|v| v.get("jsonrpc").is_some())
            })
            .unwrap_or_else(|| panic!("no JSON-RPC message in response body: {body:?}"));
        let result = &message["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content: {message}"))
            .to_string();
        assert!(
            !result["isError"].as_bool().unwrap_or(false),
            "{tool} unexpectedly errored: {text}"
        );
        text
    }
}

/// The lines of the section `## Activity` of a `get_history` text.
fn activity_lines(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|line| *line != "## Activity")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The position of the first line that contains `needle`.
fn position(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line has {needle:?}:\n{}", lines.join("\n")))
}

#[tokio::test]
async fn get_history_lists_the_activity_of_the_item_newest_first() {
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
    let svc_token = user_token(&http, "svc").await;
    let bob_token = user_token(&http, "bob").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let router = app::router(app::state_with(base_config(&scratch_url), pool, auth));
    let server = spawn_server(router.clone()).await;
    let admin = server.client(&svc_token, "acme");
    let bob = server.client(&bob_token, "acme");
    for client in [&admin, &bob] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("bob@kairos.test", OrgRole::Member),
    ] {
        let (user_id, _) = user(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let (_, svc_name) = user(&mut conn, "svc@kairos.test");
    let (bob_id, bob_name) = user(&mut conn, "bob@kairos.test");
    let team = admin
        .create_team(&CreateTeamRequest {
            name: "Web".into(),
            slug: "web".into(),
            code_prefix: "WEB".into(),
            team_type: None,
        })
        .await
        .expect("team");
    admin
        .add_team_member(
            &team.id,
            &AddTeamMemberRequest {
                user_id: bob_id.to_string(),
            },
        )
        .await
        .expect("bob joins the team");
    let board = team.delivery_board_id.clone().expect("delivery board");
    let new_task = |title: &str| CreateTaskRequest {
        board_id: Some(board.clone()),
        column_id: None,
        title: title.into(),
        content: "one".into(),
        task_type: None,
        work_class: None,
        team_id: None,
        repository: None,
        from_team: None,
    };
    let task = admin.create_task(&new_task("Worked")).await.expect("task");
    let blocker = admin
        .create_task(&new_task("Blocker"))
        .await
        .expect("blocker");
    let quiet = admin.create_task(&new_task("Quiet")).await.expect("quiet");
    let code = task.short_code.clone();
    let other_lane = if task.work_class == "planned" {
        "support"
    } else {
        "planned"
    };

    // --- 1. the work, through MCP and REST -----------------------------------
    let mut mcp = McpSession::open(&server.base_url, &svc_token).await;
    for column in ["Todo", "Active"] {
        mcp.call_ok(
            "transition_item",
            json!({"short_code": code, "to_column": column}),
        )
        .await;
    }
    mcp.call_ok(
        "hand_off_item",
        json!({"short_code": code, "to": "bob@kairos.test"}),
    )
    .await;
    mcp.call_ok("release_item", json!({"short_code": code})).await;
    mcp.call_ok(
        "set_work_class",
        json!({"short_code": code, "work_class": other_lane}),
    )
    .await;
    mcp.call_ok(
        "set_metadata",
        json!({"short_code": code, "values": {"priority": "high"}}),
    )
    .await;
    // The same value again changes nothing and writes no row.
    mcp.call_ok(
        "set_metadata",
        json!({"short_code": code, "values": {"priority": "high"}}),
    )
    .await;
    let (status, body) = request(
        &router,
        Method::PATCH,
        &format!("/api/tasks/{code}/metadata"),
        Some(&svc_token),
        &TENANT,
        Some(json!({"values": {"priority": null}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    mcp.call_ok(
        "set_state",
        json!({"short_code": code, "branch": "web-1", "commit": "abc1234"}),
    )
    .await;
    mcp.call_ok(
        "link_items",
        json!({"source": blocker.short_code, "target": code, "relationship": "blocks"}),
    )
    .await;

    let text = mcp
        .call_ok("get_history", json!({"short_code": code}))
        .await;
    let lines = activity_lines(&text);
    let newest_first = [
        format!("link added: blocks {} -> {code}", blocker.short_code),
        "state changed: branch, commit".to_string(),
        "metadata priority: cleared".to_string(),
        "metadata priority: high".to_string(),
        format!("lane: {} -> {other_lane}", task.work_class),
        format!("claim released: {bob_name}"),
        format!("claim handed off: {svc_name} -> {bob_name}"),
        "moved from Todo to Active".to_string(),
        "moved from Backlog to Todo".to_string(),
        format!("created as {code}"),
    ];
    let positions: Vec<usize> = newest_first
        .iter()
        .map(|needle| position(&lines, needle))
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "newest first: {positions:?}\n{text}"
    );
    // The move to Active and the claim are in one transaction: same time.
    let claim = position(&lines, &format!("claim taken: {svc_name}"));
    assert!(claim > positions[6] && claim < positions[8], "{text}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("metadata priority"))
            .count(),
        2,
        "the same value again writes no row:\n{text}"
    );
    assert!(
        lines[0].ends_with(&format!(" by {svc_name}")) && lines[0].starts_with("- link added"),
        "each line says who: {text}"
    );
    assert!(
        !text.contains(&bob_id.to_string()),
        "the names come from the directory, not raw ids:\n{text}"
    );
    assert_eq!(lines.len(), 11, "each row once, no more line:\n{text}");

    // --- 2. the limit caps the activity too ------------------------------------
    let text = mcp
        .call_ok("get_history", json!({"short_code": code, "limit": 3}))
        .await;
    let lines = activity_lines(&text);
    assert_eq!(lines.len(), 4, "{text}");
    assert!(lines[0].contains("link added"), "{text}");
    assert_eq!(
        lines[3], "- 8 older rows are not shown. Send a larger `limit` to see them.",
        "{text}"
    );

    // --- 3. REST: the same rows, with names and pages ----------------------------
    let (status, page) = request(
        &router,
        Method::GET,
        &format!("/api/tasks/{code}/activity"),
        Some(&svc_token),
        &TENANT,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 11, "{page}");
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 11, "{page}");
    assert_eq!(
        items[0]["text"],
        format!("link added: blocks {} -> {code}", blocker.short_code)
    );
    assert_eq!(items[0]["action"], "relationship_add");
    assert_eq!(items[0]["actor_name"], svc_name.as_str());
    assert!(
        items
            .iter()
            .any(|row| row["text"] == format!("claim released: {bob_name}")),
        "{page}"
    );
    let (status, second) = request(
        &router,
        Method::GET,
        &format!("/api/tasks/{code}/activity?limit=2&offset=1"),
        Some(&svc_token),
        &TENANT,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(second["items"][0]["id"], items[1]["id"]);
    assert_eq!(second["items"][1]["id"], items[2]["id"]);
    assert_eq!(second["total"], 11);
    let (status, refused) = request(
        &router,
        Method::GET,
        &format!("/api/tasks/{code}/activity?kind=claim"),
        Some(&svc_token),
        &TENANT,
        None,
    )
    .await;
    assert!(status.is_client_error(), "{status} {refused}");
    assert!(refused.to_string().contains("kind"), "{refused}");
    let versions = bob
        .history(kairos_client::EntityKind::Task, &code, None, None)
        .await
        .expect("versions");
    assert_eq!(
        versions.items[0].edited_by_name.as_deref(),
        Some(svc_name.as_str())
    );
    let activity = bob
        .item_activity(kairos_client::EntityKind::Task, &code, Some(1), None)
        .await
        .expect("the client reads the activity");
    assert_eq!(activity.total, 11);
    assert_eq!(activity.items[0].text, items[0]["text"].as_str().unwrap());

    // --- 4. the link is in the activity of the other task ------------------------
    let text = mcp
        .call_ok("get_history", json!({"short_code": blocker.short_code}))
        .await;
    let lines = activity_lines(&text);
    assert!(
        lines[0].starts_with(&format!(
            "- link added: blocks {} -> {code} — ",
            blocker.short_code
        )),
        "{text}"
    );

    // --- 5. an item with no activity rows --------------------------------------------
    let quiet_id: Uuid = quiet.id.parse().expect("uuid");
    diesel::sql_query("DELETE FROM org_acme.activity_log WHERE entity_id = $1")
        .bind::<diesel::sql_types::Uuid, _>(quiet_id)
        .execute(&mut conn)
        .expect("removing the rows of the quiet task");
    let text = mcp
        .call_ok("get_history", json!({"short_code": quiet.short_code}))
        .await;
    assert_eq!(
        activity_lines(&text),
        vec!["No activity is recorded for this item.".to_string()],
        "{text}"
    );
    let (_, page) = request(
        &router,
        Method::GET,
        &format!("/api/tasks/{}/activity", quiet.short_code),
        Some(&svc_token),
        &TENANT,
        None,
    )
    .await;
    assert_eq!(page["total"], 0, "{page}");

    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
