//! The WIP limit of a column (KAIROS-T-0371).
//!
//! 1. A person with `configure_boards` sets the limit of a column
//!    (`PATCH /api/boards/{id}/columns/{col_id}`, `wip_limit`); 0 is 422; a
//!    member with no capability gets 403; `null` removes the limit.
//! 2. A move into a column at its limit succeeds, with the header
//!    `Kairos-Warning`; a move under the limit has no warning.
//! 3. `GET /api/boards/{id}/items` gives the count of each column and marks
//!    a column over its limit; MCP `board_items` shows `[WIP n/limit, over
//!    the limit]`; MCP `transition_item` gives the warning line.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_column_wip_limit_t0371_test`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::Error;
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_org::{BoardItemsQuery, CreateTeamRequest, UpdateColumnRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_column_wip_limit_t0371_test";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

/// One JSON-RPC message of an MCP response: plain JSON or SSE `data:`.
fn rpc_message(body: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && value.get("jsonrpc").is_some()
    {
        return value;
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|value| value.get("jsonrpc").is_some())
        .unwrap_or_else(|| panic!("no JSON-RPC message in {body:?}"))
}

/// A minimal MCP session over HTTP.
struct Mcp {
    http: reqwest::Client,
    url: String,
    token: String,
    session: Option<String>,
    next_id: i64,
}

impl Mcp {
    async fn post(&mut self, body: Value) -> (reqwest::StatusCode, String) {
        let mut request = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("x-tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(session) = &self.session {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await.expect("MCP request");
        let status = response.status();
        if let Some(session) = response.headers().get("mcp-session-id") {
            self.session = Some(session.to_str().expect("session id").to_string());
        }
        (status, response.text().await.expect("MCP body"))
    }

    async fn connect(base_url: &str, token: &str) -> Mcp {
        let mut mcp = Mcp {
            http: reqwest::Client::new(),
            url: format!("{base_url}/mcp"),
            token: token.to_string(),
            session: None,
            next_id: 0,
        };
        let (status, body) = mcp
            .post(json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "kairos-t0371-wip", "version": "0.0.0"},
                },
            }))
            .await;
        assert!(status.is_success(), "initialize: {body}");
        let (status, body) = mcp
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert!(status.is_success(), "initialized: {body}");
        mcp
    }

    async fn call_ok(&mut self, tool: &str, arguments: Value) -> String {
        self.next_id += 1;
        let (status, body) = self
            .post(json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .await;
        assert!(status.is_success(), "{tool}: {body}");
        let result = rpc_message(&body)["result"].clone();
        assert!(
            !result["isError"].as_bool().unwrap_or(false),
            "{tool}: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} gave no text: {result}"))
            .to_string()
    }
}

/// A transition over raw HTTP, to read the headers: the status and the
/// values of `Kairos-Warning`.
async fn transition(
    base_url: &str,
    token: &str,
    code: &str,
    to_column_id: &str,
) -> (reqwest::StatusCode, Vec<String>) {
    let response = reqwest::Client::new()
        .post(format!("{base_url}/api/tasks/{code}/transition"))
        .bearer_auth(token)
        .header("x-tenant", "acme")
        .json(&json!({ "to_column_id": to_column_id }))
        .send()
        .await
        .expect("POST transition");
    let status = response.status();
    let warnings = response
        .headers()
        .get_all("Kairos-Warning")
        .iter()
        .map(|value| value.to_str().expect("ASCII").to_string())
        .collect();
    (status, warnings)
}

fn wip_request(limit: Option<i32>) -> UpdateColumnRequest {
    UpdateColumnRequest {
        name: None,
        position: None,
        is_done: None,
        claims: None,
        wip_limit: Some(limit),
    }
}

#[tokio::test]
async fn a_column_has_a_wip_limit_and_a_move_over_it_warns() {
    let _admin_conn = recreate_scratch_db(SCRATCH_DB);
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
    let server = spawn_server(app::router(app::state_with(
        base_config(&scratch_url),
        pool,
        auth,
    )))
    .await;
    let admin = server.client(&svc_token, "acme");
    let bob = server.client(&bob_token, "acme");
    let _ = admin.whoami().await;
    let _ = bob.whoami().await;
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("bob@kairos.test", OrgRole::Member),
    ] {
        let user_id = user_id(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let team = admin
        .create_team(&CreateTeamRequest {
            name: "Web".into(),
            slug: "web".into(),
            code_prefix: "WEB".into(),
            team_type: None,
        })
        .await
        .expect("team");
    let board = team.delivery_board_id.clone().expect("delivery board");
    let detail = admin.get_board(&board).await.expect("board");
    let columns: HashMap<String, String> = detail
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    assert!(detail.columns.iter().all(|c| c.wip_limit.is_none()));
    let active = columns["Active"].clone();
    let todo = columns["Todo"].clone();

    // 1. The limit: a refusal of 0, a 403 for bob, then 1.
    match admin
        .update_column(&board, &active, &wip_request(Some(0)))
        .await
    {
        Err(Error::Validation { message, .. }) => {
            assert!(message.contains("wip_limit"), "{message}")
        }
        other => panic!("expected 422, got {other:?}"),
    }
    match bob
        .update_column(&board, &active, &wip_request(Some(2)))
        .await
    {
        Err(Error::Forbidden { .. }) => {}
        other => panic!("expected 403, got {other:?}"),
    }
    let set = admin
        .update_column(&board, &active, &wip_request(Some(1)))
        .await
        .expect("set the limit");
    assert_eq!(set.wip_limit, Some(1));
    let detail = admin.get_board(&board).await.expect("board");
    let column = detail.columns.iter().find(|c| c.id == active).unwrap();
    assert_eq!(column.wip_limit, Some(1));

    // 2. Two tasks to Active: the second move warns.
    let mut codes = Vec::new();
    for title in ["One", "Two", "Three"] {
        let task = admin
            .create_task(&CreateTaskRequest {
                board_id: Some(board.clone()),
                column_id: None,
                title: title.into(),
                content: String::new(),
                task_type: None,
                work_class: None,
                team_id: None,
                repository: None,
                from_team: None,
            })
            .await
            .expect("task");
        let (status, _) = transition(&server.base_url, &svc_token, &task.short_code, &todo).await;
        assert!(status.is_success());
        codes.push(task.short_code);
    }
    let (status, warnings) = transition(&server.base_url, &svc_token, &codes[0], &active).await;
    assert!(status.is_success());
    assert!(
        warnings.is_empty(),
        "at the limit, no warning: {warnings:?}"
    );
    let (status, warnings) = transition(&server.base_url, &svc_token, &codes[1], &active).await;
    assert!(status.is_success(), "a move over the limit is allowed");
    assert_eq!(
        warnings,
        vec![format!(
            "{} moved into Active, over its WIP limit (2 of 1)",
            codes[1]
        )]
    );

    // 3. The board items: the count and the mark.
    let items = admin
        .board_items(&board, &BoardItemsQuery::default())
        .await
        .expect("board items");
    let group = |id: &str| items.columns.iter().find(|g| g.column.id == id).unwrap();
    assert_eq!(group(&active).item_count, 2);
    assert!(group(&active).over_wip_limit);
    assert_eq!(group(&active).column.wip_limit, Some(1));
    assert_eq!(group(&todo).item_count, 1);
    assert!(!group(&todo).over_wip_limit);
    // The count is of the column, not of the page.
    let page = admin
        .board_items(
            &board,
            &BoardItemsQuery {
                limit: Some(1),
                ..BoardItemsQuery::default()
            },
        )
        .await
        .expect("one card");
    let paged = page.columns.iter().find(|g| g.column.id == active).unwrap();
    assert_eq!(paged.item_count, 2);

    let mut mcp = Mcp::connect(&server.base_url, &svc_token).await;
    let text = mcp.call_ok("board_items", json!({"board": board})).await;
    assert!(
        text.contains("## Active (2) [WIP 2/1, over the limit]"),
        "{text}"
    );
    assert!(
        text.contains("## Todo (1)\n"),
        "no limit, no marker: {text}"
    );
    let text = mcp
        .call_ok(
            "transition_item",
            json!({"short_code": codes[2], "to_column": "Active"}),
        )
        .await;
    assert!(
        text.contains(&format!(
            "Warning: the column Active has 3 items, over its WIP limit of 1. Your move of {} is \
             recorded.",
            codes[2]
        )),
        "{text}"
    );

    // null removes the limit.
    let (status, body) = admin
        .raw_request(
            Method::PATCH,
            &format!("/api/boards/{board}/columns/{active}"),
            Some(&json!({"wip_limit": null})),
        )
        .await
        .unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["wip_limit"], Value::Null);
    let items = admin
        .board_items(&board, &BoardItemsQuery::default())
        .await
        .expect("board items");
    let active_group = items
        .columns
        .iter()
        .find(|g| g.column.id == active)
        .unwrap();
    assert!(!active_group.over_wip_limit);
    let (_, warnings) = transition(&server.base_url, &svc_token, &codes[0], &todo).await;
    assert!(warnings.is_empty());
}
