//! Notifications ride along on MCP answers until read (KAIROS-T-0382). The
//! acceptance criteria:
//!
//! 1. An answer to a person's key (an agent key, or the person's own login)
//!    ends with the unread notifications: the 5 newest, then "N more: call
//!    get_events"; it stops after `mark_events_read`.
//! 2. A service account gets no block; `get_events` has no block.
//! 3. `get_events` and `GET /api/me/notifications` agree;
//!    `mark_events_read` and `POST /api/me/notifications/read` refuse an id
//!    that is not one of yours, and name it.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_notifications_t0382_server_test`.

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
use kairos_client::types_notifications::MarkNotificationsReadRequest;
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::types_service_accounts::{CreateApiKeyRequest, CreateServiceAccountRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_notifications_t0382_server_test";

fn user(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

fn key_request(name: &str) -> CreateApiKeyRequest {
    CreateApiKeyRequest {
        name: name.into(),
        expires_at: None,
        no_expiry: false,
    }
}

/// The ids in the lines of a notification list (`- <uuid> · ...`).
fn ids_in(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(|rest| rest.split(" · ").next())
        .filter(|id| Uuid::parse_str(id).is_ok())
        .map(str::to_string)
        .collect()
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
    /// The text blocks of the last answer (KAIROS-T-0549).
    last_blocks: Vec<String>,
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
            last_blocks: Vec::new(),
        };
        let (status, body) = mcp
            .post(json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "kairos-t0359-claims", "version": "0.0.0"},
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

    /// The text of a tool call, and whether it is an error.
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
        // KAIROS-T-0549: the answer is the first block; the notifications,
        // when the person has unread ones, are a block of their own. `text`
        // joins every text block, so the assertions on the whole stay.
        let blocks: Vec<&str> = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool} gave no content: {result}"))
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        assert!(!blocks.is_empty(), "{tool} gave no text: {result}");
        self.last_blocks = blocks.iter().map(|b| b.to_string()).collect();
        let text = blocks.join("\n\n");
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    async fn call_ok(&mut self, tool: &str, arguments: Value) -> String {
        let (text, error) = self.call(tool, arguments).await;
        assert!(!error, "{tool}: {text}");
        text
    }
}

#[tokio::test]
async fn notifications_ride_along_until_read() {
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
    let bob_token = user_token(&http, "bob").await;
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
    let bob = server.client(&bob_token, "acme");
    for client in [&admin, &alice, &bob] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
    ] {
        let user_id = user(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    diesel::update(users::table.filter(users::email.eq("alice@kairos.test")))
        .set(users::user_name.eq("alice"))
        .execute(&mut conn)
        .expect("user name");
    let bob_id = user(&mut conn, "bob@kairos.test");
    let web = admin
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
            &web.id,
            &AddTeamMemberRequest {
                user_id: bob_id.to_string(),
            },
        )
        .await
        .expect("bob in web");
    let board = web.delivery_board_id.clone().expect("board");

    // 7 mentions of alice by bob.
    for n in 1..=7 {
        bob.create_task(&CreateTaskRequest {
            board_id: Some(board.clone()),
            column_id: None,
            title: format!("Task {n}"),
            content: "Ask @alice.".into(),
            task_type: None,
            work_class: None,
            team_id: None,
            repository: None,
            from_team: None,
        })
        .await
        .expect("task");
    }

    // --- 1. The block at the end of each answer ------------------------------
    let agent_key = alice
        .create_agent_key(&key_request("laptop"))
        .await
        .expect("agent key");
    let mut agent = Mcp::connect(&server.base_url, &agent_key.key).await;
    let text = agent.call_ok("whoami", json!({})).await;
    let block = text
        .split("## Your notifications (7 unread)")
        .nth(1)
        .unwrap_or_else(|| panic!("the block: {text}"));
    // KAIROS-T-0549: the 5 newest as lines with no id, in a content block of
    // their own; the answer block has no trace of them.
    assert_eq!(block.matches("\n- ").count(), 5, "the 5 newest: {block}");
    assert_eq!(ids_in(block).len(), 0, "no id in the ride-along: {block}");
    assert_eq!(
        agent.last_blocks.len(),
        2,
        "two blocks: {:?}",
        agent.last_blocks
    );
    assert!(
        !agent.last_blocks[0].contains("Your notifications"),
        "the answer block is the answer alone: {}",
        agent.last_blocks[0]
    );
    assert!(block.contains("2 more: call get_events"), "{block}");
    assert!(block.contains("mark_events_read"), "{block}");
    assert!(block.contains("mention"), "{block}");
    // The person's own login gets it too.
    let mut own = Mcp::connect(&server.base_url, &alice_token).await;
    let text = own.call_ok("my_boards", json!({})).await;
    assert!(text.contains("## Your notifications (7 unread)"), "{text}");
    // A refusal has no block.
    let (text, error) = own
        .call("get_item", json!({"short_code": "WEB-T-9999"}))
        .await;
    assert!(error, "{text}");
    assert!(!text.contains("Your notifications"), "{text}");

    // --- 2. No block for a service account, nor on get_events ---------------
    let account = admin
        .create_service_account(&CreateServiceAccountRequest { name: "bot".into() })
        .await
        .expect("service account");
    let bot_key = admin
        .create_api_key(&account.id, &key_request("k"))
        .await
        .expect("key");
    let mut bot = Mcp::connect(&server.base_url, &bot_key.key).await;
    let text = bot.call_ok("whoami", json!({})).await;
    assert!(!text.contains("Your notifications"), "{text}");
    let text = bot.call_ok("get_events", json!({})).await;
    assert!(text.contains("(none)"), "{text}");

    let events = agent.call_ok("get_events", json!({})).await;
    assert!(
        events.starts_with("# Your notifications (7 unread)"),
        "{events}"
    );
    assert!(
        !events.contains("## Your notifications"),
        "no block: {events}"
    );
    let mcp_ids = ids_in(&events);
    assert_eq!(mcp_ids.len(), 7, "{events}");

    // --- 3. REST agrees -------------------------------------------------------
    let list = alice.notifications().await.expect("REST list");
    assert_eq!(list.unread, 7);
    let rest_ids: Vec<String> = list.items.iter().map(|n| n.id.clone()).collect();
    assert_eq!(
        rest_ids, mcp_ids,
        "the same notifications, in the same order"
    );
    assert!(
        list.items
            .iter()
            .all(|n| n.kind == "mention" && n.read_at.is_none())
    );
    assert!(
        list.items[0]
            .short_code
            .as_deref()
            .is_some_and(|c| c.starts_with("WEB-T-"))
    );
    // bob sees none of alice's.
    assert_eq!(bob.notifications().await.expect("bob list").items.len(), 0);
    let err = bob
        .mark_notifications_read(&MarkNotificationsReadRequest {
            ids: Some(vec![mcp_ids[0].clone()]),
            all: None,
        })
        .await
        .expect_err("not bob's");
    assert!(matches!(err, Error::NotFound { .. }), "{err}");

    // mark_events_read: two ids.
    let text = agent
        .call_ok("mark_events_read", json!({"ids": [mcp_ids[0], mcp_ids[1]]}))
        .await;
    assert!(
        text.starts_with("Marked 2 notification(s) read. You have 5 unread."),
        "{text}"
    );
    let text = agent.call_ok("whoami", json!({})).await;
    assert!(text.contains("## Your notifications (5 unread)"), "{text}");
    assert!(!text.contains("more: call get_events"), "{text}");
    // Refusals name the id, and change nothing.
    let unknown = Uuid::new_v4().to_string();
    let (text, error) = agent
        .call("mark_events_read", json!({"ids": [unknown]}))
        .await;
    assert!(
        error && text.starts_with("NOT_FOUND") && text.contains(&unknown),
        "{text}"
    );
    let (text, error) = agent
        .call(
            "mark_events_read",
            json!({"ids": [mcp_ids[2]], "all": true}),
        )
        .await;
    assert!(error && text.starts_with("VALIDATION"), "{text}");
    let (text, error) = agent.call("mark_events_read", json!({})).await;
    assert!(error && text.starts_with("VALIDATION"), "{text}");
    let (text, error) = agent
        .call("mark_events_read", json!({"all": true, "everything": 1}))
        .await;
    assert!(error, "an unknown argument: {text}");
    assert_eq!(alice.notifications().await.expect("list").unread, 5);

    // REST: all.
    let marked = alice
        .mark_notifications_read(&MarkNotificationsReadRequest {
            ids: None,
            all: Some(true),
        })
        .await
        .expect("all read");
    assert_eq!((marked.marked.len(), marked.unread), (5, 0));
    let text = agent.call_ok("whoami", json!({})).await;
    assert!(
        !text.contains("Your notifications"),
        "read: no block: {text}"
    );
    let events = agent.call_ok("get_events", json!({})).await;
    assert!(
        events.contains("(0 unread)") && events.contains("## Read"),
        "{events}"
    );
    assert_eq!(ids_in(&events).len(), 7, "{events}");
    let list = alice.notifications().await.expect("list");
    assert!(list.items.iter().all(|n| n.read_at.is_some()));
}
