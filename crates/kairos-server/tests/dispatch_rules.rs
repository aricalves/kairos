//! The agent dispatch rules of a board (KAIROS-T-0385, COLLIERY-I-0614).
//!
//! 1. Only a board manager makes targets and rules; a person who belongs to
//!    the board reads the rules and the log, and gets the URL of a target
//!    with no query; a person who does not belong to the board reads
//!    nothing.
//! 2. A task that enters the column of a rule and matches its filter fires
//!    one signed delivery (a transition, a move to the board, a cancel); a
//!    move that does not match fires none; a removed rule fires none.
//! 3. The payload: code, title, task type, link (with KAIROS_PUBLIC_URL),
//!    board, column, repository, actor.
//! 4. The log shows each delivery with its tries.
//! 5. MCP: create_dispatch_rule (with a new target and its secret),
//!    list_dispatch_rules, delete_dispatch_rule, and the refusal for a
//!    person who cannot configure the board.
//! 6. KAIROS-T-0427: an `unblocked` rule fires when the last open blocker
//!    of a task closes (a cancel, an archive), once for a task with two
//!    blockers, with the code of the blocker; its column and its filter
//!    must match.
//!
//! Runs against the LIVE compose stack (`angreal services up`), with
//! KAIROS_WEBHOOK_ALLOW_PRIVATE on (the receiver is on 127.0.0.1). Owns the
//! scratch database `kairos_dispatch_rules_t0385_test`.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_dispatch::{
    CreateDispatchRuleRequest, CreateWebhookTargetRequest, WebhookDeliveriesQuery,
};
use kairos_client::types_meta::{CreateRelationshipRequest, UpdateMetadataRequest};
use kairos_client::types_org::{AddBoardMemberRequest, AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::{EntityKind, Error, KairosClient};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::jobs::{Worker, kinds};
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;
use kairos_server::webhooks::signature;

const SCRATCH_DB: &str = "kairos_dispatch_rules_t0385_test";

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
                    "clientInfo": {"name": "kairos-t0385-dispatch", "version": "0.0.0"},
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

/// Run the worker until no delivery job is open.
async fn deliver(worker: &Arc<Worker>, url: &str) {
    let url = url.to_string();
    let run = async {
        loop {
            worker.pass().await;
            let open = {
                let mut conn = PgConnection::establish(&url).expect("connect");
                kairos_db::jobs::list(&mut conn, kinds::WEBHOOK_DELIVERY)
                    .expect("jobs")
                    .iter()
                    .any(|j| j.state == "queued" || j.state == "leased")
            };
            if !open && worker.running() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the deliveries went in time");
}

async fn new_task(client: &KairosClient, board: &str, title: &str, task_type: &str) -> String {
    client
        .create_task(&CreateTaskRequest {
            board_id: Some(board.to_string()),
            column_id: None,
            title: title.to_string(),
            content: String::new(),
            task_type: Some(task_type.to_string()),
            work_class: None,
            team_id: None,
            repository: None,
        })
        .await
        .expect("task")
        .short_code
}

fn forbidden(result: Result<impl std::fmt::Debug, Error>) {
    let err = result.expect_err("refused");
    assert!(matches!(err, Error::Forbidden { .. }), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_that_enters_the_column_of_a_rule_fires_one_signed_delivery() {
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
    let carol_token = user_token(&http, "carol").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([9u8; 32]));
    config.webhooks.allow_private = true;
    config.webhooks.retry_base = Duration::from_millis(10);
    let state = app::state_with(config, pool, auth);
    let worker = Arc::clone(&state.jobs);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    let bob = server.client(&bob_token, "acme");
    let carol = server.client(&carol_token, "acme");
    for client in [&admin, &alice, &bob, &carol] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
        ("carol@kairos.test", OrgRole::Member),
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
    let alice_id = user_id(&mut conn, "alice@kairos.test");
    let bob_id = user_id(&mut conn, "bob@kairos.test");

    // The board of the team web: alice manages it, bob is in the team, carol
    // is in neither.
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
    admin
        .add_team_member(
            &team.id,
            &AddTeamMemberRequest {
                user_id: bob_id.to_string(),
            },
        )
        .await
        .expect("bob in the team");
    admin
        .add_board_member(
            &board,
            &AddBoardMemberRequest {
                user_id: alice_id.to_string(),
                capabilities: vec![
                    "configure_boards".into(),
                    "manage_tasks".into(),
                    "transition_items".into(),
                ],
            },
        )
        .await
        .expect("alice manages the board");
    let detail = admin.get_board(&board).await.expect("board");
    let slug = detail.board.slug.clone();
    let column: HashMap<String, String> = detail
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    // A second board, for a move into its entry column.
    let api = admin
        .create_team(&CreateTeamRequest {
            name: "Api".into(),
            slug: "api".into(),
            code_prefix: "API".into(),
            team_type: None,
        })
        .await
        .expect("team api");
    let api_board = api.delivery_board_id.clone().expect("api board");

    let receiver = common::hook_http::serve().await;

    // --- 1. Rights ---------------------------------------------------------
    let request = CreateWebhookTargetRequest {
        name: "Runner".into(),
        url: format!("{}?token=abc", receiver.url),
    };
    forbidden(bob.create_webhook_target(&slug, &request).await);
    forbidden(carol.create_webhook_target(&slug, &request).await);
    let created = alice
        .create_webhook_target(&slug, &request)
        .await
        .expect("alice makes a target");
    let secret = created.secret.clone();
    assert!(secret.starts_with("kairos_whsec_"), "{secret}");
    let target = created.target.id.clone();
    // The secret is never shown again.
    let listed = alice.list_webhook_targets(&slug).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert!(!serde_json::to_string(&listed).unwrap().contains(&secret));
    assert!(listed[0].url.ends_with("?token=abc"), "a manager sees it");
    let listed = bob.list_webhook_targets(&slug).await.expect("bob reads");
    assert_eq!(listed[0].url, receiver.url, "a member gets no query");
    forbidden(carol.list_webhook_targets(&slug).await);
    forbidden(carol.list_dispatch_rules(&slug).await);
    forbidden(
        carol
            .list_webhook_deliveries(&slug, &WebhookDeliveriesQuery::default())
            .await,
    );
    // An unknown or private input is refused and named.
    let err = alice
        .create_webhook_target(
            &slug,
            &CreateWebhookTargetRequest {
                name: "Bad".into(),
                url: "ftp://example.com/x".into(),
            },
        )
        .await
        .expect_err("ftp");
    assert!(matches!(err, Error::Validation { .. }), "{err:?}");

    let rule = |column: &str, task_type: Option<&str>| CreateDispatchRuleRequest {
        column: Some(column.into()),
        target_id: target.clone(),
        task_type: task_type.map(str::to_string),
        ..Default::default()
    };
    forbidden(bob.create_dispatch_rule(&slug, &rule("Active", None)).await);
    let err = alice
        .create_dispatch_rule(&slug, &rule("Doing", None))
        .await
        .expect_err("no such column");
    assert!(err.to_string().contains("Todo"), "{err}");
    let err = alice
        .create_dispatch_rule(&slug, &rule("Active", Some("epic")))
        .await
        .expect_err("no such type");
    assert!(err.to_string().contains("tech_debt"), "{err}");
    let err = alice
        .create_dispatch_rule(
            &slug,
            &CreateDispatchRuleRequest {
                metadata_slug: Some("nope".into()),
                metadata_value: Some("x".into()),
                ..rule("Todo", None)
            },
        )
        .await
        .expect_err("no such definition");
    assert!(err.to_string().contains("nope"), "{err}");

    // R1: a bug that enters Active. R2: a task with priority high that
    // enters Todo. R3: each task that is cancelled (enters Completed).
    let r1 = alice
        .create_dispatch_rule(&slug, &rule("Active", Some("bug")))
        .await
        .expect("r1");
    assert_eq!(
        (r1.column.as_deref(), r1.task_type.as_deref()),
        (Some("Active"), Some("bug"))
    );
    let r2 = alice
        .create_dispatch_rule(
            &slug,
            &CreateDispatchRuleRequest {
                metadata_slug: Some("priority".into()),
                metadata_value: Some("High".into()),
                ..rule("todo", None)
            },
        )
        .await
        .expect("r2");
    assert_eq!(r2.column.as_deref(), Some("Todo"));
    assert_eq!(r2.trigger, "entered_column");
    let r3 = alice
        .create_dispatch_rule(&slug, &rule("Completed", None))
        .await
        .expect("r3");
    // The rules of the api board: each task that lands in its entry column.
    let api_target = admin
        .create_webhook_target(
            &api_board,
            &CreateWebhookTargetRequest {
                name: "Api runner".into(),
                url: receiver.url.clone(),
            },
        )
        .await
        .expect("api target");
    let api_detail = admin.get_board(&api_board).await.expect("api board");
    let api_entry = api_detail.columns[0].name.clone();
    admin
        .create_dispatch_rule(
            &api_board,
            &CreateDispatchRuleRequest {
                column: Some(api_entry.clone()),
                target_id: api_target.target.id.clone(),
                ..Default::default()
            },
        )
        .await
        .expect("api rule");
    // A bob reads the rules.
    let rules = bob.list_dispatch_rules(&slug).await.expect("bob reads");
    assert_eq!(rules.len(), 3);

    // --- 2. The moves ------------------------------------------------------
    let bug = new_task(&alice, &board, "Fix the login", "bug").await;
    let plain = new_task(&alice, &board, "Write the guide", "task").await;
    let urgent = new_task(&alice, &board, "Ship the fix", "task").await;
    let moved = new_task(&alice, &board, "Move me", "task").await;
    let dropped = new_task(&alice, &board, "Drop me", "task").await;
    alice
        .update_metadata(
            EntityKind::Task,
            &urgent,
            &UpdateMetadataRequest {
                values: BTreeMap::from([("priority".to_string(), Some("high".to_string()))]),
            },
        )
        .await
        .expect("priority");

    // Backlog -> Todo: the bug has no priority, so R2 does not match.
    alice
        .transition_task(&bug, &column["Todo"])
        .await
        .expect("bug to Todo");
    // Todo -> Active: R1 matches the bug (1).
    alice
        .transition_task(&bug, &column["Active"])
        .await
        .expect("bug to Active");
    // A task to Active: R1 is for bugs only.
    alice
        .transition_task(&plain, &column["Todo"])
        .await
        .expect("plain to Todo");
    alice
        .transition_task(&plain, &column["Active"])
        .await
        .expect("plain to Active");
    // A task with priority high to Todo: R2 matches (2).
    alice
        .transition_task(&urgent, &column["Todo"])
        .await
        .expect("urgent to Todo");
    // A move to the api board: its rule matches (3).
    admin.move_task(&moved, &api_board).await.expect("move");
    // A cancel: R3 matches (4).
    alice
        .cancel_task(&dropped, "Not needed")
        .await
        .expect("cancel");
    deliver(&worker, &scratch_url).await;

    let received = receiver.received();
    let mut by_code: HashMap<String, Vec<Value>> = HashMap::new();
    for request in &received {
        // Each request is signed with the secret of its target.
        let key = if request.json()["data"]["board"]["slug"] == json!(slug) {
            secret.clone()
        } else {
            api_target.secret.clone()
        };
        assert_eq!(
            request.headers["x-kairos-signature"],
            signature(&key, &request.body),
            "{:?}",
            request.json()
        );
        assert_eq!(request.headers["x-kairos-event"], "task.entered_column");
        let body = request.json();
        assert_eq!(body["id"], json!(request.headers["x-kairos-delivery"]));
        by_code
            .entry(body["data"]["code"].as_str().unwrap().to_string())
            .or_default()
            .push(body);
    }
    let mut codes: Vec<&String> = by_code.keys().collect();
    codes.sort();
    let mut expected = vec![&bug, &urgent, &moved, &dropped];
    expected.sort();
    assert_eq!(codes, expected, "one delivery for each matching move");
    assert!(by_code.values().all(|v| v.len() == 1), "{by_code:?}");
    assert!(!by_code.contains_key(&plain), "no match, no delivery");

    // --- 3. The payload ----------------------------------------------------
    let data = &by_code[&bug][0]["data"];
    assert_eq!(data["title"], "Fix the login");
    assert_eq!(data["task_type"], "bug");
    assert_eq!(data["work_class"], "planned", "KAIROS-T-0429: the lane");
    assert_eq!(data["link"], format!("https://kairos.test/items/{bug}"));
    assert_eq!(data["board"]["slug"], json!(slug));
    assert_eq!(data["column"]["name"], "Active");
    assert_eq!(data["repository"], Value::Null);
    assert_eq!(data["actor"]["id"], json!(alice_id));
    assert_eq!(data["rule_id"], json!(r1.id));
    assert_eq!(by_code[&urgent][0]["data"]["rule_id"], json!(r2.id));
    assert_eq!(by_code[&dropped][0]["data"]["rule_id"], json!(r3.id));
    assert_eq!(by_code[&dropped][0]["data"]["column"]["name"], "Completed");
    assert_eq!(
        by_code[&moved][0]["data"]["column"]["name"],
        json!(api_entry)
    );
    assert_eq!(by_code[&moved][0]["organization"], "acme");

    // --- 4. The log --------------------------------------------------------
    let log = bob
        .list_webhook_deliveries(&slug, &WebhookDeliveriesQuery::default())
        .await
        .expect("bob reads the log");
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(log.iter().all(|d| d.status == "delivered" && d.tries == 1));
    assert!(log.iter().all(|d| d.log.len() == 1 && d.log[0].ok));
    assert!(log.iter().all(|d| d.log[0].response_code == Some(204)));
    let of_r1 = bob
        .list_webhook_deliveries(
            &slug,
            &WebhookDeliveriesQuery {
                rule_id: Some(r1.id.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("the log of r1");
    assert_eq!(of_r1.len(), 1);
    assert_eq!(of_r1[0].short_code.as_deref(), Some(bug.as_str()));
    let err = bob
        .list_webhook_deliveries(
            &slug,
            &WebhookDeliveriesQuery {
                limit: Some(0),
                ..Default::default()
            },
        )
        .await
        .expect_err("limit 0");
    assert!(matches!(err, Error::Validation { .. }), "{err:?}");

    // A removed rule fires nothing.
    forbidden(bob.delete_dispatch_rule(&slug, &r1.id).await);
    alice
        .delete_dispatch_rule(&slug, &r1.id)
        .await
        .expect("remove r1");
    let other_bug = new_task(&alice, &board, "Another bug", "bug").await;
    alice
        .transition_task(&other_bug, &column["Todo"])
        .await
        .expect("to Todo");
    alice
        .transition_task(&other_bug, &column["Active"])
        .await
        .expect("to Active");
    deliver(&worker, &scratch_url).await;
    assert_eq!(receiver.received().len(), received.len());

    // A test delivery (ping) by a manager.
    let ping = alice
        .test_webhook_target(&slug, &target)
        .await
        .expect("ping");
    assert_eq!(ping.event, "ping");
    deliver(&worker, &scratch_url).await;
    let last = receiver.received().pop().expect("the ping");
    assert_eq!(last.headers["x-kairos-event"], "ping");
    assert_eq!(last.headers["x-kairos-delivery"], ping.id);

    // --- 5. MCP ------------------------------------------------------------
    let mut alice_mcp = Mcp::connect(&server.base_url, &alice_token).await;
    let (text, error) = alice_mcp
        .call(
            "create_dispatch_rule",
            json!({
                "board": slug,
                "column": "Todo",
                "url": receiver.url,
                "target_name": "Claude runner",
                "task_type": "bug",
            }),
        )
        .await;
    assert!(!error, "{text}");
    assert!(text.contains("secret: kairos_whsec_"), "{text}");
    assert!(text.contains("Claude runner"), "{text}");
    let (text, error) = alice_mcp
        .call("list_dispatch_rules", json!({ "board": slug }))
        .await;
    assert!(!error, "{text}");
    assert!(text.contains("Dispatch rules of"), "{text}");
    assert!(text.contains("Claude runner"), "{text}");
    assert!(text.contains("delivered"), "{text}");
    assert!(
        !text.contains("kairos_whsec_"),
        "no secret in a list: {text}"
    );
    let rules = alice.list_dispatch_rules(&slug).await.expect("rules");
    let made = rules
        .iter()
        .find(|r| r.target == "Claude runner")
        .expect("the MCP rule");
    let mut bob_mcp = Mcp::connect(&server.base_url, &bob_token).await;
    let (text, error) = bob_mcp
        .call(
            "create_dispatch_rule",
            json!({ "board": slug, "column": "Todo", "target_id": target }),
        )
        .await;
    assert!(error, "{text}");
    assert!(text.contains("FORBIDDEN"), "{text}");
    let (text, error) = bob_mcp
        .call(
            "create_dispatch_rule",
            json!({ "board": slug, "column": "Todo" }),
        )
        .await;
    assert!(error && text.contains("VALIDATION"), "{text}");
    let (text, error) = alice_mcp
        .call(
            "delete_dispatch_rule",
            json!({ "board": slug, "rule_id": made.id }),
        )
        .await;
    assert!(!error, "{text}");
    assert!(text.contains("Removed the dispatch rule"), "{text}");
    assert_eq!(
        alice.list_dispatch_rules(&slug).await.expect("rules").len(),
        2
    );

    // --- 6. KAIROS-T-0427: unblocked -----------------------------------
    let err = alice
        .create_dispatch_rule(
            &slug,
            &CreateDispatchRuleRequest {
                column: None,
                ..rule("Todo", None)
            },
        )
        .await
        .expect_err("entered_column needs a column");
    assert!(err.to_string().contains("column"), "{err}");
    let err = alice
        .create_dispatch_rule(
            &slug,
            &CreateDispatchRuleRequest {
                trigger: Some("closed".into()),
                ..rule("Todo", None)
            },
        )
        .await
        .expect_err("no such trigger");
    assert!(err.to_string().contains("unblocked"), "{err}");
    // U1: each task (type task) that is unblocked, in each column. U2: a
    // bug in Todo that is unblocked.
    let u1 = alice
        .create_dispatch_rule(
            &slug,
            &CreateDispatchRuleRequest {
                trigger: Some("unblocked".into()),
                column: None,
                ..rule("x", Some("task"))
            },
        )
        .await
        .expect("u1");
    assert_eq!(
        (u1.trigger.as_str(), u1.column.as_deref()),
        ("unblocked", None)
    );
    let u2 = alice
        .create_dispatch_rule(
            &slug,
            &CreateDispatchRuleRequest {
                trigger: Some("unblocked".into()),
                ..rule("Todo", Some("bug"))
            },
        )
        .await
        .expect("u2");
    let blocks = |source: &str, target: &str| CreateRelationshipRequest {
        source_short_code: source.into(),
        target_short_code: target.into(),
        relationship: "blocks".into(),
    };
    let first = new_task(&alice, &board, "First blocker", "task").await;
    let second = new_task(&alice, &board, "Second blocker", "task").await;
    let waiting = new_task(&alice, &board, "Waits for two", "task").await;
    let bug_blocker = new_task(&alice, &board, "Bug blocker", "task").await;
    let bug_in_todo = new_task(&alice, &board, "Bug in Todo", "bug").await;
    let other_blocker = new_task(&alice, &board, "Other blocker", "task").await;
    let bug_in_backlog = new_task(&alice, &board, "Bug in Backlog", "bug").await;
    for (source, target) in [
        (&first, &waiting),
        (&second, &waiting),
        (&bug_blocker, &bug_in_todo),
        (&other_blocker, &bug_in_backlog),
    ] {
        alice
            .create_relationship(&blocks(source, target))
            .await
            .expect("blocks");
    }
    alice
        .transition_task(&bug_in_todo, &column["Todo"])
        .await
        .expect("to Todo");
    let before = receiver.received().len();
    // The first of two blockers closes: nothing.
    alice
        .cancel_task(&first, "Done elsewhere")
        .await
        .expect("cancel");
    deliver(&worker, &scratch_url).await;
    let unblocked = |from: usize| -> Vec<Value> {
        receiver.received()[from..]
            .iter()
            .filter(|r| r.headers["x-kairos-event"] == "task.unblocked")
            .map(|r| r.json())
            .collect()
    };
    assert!(unblocked(before).is_empty(), "{:?}", unblocked(before));
    // The second closes: one delivery, with its code.
    alice
        .cancel_task(&second, "Done elsewhere")
        .await
        .expect("cancel");
    // An archived blocker unblocks the bug in Todo (U2).
    alice.delete_task(&bug_blocker).await.expect("archive");
    // The bug in Backlog: U2 wants Todo, U1 wants a task. Nothing.
    alice
        .cancel_task(&other_blocker, "Done elsewhere")
        .await
        .expect("cancel");
    deliver(&worker, &scratch_url).await;
    let mut got: Vec<(String, String, String)> = unblocked(before)
        .iter()
        .map(|body| {
            (
                body["data"]["code"].as_str().unwrap().to_string(),
                body["data"]["blocker"].as_str().unwrap().to_string(),
                body["data"]["rule_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    got.sort();
    let mut want = vec![
        (waiting.clone(), second.clone(), u1.id.clone()),
        (bug_in_todo.clone(), bug_blocker.clone(), u2.id.clone()),
    ];
    want.sort();
    assert_eq!(got, want);
    let (text, error) = alice_mcp
        .call(
            "create_dispatch_rule",
            json!({ "board": slug, "trigger": "unblocked", "target_id": target }),
        )
        .await;
    assert!(!error, "{text}");
    assert!(text.contains("a task that is unblocked"), "{text}");

    // A removed target takes its rules and its log.
    alice
        .delete_webhook_target(&slug, &target)
        .await
        .expect("remove the target");
    assert!(
        alice
            .list_dispatch_rules(&slug)
            .await
            .expect("rules")
            .iter()
            .all(|r| r.target_id != target)
    );
}
