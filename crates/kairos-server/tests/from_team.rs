//! The sending team of a task (KAIROS-T-0426, Dylan 2026-10-14): `from_team`
//! on create. When it is not the team of the target board, the task is a
//! request whoever sends it, an org admin too: entry column, support lane;
//! `planned` and a non-entry column are refused. With no `from_team`, or
//! with the team of the board, the rule of COLLIERY-A-0023 stays. REST and
//! MCP.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_from_team_t0426_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::Error;
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_org::CreateTeamRequest;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_from_team_t0426_test";

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
                    "clientInfo": {"name": "kairos-t0426-from-team", "version": "0.0.0"},
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

    /// The text of the tool, and whether it is a tool error.
    async fn call(&mut self, tool: &str, arguments: Value) -> (String, bool) {
        self.next_id += 1;
        let (status, body) = self
            .post(json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .await;
        assert!(status.is_success(), "{tool}: {body}");
        let result = rpc_message(&body)["result"].clone();
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} gave no text: {result}"))
            .to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }
}

impl Mcp {
    async fn ok(&mut self, tool: &str, arguments: Value) -> String {
        let (text, error) = self.call(tool, arguments).await;
        assert!(!error, "{tool}: {text}");
        text
    }

    async fn refused(&mut self, tool: &str, arguments: Value, code: &str) -> String {
        let (text, error) = self.call(tool, arguments).await;
        assert!(
            error && text.contains(code),
            "{tool}: expected {code}, got {text}"
        );
        text
    }
}

fn task(board: &str, title: &str, from_team: Option<&str>) -> CreateTaskRequest {
    CreateTaskRequest {
        board_id: Some(board.into()),
        column_id: None,
        title: title.into(),
        content: String::new(),
        task_type: None,
        work_class: None,
        team_id: None,
        repository: None,
        from_team: from_team.map(str::to_string),
        parent: None,
    }
}

fn validation(result: Result<impl std::fmt::Debug, Error>, field: &str) {
    let err = result.expect_err("refused");
    assert!(matches!(err, Error::Validation { .. }), "{err:?}");
    assert!(err.to_string().contains(field), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_from_another_team_is_a_request_whoever_sends_it() {
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
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let state = app::state_with(base_config(&scratch_url), pool, auth);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let _ = admin.whoami().await;
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: user_id(&mut conn, "svc@kairos.test"),
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("admin membership");
    for (name, slug, prefix) in [("Web", "web", "WEB"), ("Api", "api", "API")] {
        admin
            .create_team(&CreateTeamRequest {
                name: name.into(),
                slug: slug.into(),
                code_prefix: prefix.into(),
                team_type: None,
            })
            .await
            .expect("team");
    }
    let board = "web-delivery";
    let detail = admin.get_board(board).await.expect("board");
    let entry = detail.columns[0].id.clone();
    let later = detail.columns[1].id.clone();

    // Today's rule: the org admin manages the board, so its task is planned.
    let own = admin
        .create_task(&task(board, "No sending team", None))
        .await
        .expect("create");
    assert_eq!(own.work_class, "planned");
    // The team of the board sends it: the same.
    let own = admin
        .create_task(&task(board, "From web", Some("web")))
        .await
        .expect("create");
    assert_eq!(own.work_class, "planned");

    // Another team sends it: a request, though the admin manages the board.
    let request = admin
        .create_task(&task(board, "From api", Some("api")))
        .await
        .expect("create");
    assert_eq!(request.work_class, "support");
    assert_eq!(request.column_id, entry);
    // `planned` and a non-entry column are refused, and named.
    validation(
        admin
            .create_task(&CreateTaskRequest {
                work_class: Some("planned".into()),
                ..task(board, "Planned from api", Some("api"))
            })
            .await,
        "work_class",
    );
    validation(
        admin
            .create_task(&CreateTaskRequest {
                column_id: Some(later.clone()),
                ..task(board, "Later from api", Some("api"))
            })
            .await,
        "column_id",
    );
    // An unknown team is refused and named.
    validation(
        admin
            .create_task(&task(board, "From nowhere", Some("nowhere")))
            .await,
        "nowhere",
    );

    // MCP: the same rule.
    let mut mcp = Mcp::connect(&server.base_url, &svc_token).await;
    let text = mcp
        .ok(
            "create_item",
            json!({"item_type": "task", "title": "Agent request", "board": board, "from_team": "api"}),
        )
        .await;
    let code = text
        .split_whitespace()
        .find(|word| word.starts_with("WEB-T-"))
        .expect("the code")
        .trim_end_matches(['.', ':', ','])
        .to_string();
    let made = admin.get_task(&code).await.expect("the task");
    assert_eq!(made.work_class, "support", "{text}");
    mcp.refused(
        "create_item",
        json!({"item_type": "task", "title": "Planned", "board": board, "from_team": "api", "work_class": "planned"}),
        "work_class",
    )
    .await;
    mcp.refused(
        "create_item",
        json!({"item_type": "initiative", "title": "Not a task", "from_team": "api"}),
        "from_team",
    )
    .await;
}
