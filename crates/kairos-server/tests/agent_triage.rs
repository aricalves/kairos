//! Who decides the triage of intake (KAIROS-T-0428). An agent (an agent
//! key of a person) that moves a task of the support lane into the planned
//! lane or into another column is refused with AGENT_TRIAGE_OFF while the
//! board setting is off; it can still write its proposal as a note. A
//! person's move works either way. With the setting on, the agent's move
//! works and the activity log has a line for it. A task in the planned lane
//! is not intake. REST and MCP.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_agent_triage_t0428_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::EntityKind;
use kairos_client::Error;
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::types_service_accounts::CreateApiKeyRequest;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_agent_triage_t0428_test";

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
                    "clientInfo": {"name": "kairos-t0428-triage", "version": "0.0.0"},
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

fn support_task(board: &str, title: &str) -> CreateTaskRequest {
    CreateTaskRequest {
        board_id: Some(board.into()),
        column_id: None,
        title: title.into(),
        content: String::new(),
        task_type: Some("support".into()),
        work_class: None,
        team_id: None,
        repository: None,
        from_team: None,
        parent: None,
    }
}

fn triage_off(result: Result<impl std::fmt::Debug, Error>) {
    let err = result.expect_err("refused");
    assert_eq!(err.code(), Some("AGENT_TRIAGE_OFF"), "{err:?}");
    assert!(err.to_string().contains("append_note"), "{err}");
}

fn agent_rows(conn: &mut PgConnection) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    diesel::sql_query(
        "SELECT COUNT(*) AS n FROM org_acme.activity_log WHERE details LIKE 'agent_triage:%'",
    )
    .get_result::<Count>(conn)
    .expect("count")
    .n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_decides_intake_only_when_the_board_allows_it() {
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
    let alice_token = user_token(&http, "alice").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let state = app::state_with(base_config(&scratch_url), pool, auth);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    let _ = admin.whoami().await;
    let _ = alice.whoami().await;
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
    ] {
        let user = user_id(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: user,
                role,
            })
            .execute(&mut conn)
            .expect("membership");
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
    // alice is in the team: transition_items on its board, no configure_boards.
    admin
        .add_team_member(
            &team.id,
            &AddTeamMemberRequest {
                user_id: "alice@kairos.test".into(),
            },
        )
        .await
        .expect("alice in the team");
    let board = "web-delivery";
    let detail = admin.get_board(board).await.expect("board");
    let column = |name: &str| {
        detail
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("column {name}"))
            .id
            .clone()
    };
    let todo = column("Todo");
    let active = column("Active");
    // A second team, for a move to another board.
    let ops = admin
        .create_team(&CreateTeamRequest {
            name: "Ops".into(),
            slug: "ops".into(),
            code_prefix: "OPS".into(),
            team_type: None,
        })
        .await
        .expect("team");
    admin
        .add_team_member(
            &ops.id,
            &AddTeamMemberRequest {
                user_id: "alice@kairos.test".into(),
            },
        )
        .await
        .expect("alice in ops");
    let key = alice
        .create_agent_key(&CreateApiKeyRequest {
            name: "board session".into(),
            expires_at: None,
            no_expiry: false,
        })
        .await
        .expect("agent key")
        .key;
    let agent = server.client(&key, "acme");

    let intake = admin
        .create_task(&support_task(board, "Intake"))
        .await
        .expect("task")
        .short_code;
    let other = admin
        .create_task(&support_task(board, "Other intake"))
        .await
        .expect("task")
        .short_code;
    assert_eq!(admin.get_task(&intake).await.unwrap().work_class, "support");

    // --- Off (the default) -------------------------------------------------
    assert!(
        !agent
            .get_agent_triage(board)
            .await
            .expect("read")
            .agents_decide
    );
    triage_off(agent.set_task_work_class(&intake, "planned").await);
    triage_off(agent.transition_task(&intake, &todo).await);
    // KAIROS-T-0456: a cancel and a move to another team are triage too.
    triage_off(agent.cancel_task(&intake, "won't do").await);
    triage_off(agent.move_task(&intake, "ops-delivery").await);
    // The agent proposes with a note.
    agent
        .append_note(
            EntityKind::Task,
            &intake,
            "Proposal: planned backlog, it is a feature.",
        )
        .await
        .expect("the proposal note");
    // A person decides either way.
    alice
        .transition_task(&other, &todo)
        .await
        .expect("a person moves intake");
    // KAIROS-T-0456: back into the entry column is a triage again...
    let entry = column("Backlog");
    triage_off(agent.transition_task(&other, &entry).await);
    // ...but once a person has triaged the task out of the entry column,
    // the agent moves it on as any other task, in the support lane.
    let worked = agent
        .transition_task(&other, &active)
        .await
        .expect("the agent takes triaged support work");
    assert_eq!(worked.work_class, "support");
    // A task in the planned lane is not intake.
    let planned = admin
        .create_task(&CreateTaskRequest {
            task_type: None,
            ..support_task(board, "Planned work")
        })
        .await
        .expect("task")
        .short_code;
    agent
        .transition_task(&planned, &todo)
        .await
        .expect("an agent moves planned work");
    assert_eq!(agent_rows(&mut conn), 0);

    // MCP: the same refusal, and the setting needs configure_boards.
    let mut agent_mcp = Mcp::connect(&server.base_url, &key).await;
    agent_mcp
        .refused(
            "set_work_class",
            json!({"short_code": intake, "work_class": "planned"}),
            "AGENT_TRIAGE_OFF",
        )
        .await;
    agent_mcp
        .refused(
            "set_agent_triage",
            json!({"board": board, "agents_decide": true}),
            "FORBIDDEN",
        )
        .await;

    // --- On ------------------------------------------------------------------
    let mut admin_mcp = Mcp::connect(&server.base_url, &svc_token).await;
    let text = admin_mcp
        .ok(
            "set_agent_triage",
            json!({"board": board, "agents_decide": true}),
        )
        .await;
    assert!(text.contains("an agent decides"), "{text}");
    let text = admin_mcp
        .ok("list_dispatch_rules", json!({"board": board}))
        .await;
    assert!(text.contains("Triage of intake"), "{text}");
    let moved = agent
        .set_task_work_class(&intake, "planned")
        .await
        .expect("the agent decides");
    assert_eq!(moved.work_class, "planned");
    let third = admin
        .create_task(&support_task(board, "Third intake"))
        .await
        .expect("task")
        .short_code;
    // A move the board refuses leaves no triage line (KAIROS-T-0456).
    agent_mcp
        .refused(
            "transition_item",
            json!({"short_code": third, "to_column": "Completed"}),
            "INVALID_TRANSITION",
        )
        .await;
    assert_eq!(agent_rows(&mut conn), 1, "no line for a refused move");
    agent_mcp
        .ok(
            "transition_item",
            json!({"short_code": third, "to_column": "Todo"}),
        )
        .await;
    assert_eq!(agent_rows(&mut conn), 2, "one line for each agent decision");
    let fifth = admin
        .create_task(&support_task(board, "Fifth intake"))
        .await
        .expect("task")
        .short_code;
    agent
        .cancel_task(&fifth, "duplicate")
        .await
        .expect("the agent decides a cancel");
    let sixth = admin
        .create_task(&support_task(board, "Sixth intake"))
        .await
        .expect("task")
        .short_code;
    agent
        .move_task(&sixth, "ops-delivery")
        .await
        .expect("the agent decides a move");
    assert_eq!(
        agent_rows(&mut conn),
        4,
        "a line for the cancel and the move"
    );

    // Back off over REST.
    let setting = admin.set_agent_triage(board, false).await.expect("off");
    assert!(!setting.agents_decide);
    let fourth = admin
        .create_task(&support_task(board, "Fourth intake"))
        .await
        .expect("task")
        .short_code;
    triage_off(agent.transition_task(&fourth, &todo).await);
}
