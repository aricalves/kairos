//! The flow metrics of a board (KAIROS-T-0372), from transitions at
//! known times.
//!
//! 4 tasks on a delivery board (Backlog, Todo, Blocked, Active, Completed).
//! The test makes the moves over REST, then sets the time of each history
//! row:
//!
//! - A: Active Mon 2026-09-28 09:00, Completed Wed 2026-09-30 09:00 (2 days);
//! - B: Active Tue 2026-09-29 10:00, still in Active (in progress);
//! - C: cancelled Thu 2026-10-01 10:00 (counted apart);
//! - D: Active Mon 2026-10-05 10:00, Completed Tue 2026-10-06 10:00 (1 day).
//!
//! REST `GET /api/boards/{id}/flow`, MCP `board_flow`, and the refusals.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_board_flow_t0372_test`.

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
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_org::{BoardFlowQuery, CreateTeamRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_board_flow_t0372_test";

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
                    "clientInfo": {"name": "kairos-t0372-flow", "version": "0.0.0"},
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

/// Set the time of the history row of a move of `task`.
fn set_time(conn: &mut PgConnection, task: Uuid, action: &str, details_prefix: &str, at: &str) {
    let changed = diesel::sql_query(
        "UPDATE org_acme.activity_log SET occurred_at = $1::timestamptz \
         WHERE entity_id = $2 AND action = $3 AND details LIKE $4",
    )
    .bind::<diesel::sql_types::Text, _>(at)
    .bind::<diesel::sql_types::Uuid, _>(task)
    .bind::<diesel::sql_types::Text, _>(action)
    .bind::<diesel::sql_types::Text, _>(format!("{details_prefix}%"))
    .execute(conn)
    .expect("set the time of a row");
    assert_eq!(changed, 1, "{action} {details_prefix} of {task}");
}

#[tokio::test]
async fn the_flow_of_a_board_from_transitions_at_known_times() {
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
    let column: HashMap<String, String> = detail
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    let slug = detail.board.slug.clone();

    // The tasks and their moves.
    let mut ids = HashMap::new();
    for name in ["A", "B", "C", "D"] {
        let task = admin
            .create_task(&CreateTaskRequest {
                board_id: Some(board.clone()),
                column_id: None,
                title: format!("Task {name}"),
                content: String::new(),
                task_type: None,
                work_class: None,
                team_id: None,
                repository: None,
                from_team: None,
                parent: None,
            })
            .await
            .expect("task");
        let id: Uuid = task.id.parse().expect("uuid");
        diesel::sql_query(
            "UPDATE org_acme.tasks SET created_at = '2026-09-28T08:00:00Z' WHERE id = $1",
        )
        .bind::<diesel::sql_types::Uuid, _>(id)
        .execute(&mut conn)
        .expect("created_at");
        ids.insert(name, (id, task.short_code));
    }
    let code = |name: &str| ids[name].1.clone();
    let id = |name: &str| ids[name].0;
    for (name, path) in [
        ("A", &["Todo", "Active", "Completed"][..]),
        ("B", &["Todo", "Active"][..]),
        ("D", &["Todo", "Active", "Completed"][..]),
    ] {
        for to in path {
            admin
                .transition_task(&code(name), &column[*to])
                .await
                .expect("transition");
        }
    }
    admin
        .cancel_task(&code("C"), "Not needed")
        .await
        .expect("cancel");
    for (name, details, at) in [
        ("A", "column:Backlog->Todo", "2026-09-28T08:30:00Z"),
        ("A", "column:Todo->Active", "2026-09-28T09:00:00Z"),
        ("A", "column:Active->Completed", "2026-09-30T09:00:00Z"),
        ("B", "column:Backlog->Todo", "2026-09-29T09:00:00Z"),
        ("B", "column:Todo->Active", "2026-09-29T10:00:00Z"),
        ("D", "column:Backlog->Todo", "2026-10-05T09:00:00Z"),
        ("D", "column:Todo->Active", "2026-10-05T10:00:00Z"),
        ("D", "column:Active->Completed", "2026-10-06T10:00:00Z"),
    ] {
        set_time(&mut conn, id(name), "transition", details, at);
    }
    set_time(
        &mut conn,
        id("C"),
        "cancel",
        "column:Backlog->Completed",
        "2026-10-01T10:00:00Z",
    );
    diesel::sql_query(
        "UPDATE org_acme.task_cancellations SET cancelled_at = '2026-10-01T10:00:00Z' \
         WHERE task_id = $1",
    )
    .bind::<diesel::sql_types::Uuid, _>(id("C"))
    .execute(&mut conn)
    .expect("cancelled_at");

    // REST, by a member with no capability on the board.
    let flow = bob
        .board_flow(
            &slug,
            &BoardFlowQuery {
                from: Some("2026-09-28".into()),
                to: Some("2026-10-11".into()),
            },
        )
        .await
        .expect("the flow");
    assert_eq!(flow.start_columns, vec!["Active"]);
    assert_eq!(flow.done_columns, vec!["Completed"]);
    assert_eq!(flow.done_count, 2);
    assert_eq!(flow.cancelled_count, 1);
    assert_eq!(flow.done_without_start, 0);
    assert_eq!(flow.cycle_time.count, 2);
    assert_eq!(flow.cycle_time.mean_days, Some(1.5));
    assert_eq!(flow.cycle_time.median_days, Some(1.0));
    assert_eq!(flow.cycle_time.p85_days, Some(2.0));
    let cycle: Vec<(String, f64)> = flow
        .cycle_time
        .items
        .iter()
        .map(|c| (c.short_code.clone(), c.days))
        .collect();
    assert_eq!(cycle, vec![(code("A"), 2.0), (code("D"), 1.0)]);
    let weeks: Vec<(String, i64, i64)> = flow
        .throughput
        .iter()
        .map(|w| (w.week_start.clone(), w.done, w.cancelled))
        .collect();
    assert_eq!(
        weeks,
        vec![
            ("2026-09-28".to_string(), 1, 1),
            ("2026-10-05".to_string(), 1, 0),
        ]
    );
    assert_eq!(flow.ageing.len(), 1);
    assert_eq!(flow.ageing[0].short_code, code("B"));
    assert_eq!(flow.ageing[0].column, "Active");
    assert_eq!(flow.ageing[0].started_at, "2026-09-29T10:00:00Z");
    let cfd = &flow.cumulative_flow;
    assert_eq!(
        cfd.columns,
        vec!["Backlog", "Todo", "Blocked", "Active", "Completed"]
    );
    assert_eq!(cfd.days.len(), 14);
    let day = |date: &str| {
        cfd.days
            .iter()
            .find(|d| d.date == date)
            .map(|d| d.counts.clone())
            .unwrap()
    };
    assert_eq!(day("2026-09-28"), vec![3, 0, 0, 1, 0]);
    assert_eq!(day("2026-09-30"), vec![2, 0, 0, 1, 1]);
    assert_eq!(day("2026-10-01"), vec![1, 0, 0, 1, 2]);
    assert_eq!(day("2026-10-06"), vec![0, 0, 0, 1, 3]);

    // A range of the first week only.
    let first = bob
        .board_flow(
            &slug,
            &BoardFlowQuery {
                from: Some("2026-09-28".into()),
                to: Some("2026-10-04".into()),
            },
        )
        .await
        .expect("the first week");
    assert_eq!((first.done_count, first.cancelled_count), (1, 1));
    assert_eq!(first.throughput.len(), 1);

    // The refusals.
    for query in [
        "from=2026-10-12&to=2026-10-11",
        "from=2025-01-01&to=2026-10-11",
        "from=10%2F01%2F2026",
    ] {
        let (status, body) = bob
            .raw_request(
                Method::GET,
                &format!("/api/boards/{slug}/flow?{query}"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(status, 422, "{query}: {body}");
    }
    let (status, body) = bob
        .raw_request(
            Method::GET,
            &format!("/api/boards/{slug}/flow?since=1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(status, 400, "an unknown parameter: {body}");
    let (status, _) = bob
        .raw_request(Method::GET, "/api/boards/no-such-board/flow", None)
        .await
        .unwrap();
    assert_eq!(status, 404);

    // MCP.
    let mut mcp = Mcp::connect(&server.base_url, &bob_token).await;
    let text = mcp
        .call_ok(
            "board_flow",
            json!({"board": slug, "from": "2026-09-28", "to": "2026-10-11"}),
        )
        .await;
    for needed in [
        "Start columns: Active. Done columns: Completed.",
        "Done: 2. Cancelled: 1 (counted apart).",
        "count 2, mean 1.5, median 1, 85th percentile 2",
        "- week of 2026-09-28: done 1, cancelled 1",
        "date | Backlog | Todo | Blocked | Active | Completed",
        "2026-10-06 | 0 | 0 | 0 | 1 | 3",
    ] {
        assert!(text.contains(needed), "{needed:?} not in {text}");
    }
    assert!(text.contains(&code("B")), "{text}");
}
