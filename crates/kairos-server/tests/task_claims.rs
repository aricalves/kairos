//! A task in Active has a claim (KAIROS-T-0359, KAIROS-A-0024 decisions 4
//! and 5), on the REST and MCP surfaces. The acceptance criteria:
//!
//! 1. A task moved to Active with the agent key of alice shows the claim
//!    "alice" (agent): REST get, REST board items, MCP `get_item` and MCP
//!    `board_items`.
//! 2. A change by bob to the claimed task succeeds, is recorded, and gives
//!    the warning that names alice: the REST header `Kairos-Warning` and a
//!    line of the MCP text. A change by alice (with or without her agent
//!    key) gives no warning.
//! 3. A hand-off to bob makes the claim bob's.
//! 4. A release leaves no claim; a claim takes a free task (KAIROS-T-0528),
//!    and the next person who moves the task
//!    to Active (out and back in) gets the claim.
//! 5. The claim ends when the task goes out of Active, to a column that
//!    holds no claim. Blocked keeps claims: the claim survives
//!    Active -> Blocked -> Active (KAIROS-T-0529).
//! 7. The holder of the claim may edit the task: notes, state, metadata;
//!    not an archive (KAIROS-T-0532).
//! 8. A hand-off goes only to a person who holds `transition_items` on the
//!    board of the task (KAIROS-T-0530): to carol (an org member with no
//!    right on the board) it is refused over REST and MCP; to bob (the
//!    team) it passes. `GET /api/tasks/{code}/hand-off-targets` lists the
//!    persons who can get the claim.
//!
//! And: a move to Active by a service account makes no claim; an archive
//! of a claimed task ends the claim; a person with no claim and no
//! `transition_items` cannot hand off or release; the column DTO carries
//! the flag `claims`.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_task_claims_t0359_server_test`.

mod common;

use std::collections::{BTreeMap, HashMap};
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
use kairos_client::types::{CreateTaskRequest, SetTaskStateRequest, Task};
use kairos_client::types_meta::{ActivityQuery, UpdateMetadataRequest};
use kairos_client::types_org::{
    AddBoardMemberRequest, AddTeamMemberRequest, BoardItemsQuery, CreateTeamRequest,
};
use kairos_client::types_service_accounts::{CreateApiKeyRequest, CreateServiceAccountRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_task_claims_t0359_server_test";

fn user(conn: &mut PgConnection, email: &str) -> (Uuid, String) {
    users::table
        .filter(users::email.eq(email))
        .select((users::id, users::display_name))
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
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool} gave no text: {result}"))
            .to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    async fn call_ok(&mut self, tool: &str, arguments: Value) -> String {
        let (text, error) = self.call(tool, arguments).await;
        assert!(!error, "{tool}: {text}");
        text
    }
}

/// A PATCH of the content of a task over raw HTTP, to read the headers.
/// Returns the status and the values of `Kairos-Warning`.
async fn patch_content(
    base_url: &str,
    token: &str,
    task: &Task,
    content: &str,
) -> (reqwest::StatusCode, Vec<String>, Value) {
    let response = reqwest::Client::new()
        .patch(format!("{base_url}/api/tasks/{}", task.short_code))
        .bearer_auth(token)
        .header("x-tenant", "acme")
        .json(&json!({"content": content, "version": task.version}))
        .send()
        .await
        .expect("PATCH");
    let status = response.status();
    let warnings = response
        .headers()
        .get_all("Kairos-Warning")
        .iter()
        .map(|value| value.to_str().expect("ASCII").to_string())
        .collect();
    let body = response.json().await.expect("JSON body");
    (status, warnings, body)
}

#[tokio::test]
async fn a_task_in_active_has_a_claim() {
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
    let state = app::state_with(base_config(&scratch_url), pool, auth);
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
    let (alice_id, alice_name) = user(&mut conn, "alice@kairos.test");
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
    // alice and bob are in the team; carol is a member of the
    // organization only.
    for user_id in [alice_id, bob_id] {
        admin
            .add_team_member(
                &team.id,
                &AddTeamMemberRequest {
                    user_id: user_id.to_string(),
                },
            )
            .await
            .expect("team member");
    }
    let board = team.delivery_board_id.clone().expect("delivery board");
    let detail = admin.get_board(&board).await.expect("board");
    let columns: HashMap<String, String> = detail
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.id.clone()))
        .collect();
    let flagged: Vec<&str> = detail
        .columns
        .iter()
        .filter(|c| c.claims)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(flagged, vec!["Active"], "the column DTO has the flag");
    let keeping: Vec<&str> = detail
        .columns
        .iter()
        .filter(|c| c.keeps_claims)
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(keeping, vec!["Blocked"], "the seed: Blocked keeps claims");
    let to = |name: &str| columns[name].clone();

    let agent_key = alice
        .create_agent_key(&key_request("laptop"))
        .await
        .expect("alice makes an agent key");
    let agent = server.client(&agent_key.key, "acme");
    let new_task = |title: &'static str| CreateTaskRequest {
        board_id: Some(board.clone()),
        column_id: None,
        title: title.into(),
        content: "one".into(),
        task_type: None,
        work_class: None,
        team_id: None,
        repository: None,
        from_team: None,
        parent: None,
    };

    // --- 1. alice's agent moves the task to Active --------------------------
    let task = alice
        .create_task(&new_task("Claimed"))
        .await
        .expect("alice makes a task");
    assert_eq!(task.claim, None);
    agent
        .transition_task(&task.short_code, &to("Todo"))
        .await
        .expect("to Todo");
    let moved = agent
        .transition_task(&task.short_code, &to("Active"))
        .await
        .expect("to Active");
    let claim = moved.claim.expect("the transition answers with the claim");
    assert_eq!(claim.user_id, alice_id.to_string());
    assert_eq!(claim.display_name, alice_name);
    assert!(claim.agent, "the agent of alice made the claim");

    let got = bob.get_task(&task.short_code).await.expect("REST get");
    assert_eq!(
        got.claim.as_ref().map(|c| c.user_id.clone()),
        Some(alice_id.to_string())
    );
    let items = bob
        .board_items(&board, &BoardItemsQuery::default())
        .await
        .expect("board items");
    let card = items
        .columns
        .iter()
        .flat_map(|group| group.tasks.iter())
        .find(|t| t.short_code == task.short_code)
        .expect("the card");
    assert_eq!(card.claim, got.claim, "the board items carry the claim");

    let mut bob_mcp = Mcp::connect(&server.base_url, &bob_token).await;
    let text = bob_mcp
        .call_ok("get_item", json!({"short_code": task.short_code}))
        .await;
    assert!(
        text.contains(&format!("- claim: {alice_name} (agent) since ")),
        "{text}"
    );
    let text = bob_mcp
        .call_ok("board_items", json!({"board": board}))
        .await;
    assert!(
        text.contains(&format!("[claimed by {alice_name} (agent)]")),
        "{text}"
    );

    // --- 2. bob changes the claimed task -------------------------------------
    let current = bob.get_task(&task.short_code).await.expect("task");
    let (status, warnings, body) =
        patch_content(&server.base_url, &bob_token, &current, "bob was here").await;
    assert!(status.is_success(), "{body}");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains(&format!("claimed by {alice_name} (agent)")),
        "{warnings:?}"
    );
    assert!(warnings[0].starts_with(&task.short_code), "{warnings:?}");
    assert_eq!(body["claim"]["user_id"], json!(alice_id.to_string()));
    let history = bob
        .history(
            kairos_client::EntityKind::Task,
            &task.short_code,
            None,
            None,
        )
        .await
        .expect("history")
        .items;
    assert!(
        history.iter().any(|v| v.edited_by == bob_id.to_string()),
        "the change of bob is recorded: {history:?}"
    );
    // alice with her own token, and with her agent key: no warning.
    for token in [alice_token.as_str(), agent_key.key.as_str()] {
        let current = alice.get_task(&task.short_code).await.expect("task");
        let (status, warnings, body) =
            patch_content(&server.base_url, token, &current, "the holder").await;
        assert!(status.is_success(), "{body}");
        assert!(
            warnings.is_empty(),
            "the holder gets no warning: {warnings:?}"
        );
    }
    // Over MCP.
    let current = bob.get_task(&task.short_code).await.expect("task");
    let text = bob_mcp
        .call_ok(
            "update_item",
            json!({
                "short_code": task.short_code,
                "content": "bob over MCP",
                "version": current.version,
            }),
        )
        .await;
    assert!(
        text.contains(&format!(
            "Warning: {alice_name} (agent) has the claim on {} (since ",
            task.short_code
        )) && text.contains("Your change is recorded."),
        "{text}"
    );
    let mut alice_mcp = Mcp::connect(&server.base_url, &agent_key.key).await;
    let current = alice.get_task(&task.short_code).await.expect("task");
    let text = alice_mcp
        .call_ok(
            "update_item",
            json!({
                "short_code": task.short_code,
                "content": "the agent over MCP",
                "version": current.version,
            }),
        )
        .await;
    assert!(!text.contains("Warning:"), "{text}");

    // carol (no claim, no transition_items) cannot hand off or release.
    let err = carol
        .hand_off_task(&task.short_code, "carol@kairos.test")
        .await
        .expect_err("carol has no right");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
    let err = carol
        .release_task(&task.short_code)
        .await
        .expect_err("carol has no right");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
    // A hand-off to a service account, or to nobody, is refused.
    let sa = admin
        .create_service_account(&CreateServiceAccountRequest { name: "ci".into() })
        .await
        .expect("service account");
    let err = alice
        .hand_off_task(&task.short_code, &sa.id)
        .await
        .expect_err("a service account cannot have a claim");
    assert!(matches!(err, Error::Validation { .. }), "{err}");
    let err = alice
        .hand_off_task(&task.short_code, "nobody@kairos.test")
        .await
        .expect_err("no such person");
    assert!(matches!(err, Error::Validation { .. }), "{err}");

    // --- 8. the target must hold transition_items (KAIROS-T-0530) ----------
    // carol is a person of the organization with no right on the board.
    let err = alice
        .hand_off_task(&task.short_code, "carol@kairos.test")
        .await
        .expect_err("carol cannot move the task");
    assert!(
        matches!(&err, Error::Validation { status: 422, message, .. }
            if message.contains("transition_items")
                && message.contains(&detail.board.slug)
                && message.contains("carol@kairos.test")),
        "{err}"
    );
    let (text, error) = bob_mcp
        .call(
            "hand_off_item",
            json!({"short_code": task.short_code, "to": "carol@kairos.test"}),
        )
        .await;
    assert!(
        error
            && text.starts_with("VALIDATION:")
            && text.contains("transition_items")
            && text.contains(&detail.board.slug)
            && text.contains("\"argument\":\"to\""),
        "{text}"
    );
    let unchanged = alice.get_task(&task.short_code).await.expect("task");
    assert_eq!(
        unchanged.claim.map(|c| c.user_id),
        Some(alice_id.to_string()),
        "the refused hand-off changes nothing"
    );
    // The list of the persons who can get the claim: the team and the
    // organization admin, not carol.
    let targets: Vec<String> = carol
        .hand_off_targets(&task.short_code)
        .await
        .expect("targets")
        .into_iter()
        .map(|t| t.email)
        .collect();
    assert_eq!(
        targets,
        vec!["alice@kairos.test", "bob@kairos.test", "svc@kairos.test"]
    );

    // --- 3. alice hands the task off to bob ---------------------------------
    let handed = alice
        .hand_off_task(&task.short_code, "bob@kairos.test")
        .await
        .expect("hand-off");
    let claim = handed.claim.expect("bob has the claim");
    assert_eq!(claim.user_id, bob_id.to_string());
    assert_eq!(claim.display_name, bob_name);
    assert!(!claim.agent);
    let err = alice
        .hand_off_task(&task.short_code, &bob_id.to_string())
        .await
        .expect_err("bob has it already");
    assert!(matches!(err, Error::Conflict { .. }), "{err}");
    // Now alice gets the warning, and bob does not.
    let current = alice.get_task(&task.short_code).await.expect("task");
    let (_, warnings, _) = patch_content(&server.base_url, &alice_token, &current, "alice").await;
    assert!(
        warnings.len() == 1 && warnings[0].contains(&format!("claimed by {bob_name} since")),
        "{warnings:?}"
    );
    let current = bob.get_task(&task.short_code).await.expect("task");
    let (_, warnings, _) = patch_content(&server.base_url, &bob_token, &current, "bob").await;
    assert!(warnings.is_empty(), "{warnings:?}");
    // Over MCP: bob gives it back to alice by user id.
    let text = bob_mcp
        .call_ok(
            "hand_off_item",
            json!({"short_code": task.short_code, "to": alice_id.to_string()}),
        )
        .await;
    assert!(
        text.contains(&format!("- claim: {alice_name} since ")),
        "{text}"
    );
    alice
        .hand_off_task(&task.short_code, "bob@kairos.test")
        .await
        .expect("hand-off to bob again");

    // --- 4. bob releases; the next person to move it in gets it -------------
    let released = bob.release_task(&task.short_code).await.expect("release");
    assert_eq!(released.claim, None);
    let err = bob
        .release_task(&task.short_code)
        .await
        .expect_err("no claim");
    assert!(
        matches!(&err, Error::Other { code, .. } if code == "NO_CLAIM"),
        "{err}"
    );
    let text = bob_mcp
        .call_ok("get_item", json!({"short_code": task.short_code}))
        .await;
    assert!(text.contains("- claim: none."), "{text}");
    // Blocked keeps claims and gives none (KAIROS-T-0529): the free task
    // moves there with no claim.
    let blocked = alice
        .transition_task(&task.short_code, &to("Blocked"))
        .await
        .expect("to Blocked");
    assert_eq!(blocked.claim, None, "a move into Blocked gives no claim");
    let back = bob
        .transition_task(&task.short_code, &to("Active"))
        .await
        .expect("back into Active");
    let claim = back.claim.expect("bob moved it in");
    assert_eq!(claim.user_id, bob_id.to_string());
    assert!(!claim.agent);
    // The claim survives Active -> Blocked -> Active, whoever moves it.
    let blocked = alice
        .transition_task(&task.short_code, &to("Blocked"))
        .await
        .expect("alice blocks bob's task");
    assert_eq!(
        blocked.claim.expect("the claim stays").user_id,
        bob_id.to_string(),
        "a block does not end the claim"
    );
    let text = bob_mcp.call_ok("my_work", json!({})).await;
    assert!(
        text.contains(&task.short_code),
        "the blocked task stays in bob's work: {text}"
    );
    let back = alice
        .transition_task(&task.short_code, &to("Active"))
        .await
        .expect("alice unblocks it");
    assert_eq!(back.claim.expect("still bob's").user_id, bob_id.to_string());
    // The release over MCP.
    let text = bob_mcp
        .call_ok("release_item", json!({"short_code": task.short_code}))
        .await;
    assert!(
        text.contains(&format!("Released {}", task.short_code)),
        "{text}"
    );
    let (text, error) = bob_mcp
        .call("release_item", json!({"short_code": task.short_code}))
        .await;
    assert!(error && text.starts_with("NO_CLAIM:"), "{text}");
    // --- 4b. a free task is claimed (KAIROS-T-0528) --------------------------
    // carol: no transition_items, not the creator.
    let err = carol
        .claim_task(&task.short_code)
        .await
        .expect_err("carol has no right");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
    // KAIROS-T-0642: the creator gets no claim from the edit rule. carol
    // files a request (file_backlog), alice moves it to Active and frees it;
    // carol, its creator with no transition_items, is refused.
    let request = carol
        .create_task(&CreateTaskRequest {
            work_class: Some("support".into()),
            ..new_task("A request from carol")
        })
        .await
        .expect("carol files a request");
    for column in ["Todo", "Active"] {
        alice
            .transition_task(&request.short_code, &to(column))
            .await
            .expect(column);
    }
    alice
        .release_task(&request.short_code)
        .await
        .expect("free the request");
    let err = carol
        .claim_task(&request.short_code)
        .await
        .expect_err("the creator has no transition_items");
    assert!(
        matches!(&err, Error::Forbidden { .. }) && err.to_string().contains("transition_items"),
        "{err}"
    );
    let claimed = bob
        .claim_task(&task.short_code)
        .await
        .expect("bob claims the free task");
    let claim = claimed.claim.expect("bob has the claim");
    assert_eq!(claim.user_id, bob_id.to_string());
    assert!(!claim.agent);
    let err = bob
        .claim_task(&task.short_code)
        .await
        .expect_err("bob has it already");
    assert!(
        matches!(&err, Error::Conflict { code, .. } if code == "CONFLICT"),
        "{err}"
    );
    let err = alice
        .claim_task(&task.short_code)
        .await
        .expect_err("bob holds it");
    assert!(
        matches!(&err, Error::Conflict { code, .. } if code == "CLAIMED"),
        "{err}"
    );
    assert!(err.to_string().contains(&bob_name), "{err}");
    // `me`: alice takes it with a hand-off to herself.
    let mine = alice
        .hand_off_task(&task.short_code, "me")
        .await
        .expect("hand-off to me");
    assert_eq!(mine.claim.expect("alice").user_id, alice_id.to_string());
    alice.release_task(&task.short_code).await.expect("release");
    // Over MCP: the claim, then the refusal names the holder.
    let text = bob_mcp
        .call_ok("claim_item", json!({"short_code": task.short_code}))
        .await;
    assert!(
        text.contains(&format!("Claimed {}", task.short_code))
            && text.contains(&format!("- claim: {bob_name}")),
        "{text}"
    );
    let (text, error) = alice_mcp
        .call("claim_item", json!({"short_code": task.short_code}))
        .await;
    assert!(
        error && text.starts_with("CLAIMED:") && text.contains(&bob_name),
        "{text}"
    );
    // From Todo: a different task.
    let second = alice
        .create_task(&new_task("From Todo"))
        .await
        .expect("task");
    bob.transition_task(&second.short_code, &to("Todo"))
        .await
        .expect("to Todo");
    let text = bob_mcp
        .call_ok(
            "transition_item",
            json!({"short_code": second.short_code, "to_column": "Active"}),
        )
        .await;
    assert!(
        text.contains(&format!("- claim: {bob_name} since ")),
        "{text}"
    );

    // --- 5. out of Active: the claim ends -----------------------------------
    let done = bob
        .transition_task(&second.short_code, &to("Completed"))
        .await
        .expect("to Completed");
    assert_eq!(done.claim, None);
    let err = bob
        .release_task(&second.short_code)
        .await
        .expect_err("not in Active");
    assert!(
        matches!(&err, Error::Other { code, .. } if code == "NOT_CLAIMABLE"),
        "{err}"
    );
    let activity = bob
        .activity(&ActivityQuery {
            entity_id: Some(second.id.clone()),
            ..ActivityQuery::default()
        })
        .await
        .expect("activity")
        .items;
    let actions: Vec<&str> = activity.iter().map(|e| e.action.as_str()).collect();
    assert!(
        actions.contains(&"claim") && actions.contains(&"release"),
        "{actions:?}"
    );

    // --- a service account makes no claim ------------------------------------
    admin
        .add_team_member(
            &team.id,
            &AddTeamMemberRequest {
                user_id: sa.id.clone(),
            },
        )
        .await
        .expect("the service account joins the team");
    let sa_key = admin
        .create_api_key(&sa.id, &key_request("ci-main"))
        .await
        .expect("service-account key");
    let machine = server.client(&sa_key.key, "acme");
    let third = alice.create_task(&new_task("Machine")).await.expect("task");
    machine
        .transition_task(&third.short_code, &to("Todo"))
        .await
        .expect("to Todo");
    let moved = machine
        .transition_task(&third.short_code, &to("Active"))
        .await
        .expect("to Active");
    assert_eq!(moved.claim, None, "a service account takes no claim");

    // --- an archive of a claimed task ends the claim -------------------------
    let fourth = alice
        .create_task(&new_task("Archived"))
        .await
        .expect("task");
    alice
        .transition_task(&fourth.short_code, &to("Todo"))
        .await
        .expect("to Todo");
    let moved = alice
        .transition_task(&fourth.short_code, &to("Active"))
        .await
        .expect("to Active");
    assert!(moved.claim.is_some());
    alice
        .delete_task(&fourth.short_code)
        .await
        .expect("archive");
    let claims: i64 = {
        diesel::sql_query("SET search_path TO \"org_acme\", public")
            .execute(&mut conn)
            .expect("search_path");
        kairos_db::schema::task_claims::table
            .filter(
                kairos_db::schema::task_claims::task_id.eq(fourth.id.parse::<Uuid>().expect("id")),
            )
            .count()
            .get_result(&mut conn)
            .expect("count")
    };
    assert_eq!(claims, 0, "the archive ended the claim");

    // --- 7. the holder of the claim may edit the task (KAIROS-T-0532) ------
    // carol is an org member with no team. A grant of transition_items
    // makes her a hand-off target (KAIROS-T-0530), and gives no edit right.
    // A hand-off gives her the claim; the claim gives her the edit rule:
    // notes, state, metadata. Not an archive.
    admin
        .add_board_member(
            &board,
            &AddBoardMemberRequest {
                user_id: user(&mut conn, "carol@kairos.test").0.to_string(),
                capabilities: vec!["transition_items".into()],
            },
        )
        .await
        .expect("carol gets transition_items");
    let fifth = alice.create_task(&new_task("Helped")).await.expect("task");
    for column in ["Todo", "Active"] {
        alice
            .transition_task(&fifth.short_code, &to(column))
            .await
            .expect(column);
    }
    let err = carol
        .append_note(EntityKind::Task, &fifth.short_code, "Not mine yet.")
        .await
        .expect_err("carol has no right before the claim");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
    let targets = alice
        .hand_off_targets(&fifth.short_code)
        .await
        .expect("targets");
    assert!(
        targets.iter().any(|t| t.email == "carol@kairos.test"),
        "the grant makes carol a target: {targets:?}"
    );
    alice
        .hand_off_task(&fifth.short_code, "carol@kairos.test")
        .await
        .expect("hand-off to carol, a person with a grant");
    carol
        .append_note(EntityKind::Task, &fifth.short_code, "Mine now.")
        .await
        .expect("the holder appends a note");
    let with_state = carol
        .set_task_state(
            &fifth.short_code,
            &SetTaskStateRequest {
                next_step: Some(Some("Run the tests.".into())),
                ..Default::default()
            },
        )
        .await
        .expect("the holder sets the state");
    assert_eq!(
        with_state.state.and_then(|s| s.next_step).as_deref(),
        Some("Run the tests.")
    );
    carol
        .update_metadata(
            EntityKind::Task,
            &fifth.short_code,
            &UpdateMetadataRequest {
                values: BTreeMap::from([("priority".to_string(), Some("high".to_string()))]),
            },
        )
        .await
        .expect("the holder sets metadata");
    let err = carol
        .delete_task(&fifth.short_code)
        .await
        .expect_err("the claim gives no archive");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
    // After her release the right is gone.
    carol
        .release_task(&fifth.short_code)
        .await
        .expect("release");
    let err = carol
        .append_note(EntityKind::Task, &fifth.short_code, "Not mine any more.")
        .await
        .expect_err("no claim, no right");
    assert!(matches!(err, Error::Forbidden { .. }), "{err}");
}
