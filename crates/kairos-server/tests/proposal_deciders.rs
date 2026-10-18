//! Who decides an edge proposal over REST (KAIROS-T-0538). A person other
//! than the proposer confirms or rejects, and not through an agent key. An
//! agent key of a person is refused, and so is the proposer. Each refusal
//! is 403 PERSON_DECIDES with `details.reason`, and leaves the proposal
//! pending. Another person confirms. A second decision is 409 CONFLICT.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_proposal_deciders_t0538_test`.

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
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::types_service_accounts::CreateApiKeyRequest;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_proposal_deciders_t0538_test";

#[derive(diesel::QueryableByName)]
struct ProposedBy {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    proposed_by: Uuid,
}

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

/// A request with no body: the status and the JSON body.
async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
) -> (u16, Value) {
    let response = http
        .request(method, url)
        .bearer_auth(token)
        .header("X-Tenant", "acme")
        .send()
        .await
        .expect("request");
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
            "clientInfo": {"name": "kairos-t0538-test", "version": "0.0.0"},
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
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_other_than_the_proposer_decides_and_once() {
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
    let base = server.base_url.clone();
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    let bob = server.client(&bob_token, "acme");
    let _ = admin.whoami().await;
    let _ = alice.whoami().await;
    let _ = bob.whoami().await;
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
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
    // Both are in the team, so both can edit its tasks: the link rule is
    // not what refuses alice.
    for email in ["alice@kairos.test", "bob@kairos.test"] {
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
    let board = "web-delivery";
    let key = alice
        .create_agent_key(&CreateApiKeyRequest {
            name: "board session".into(),
            expires_at: None,
            no_expiry: false,
        })
        .await
        .expect("agent key")
        .key;

    let source = admin
        .create_task(&task_on(board, "The blocker"))
        .await
        .expect("task")
        .short_code;
    let target = admin
        .create_task(&task_on(board, "The blocked"))
        .await
        .expect("task")
        .short_code;

    // The agent of alice proposes with her agent key, over MCP. So alice
    // is the proposer.
    let (is_error, text) = mcp_call(
        &base,
        &key,
        "propose_edge",
        json!({"source": source, "target": target, "relationship": "blocks",
               "why": "The blocker writes the table that the blocked reads."}),
    )
    .await;
    assert!(!is_error, "the agent proposes: {text}");
    assert!(
        text.contains("A person other than the proposer must confirm it"),
        "the reply says who confirms: {text}"
    );
    let (status, body) = send(
        &http,
        reqwest::Method::GET,
        format!("{base}/api/items/{target}/proposals"),
        &bob_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let proposal = body[0]["id"].as_str().expect("proposal id").to_string();
    let alice_id = user_id(&mut conn, "alice@kairos.test");
    diesel::sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning");
    let id_of = |conn: &mut PgConnection, code: &str| {
        kairos_db::embeddings::item_by_short_code(conn, code)
            .expect("item")
            .id
    };
    let (source_id, target_id) = (id_of(&mut conn, &source), id_of(&mut conn, &target));
    let proposed_by: Uuid = diesel::sql_query("SELECT proposed_by FROM edge_proposals")
        .get_result::<ProposedBy>(&mut conn)
        .expect("the proposal row")
        .proposed_by;
    assert_eq!(proposed_by, alice_id, "an agent key proposes as its person");
    let url = |verb: &str| format!("{base}/api/proposals/{proposal}/{verb}");
    let still_pending = |http: reqwest::Client, token: String| {
        let base = base.clone();
        let target = target.clone();
        async move {
            let (status, body) = send(
                &http,
                reqwest::Method::GET,
                format!("{base}/api/items/{target}/proposals"),
                &token,
            )
            .await;
            assert_eq!(status, 200, "{body}");
            body.as_array()
                .is_some_and(|rows| rows.len() == 1 && rows[0]["state"] == json!("pending"))
        }
    };
    let refused = |status: u16, body: &Value, reason: &str| {
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], json!("PERSON_DECIDES"), "{body}");
        assert_eq!(body["error"]["details"]["reason"], json!(reason), "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("A person other than the proposer")),
            "the refusal says who decides: {body}"
        );
    };

    // 1. An agent key confirms and rejects: refused, the proposal waits.
    for verb in ["confirm", "reject"] {
        let (status, body) = send(&http, reqwest::Method::POST, url(verb), &key).await;
        refused(status, &body, "agent_key");
    }
    assert!(still_pending(http.clone(), bob_token.clone()).await);

    // 2. The proposer confirms and rejects with her login: refused.
    for verb in ["confirm", "reject"] {
        let (status, body) = send(&http, reqwest::Method::POST, url(verb), &alice_token).await;
        refused(status, &body, "proposer");
    }
    assert!(still_pending(http.clone(), bob_token.clone()).await);
    let edges = |conn: &mut PgConnection| {
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        diesel::sql_query(
            "SELECT count(*)::bigint AS n FROM item_relationships \
             WHERE source_id = $1 AND target_id = $2 AND relationship = 'blocks'",
        )
        .bind::<diesel::sql_types::Uuid, _>(source_id)
        .bind::<diesel::sql_types::Uuid, _>(target_id)
        .get_result::<Count>(conn)
        .expect("counting edges")
        .n
    };
    assert_eq!(edges(&mut conn), 0, "a refused confirm writes no edge");

    // 3. Another person confirms: the edge exists.
    let (status, body) = send(&http, reqwest::Method::POST, url("confirm"), &bob_token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], json!("confirmed"), "{body}");
    assert_eq!(edges(&mut conn), 1, "the confirm wrote the edge");

    // 4. A second decision of the same proposal: 409, and nothing changes.
    for verb in ["confirm", "reject"] {
        let (status, body) = send(&http, reqwest::Method::POST, url(verb), &bob_token).await;
        assert_eq!(status, 409, "{verb}: {body}");
        assert_eq!(body["error"]["code"], json!("CONFLICT"), "{verb}: {body}");
    }
    assert_eq!(edges(&mut conn), 1, "one edge, not two");
}
