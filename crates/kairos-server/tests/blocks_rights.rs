//! Who may create a `blocks` edge (KAIROS-T-0569). A `blocks` edge needs
//! the right to edit the TARGET, the blocked item. A caller who can edit
//! the source only gets a proposal, over REST (202) and over MCP
//! `link_items`. The answer names the proposal and says who can confirm it
//! (KAIROS-A-0060). A caller who can edit neither end is refused (403).
//! The remove of a `blocks` edge keeps the rule of either end. An org
//! admin links each edge.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_blocks_rights_t0569_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_meta::{CreateRelationshipRequest, RelationshipOutcome};
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_blocks_rights_t0569_test";

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

/// A request with an optional JSON body: the status and the JSON body.
async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = http
        .request(method, url)
        .bearer_auth(token)
        .header("X-Tenant", "acme");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    (
        status,
        response.json::<Value>().await.unwrap_or(Value::Null),
    )
}

/// One tool call over MCP: `(is_error, text)`. It opens a session first.
async fn mcp_call(base_url: &str, token: &str, tool: &str, arguments: Value) -> (bool, String) {
    let http = reqwest::Client::new();
    let url = format!("{base_url}/mcp");
    let post = |body: Value| {
        http.post(&url)
            .bearer_auth(token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
    };
    let response = post(json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "kairos-t0569-test", "version": "0.0.0"},
        },
    }))
    .await
    .expect("initialize");
    assert_eq!(response.status(), 200, "initialize");
    let response = post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await
        .expect("initialized");
    assert!(response.status().is_success(), "initialized");
    let response = post(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    }))
    .await
    .expect("tools/call");
    assert_eq!(response.status(), 200, "tools/call");
    let body = response.text().await.expect("body");
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .filter(|v| v.get("jsonrpc").is_some())
        .or_else(|| {
            body.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                .find(|v| v.get("jsonrpc").is_some())
        })
        .unwrap_or_else(|| panic!("no JSON-RPC message in {body:?}"));
    let result = &message["result"];
    (
        result["isError"].as_bool().unwrap_or(false),
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text: {message}"))
            .to_string(),
    )
}

fn task_on(board: &str, title: &str) -> CreateTaskRequest {
    CreateTaskRequest {
        board_id: Some(board.into()),
        column_id: None,
        title: title.into(),
        content: String::new(),
        task_type: None,
        work_class: None,
        team_id: None,
        repository: None,
        from_team: None,
        parent: None,
    }
}

fn edge(source: &str, target: &str, relationship: &str) -> Value {
    json!({
        "source_short_code": source,
        "target_short_code": target,
        "relationship": relationship,
    })
}

/// The edges of a type between two items.
fn edges(conn: &mut PgConnection, source: &str, target: &str, relationship: &str) -> i64 {
    let id_of = |conn: &mut PgConnection, code: &str| {
        kairos_db::embeddings::item_by_short_code(conn, code)
            .expect("item")
            .id
    };
    let (source_id, target_id) = (id_of(conn, source), id_of(conn, target));
    diesel::sql_query(
        "SELECT count(*)::bigint AS n FROM item_relationships \
         WHERE source_id = $1 AND target_id = $2 AND relationship = $3",
    )
    .bind::<diesel::sql_types::Uuid, _>(source_id)
    .bind::<diesel::sql_types::Uuid, _>(target_id)
    .bind::<diesel::sql_types::Text, _>(relationship)
    .get_result::<Count>(conn)
    .expect("counting edges")
    .n
}

/// The pending proposals of the whole tenant.
fn pending(conn: &mut PgConnection) -> i64 {
    diesel::sql_query("SELECT count(*)::bigint AS n FROM edge_proposals WHERE state = 'pending'")
        .get_result::<Count>(conn)
        .expect("counting proposals")
        .n
}

/// The id of the edge between two items, from the relationships of the
/// target.
async fn edge_id(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    source: &str,
    target: &str,
) -> String {
    let (status, body) = send(
        http,
        reqwest::Method::GET,
        format!("{base}/api/tasks/{target}/relationships"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body["incoming"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|group| group["relationship"] == json!("blocks"))
        .flat_map(|group| group["items"].as_array().cloned().unwrap_or_default())
        .find(|item| item["short_code"] == json!(source))
        .and_then(|item| item["relationship_id"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no blocks edge {source} -> {target}: {body}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocks_edge_needs_the_target_and_the_source_alone_proposes() {
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
    let base = server.base_url.clone();
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    for token in [&svc_token, &alice_token, &bob_token, &carol_token] {
        let _ = server.client(token, "acme").whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
        ("carol@kairos.test", OrgRole::Member),
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
    diesel::sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning");

    // Two teams: alice works web, bob works platform. carol is in no team.
    // The admin creates each task, so no member edits a task as its
    // creator: the team gives the right to edit.
    for (name, slug, email) in [
        ("Web", "web", "alice@kairos.test"),
        ("Platform", "platform", "bob@kairos.test"),
    ] {
        let team = admin
            .create_team(&CreateTeamRequest {
                name: name.into(),
                slug: slug.into(),
                code_prefix: kairos_core::short_code::prefix_from_slug(slug),
                team_type: None,
            })
            .await
            .expect("team");
        admin
            .add_team_member(
                &team.id,
                &AddTeamMemberRequest {
                    user_id: email.into(),
                },
            )
            .await
            .expect("member of the team");
    }
    let (web, platform) = ("web-delivery", "platform-delivery");
    let mut codes = Vec::new();
    for (board, title) in [
        (web, "web one"),
        (web, "web two"),
        (web, "web three"),
        (platform, "platform one"),
        (platform, "platform two"),
        (platform, "platform three"),
    ] {
        let created = admin
            .create_task(&task_on(board, title))
            .await
            .expect("task");
        codes.push(created.short_code);
    }
    let [a1, a2, a3, b1, b2, b3]: [String; 6] = codes.try_into().expect("six tasks");
    let relationships = format!("{base}/api/relationships");

    // 1. Rights on the TARGET link: bob edits the blocked item.
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        relationships.clone(),
        &bob_token,
        Some(edge(&a1, &b1, "blocks")),
    )
    .await;
    assert_eq!(status, 201, "1 REST: the target links: {body}");
    assert_eq!(body["relationship"], json!("blocks"), "{body}");
    assert_eq!(edges(&mut conn, &a1, &b1, "blocks"), 1);
    let (is_error, text) = mcp_call(
        &base,
        &bob_token,
        "link_items",
        json!({"source": a2, "target": b2, "relationship": "blocks"}),
    )
    .await;
    assert!(!is_error && text.starts_with("Linked"), "1 MCP: {text}");
    assert_eq!(edges(&mut conn, &a2, &b2, "blocks"), 1);

    // 2. Rights on the SOURCE only make a proposal: alice edits the
    //    blocker and not the blocked item. No edge exists.
    assert_eq!(pending(&mut conn), 0);
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        relationships.clone(),
        &alice_token,
        Some(edge(&a1, &b2, "blocks")),
    )
    .await;
    assert_eq!(status, 202, "2 REST: the source proposes: {body}");
    let proposal = &body["proposal"];
    let rest_proposal = proposal["id"].as_str().expect("proposal id").to_string();
    assert_eq!(proposal["state"], json!("pending"), "{body}");
    assert_eq!(proposal["relationship"], json!("blocks"), "{body}");
    assert_eq!(proposal["source"], json!(a1), "{body}");
    assert_eq!(proposal["target"], json!(b2), "{body}");
    let message = body["message"].as_str().expect("message");
    for part in [
        "Proposed",
        "NOT an edge yet",
        rest_proposal.as_str(),
        "\"approve_proposals\"",
        web,
        platform,
        "organization admin",
    ] {
        assert!(message.contains(part), "2 REST: {part:?} in {message}");
    }
    assert_eq!(edges(&mut conn, &a1, &b2, "blocks"), 0, "no edge");
    assert_eq!(pending(&mut conn), 1);
    // The client reads the two answers.
    let outcome = alice
        .link_or_propose(&CreateRelationshipRequest {
            source_short_code: a3.clone(),
            target_short_code: b3.clone(),
            relationship: "blocks".into(),
        })
        .await
        .expect("the client reads a 202");
    match outcome {
        RelationshipOutcome::Proposed(proposed) => {
            assert_eq!(proposed.proposal.state, "pending");
            assert!(proposed.message.contains("NOT an edge yet"), "{proposed:?}");
        }
        RelationshipOutcome::Linked(linked) => panic!("2 client: an edge {linked:?}"),
    }
    assert_eq!(edges(&mut conn, &a3, &b3, "blocks"), 0, "no edge");
    // The same call again: the proposal is pending already (409).
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        relationships.clone(),
        &alice_token,
        Some(edge(&a1, &b2, "blocks")),
    )
    .await;
    assert_eq!(status, 409, "2 REST: the proposal waits: {body}");
    assert_eq!(body["error"]["code"], json!("CONFLICT"), "{body}");
    // MCP link_items says so too.
    let (is_error, text) = mcp_call(
        &base,
        &alice_token,
        "link_items",
        json!({"source": a2, "target": b1, "relationship": "blocks"}),
    )
    .await;
    assert!(!is_error, "2 MCP: {text}");
    for part in [
        "Proposed",
        "NOT an edge yet",
        "proposal ",
        "\"approve_proposals\"",
        web,
        platform,
    ] {
        assert!(text.contains(part), "2 MCP: {part:?} in {text}");
    }
    assert_eq!(edges(&mut conn, &a2, &b1, "blocks"), 0, "no edge");
    assert_eq!(pending(&mut conn), 3);
    // An edge of a different type keeps either end: alice links an
    // `informs` edge from her task to the task of bob.
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        relationships.clone(),
        &alice_token,
        Some(edge(&a1, &b2, "informs")),
    )
    .await;
    assert_eq!(status, 201, "2 REST: informs keeps either end: {body}");
    // A decider confirms the proposal, and the edge exists.
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        format!("{base}/api/proposals/{rest_proposal}/confirm"),
        &svc_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "2: the admin confirms: {body}");
    assert_eq!(edges(&mut conn, &a1, &b2, "blocks"), 1);

    // 3. No rights on either end: refused, and no proposal.
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        relationships.clone(),
        &carol_token,
        Some(edge(&a3, &b1, "blocks")),
    )
    .await;
    assert_eq!(status, 403, "3 REST: {body}");
    let error = &body["error"];
    assert_eq!(error["code"], json!("FORBIDDEN"), "{body}");
    assert_eq!(
        error["details"]["required_capability"],
        json!("manage_tasks"),
        "{body}"
    );
    assert_eq!(
        error["details"]["any_of"][0]["end"],
        json!("target"),
        "{body}"
    );
    assert_eq!(
        error["details"]["any_of"][0]["gives"],
        json!("edge"),
        "{body}"
    );
    assert_eq!(
        error["details"]["any_of"][1]["end"],
        json!("source"),
        "{body}"
    );
    assert_eq!(
        error["details"]["any_of"][1]["gives"],
        json!("proposal"),
        "{body}"
    );
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("the blocked item") && message.contains("proposal"),
        "3 REST: {message}"
    );
    let (is_error, text) = mcp_call(
        &base,
        &carol_token,
        "link_items",
        json!({"source": a3, "target": b1, "relationship": "blocks"}),
    )
    .await;
    assert!(
        is_error && text.contains("the blocked item"),
        "3 MCP: {text}"
    );
    assert_eq!(edges(&mut conn, &a3, &b1, "blocks"), 0, "no edge");
    assert_eq!(pending(&mut conn), 2, "no new proposal");

    // 4. The remove keeps either end. alice edits the source only, and
    //    removes the edge of bob over REST and over MCP. carol is refused.
    let id = edge_id(&http, &base, &alice_token, &a1, &b1).await;
    let (status, body) = send(
        &http,
        reqwest::Method::DELETE,
        format!("{relationships}/{id}"),
        &carol_token,
        None,
    )
    .await;
    assert_eq!(status, 403, "4 REST: carol edits neither end: {body}");
    assert_eq!(body["error"]["code"], json!("FORBIDDEN"), "{body}");
    let (status, body) = send(
        &http,
        reqwest::Method::DELETE,
        format!("{relationships}/{id}"),
        &alice_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "4 REST: the source removes: {body}");
    assert_eq!(edges(&mut conn, &a1, &b1, "blocks"), 0);
    let (is_error, text) = mcp_call(
        &base,
        &alice_token,
        "unlink_items",
        json!({"source": a2, "target": b2, "relationship": "blocks"}),
    )
    .await;
    assert!(!is_error, "4 MCP: the source removes: {text}");
    assert_eq!(edges(&mut conn, &a2, &b2, "blocks"), 0);
    // The target removes too: bob removes the confirmed edge.
    let id = edge_id(&http, &base, &bob_token, &a1, &b2).await;
    let (status, body) = send(
        &http,
        reqwest::Method::DELETE,
        format!("{relationships}/{id}"),
        &bob_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "4 REST: the target removes: {body}");

    // 5. An org admin links each edge, over REST and over MCP.
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        relationships.clone(),
        &svc_token,
        Some(edge(&b3, &a3, "blocks")),
    )
    .await;
    assert_eq!(status, 201, "5 REST: the admin links: {body}");
    let (is_error, text) = mcp_call(
        &base,
        &svc_token,
        "link_items",
        json!({"source": a1, "target": b1, "relationship": "blocks"}),
    )
    .await;
    assert!(!is_error && text.starts_with("Linked"), "5 MCP: {text}");
    assert_eq!(edges(&mut conn, &b3, &a3, "blocks"), 1);
    assert_eq!(edges(&mut conn, &a1, &b1, "blocks"), 1);
    assert_eq!(pending(&mut conn), 2, "the admin makes no proposal");
}
