//! Who decides an edge proposal over REST (KAIROS-T-0659). A holder of
//! `approve_proposals` on the board of the source or of the target confirms
//! or rejects, or an org admin. The agent key of a holder decides, a
//! service account that holds the grant decides, and the proposer decides
//! as a holder. Without the grant, the decision is 403 FORBIDDEN: the
//! details name `approve_proposals` and the two boards, and the proposal
//! waits. Of two decisions at the same time, one gets 409 CONFLICT.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_proposal_deciders_t0659_test`.

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
use kairos_client::types_org::{AddBoardMemberRequest, AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::types_service_accounts::CreateApiKeyRequest;
use kairos_db::api_keys::{self, NewApiKey};
use kairos_db::models::public::{NewServiceAccountUser, User};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::service_accounts::auth::{generate_key, hash_key};

const SCRATCH_DB: &str = "kairos_proposal_deciders_t0659_test";

#[derive(diesel::QueryableByName)]
struct ProposedBy {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    proposed_by: Uuid,
}

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
            "clientInfo": {"name": "kairos-t0659-test", "version": "0.0.0"},
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

/// A service account in the organization, with its key.
fn service_account(
    conn: &mut PgConnection,
    org: Uuid,
    created_by: Uuid,
    name: &str,
) -> (Uuid, String) {
    let account: User = diesel::insert_into(users::table)
        .values(NewServiceAccountUser::new(
            format!("svc:{name}"),
            format!("{name}@svc.acme.kairos"),
            name,
        ))
        .returning(User::as_returning())
        .get_result(conn)
        .expect("service account");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org,
            user_id: account.id,
            role: OrgRole::Member,
        })
        .execute(conn)
        .expect("membership");
    let raw_key = generate_key("acme");
    api_keys::create_key(
        conn,
        NewApiKey {
            user_id: account.id,
            name: name.into(),
            token_hash: hash_key(&raw_key),
            prefix: name.into(),
            created_by,
            expires_at: None,
        },
    )
    .expect("key");
    (account.id, raw_key)
}

/// The pending proposals on an item.
async fn pending(http: &reqwest::Client, base: &str, code: &str, token: &str) -> Vec<Value> {
    let (status, body) = send(
        http,
        reqwest::Method::GET,
        format!("{base}/api/items/{code}/proposals"),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body.as_array().cloned().unwrap_or_default()
}

/// The edges `blocks` between two items.
fn edges(conn: &mut PgConnection, source: &str, target: &str) -> i64 {
    let id_of = |conn: &mut PgConnection, code: &str| {
        kairos_db::embeddings::item_by_short_code(conn, code)
            .expect("item")
            .id
    };
    let (source_id, target_id) = (id_of(conn, source), id_of(conn, target));
    diesel::sql_query(
        "SELECT count(*)::bigint AS n FROM item_relationships \
         WHERE source_id = $1 AND target_id = $2 AND relationship = 'blocks'",
    )
    .bind::<diesel::sql_types::Uuid, _>(source_id)
    .bind::<diesel::sql_types::Uuid, _>(target_id)
    .get_result::<Count>(conn)
    .expect("counting edges")
    .n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_of_approve_proposals_on_either_board_decides_once() {
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
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    let (maker_id, maker_key) = service_account(&mut conn, org_id, svc_id, "maker");

    // Two teams. carol is in both, so she can edit each end: the link rule
    // does not decide a proposal.
    let mut boards = Vec::new();
    for (name, slug, members) in [
        ("Web", "web", ["alice@kairos.test", "carol@kairos.test"]),
        (
            "Platform",
            "platform",
            ["bob@kairos.test", "carol@kairos.test"],
        ),
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
        for email in members {
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
        boards.push(team.delivery_board_id.clone().expect("delivery board"));
    }
    let (web, platform) = ("web-delivery", "platform-delivery");
    // The grant: alice and the service account on the board of the
    // sources, bob on the board of the targets.
    for (board, user) in [
        (&boards[0], "alice@kairos.test".to_string()),
        (&boards[0], maker_id.to_string()),
        (&boards[1], "bob@kairos.test".to_string()),
    ] {
        admin
            .add_board_member(
                board,
                &AddBoardMemberRequest {
                    user_id: user,
                    capabilities: vec!["approve_proposals".into()],
                },
            )
            .await
            .expect("grant approve_proposals");
    }
    let key = alice
        .create_agent_key(&CreateApiKeyRequest {
            name: "board session".into(),
            expires_at: None,
            no_expiry: false,
            scope: None,
        })
        .await
        .expect("agent key")
        .key;

    let source = admin
        .create_task(&task_on(web, "The blocker"))
        .await
        .expect("task")
        .short_code;
    // A proposal from the source to a new target on platform, by `token`
    // over MCP: the id and the target.
    let propose = |token: String, title: &'static str| {
        let (base, source, admin) = (
            base.clone(),
            source.clone(),
            server.client(&svc_token, "acme"),
        );
        let http = http.clone();
        async move {
            let target = admin
                .create_task(&task_on(platform, title))
                .await
                .expect("task")
                .short_code;
            let (is_error, text) = mcp_call(
                &base,
                &token,
                "propose_edge",
                json!({"source": source, "target": target, "relationship": "blocks",
                       "why": "The blocker writes the table that the blocked reads."}),
            )
            .await;
            assert!(!is_error, "the proposal: {text}");
            let waiting = pending(&http, &base, &target, &token).await;
            assert_eq!(waiting.len(), 1, "{waiting:?}");
            let id = waiting[0]["id"].as_str().expect("proposal id").to_string();
            (id, target, text)
        }
    };
    let url = |id: &str, verb: &str| format!("{base}/api/proposals/{id}/{verb}");

    // 1. The agent key of alice proposes. The answer says who decides and
    //    names the two boards. alice is the proposer.
    let (first, first_target, text) = propose(key.clone(), "1: the agent key").await;
    assert!(
        text.contains("NOT an edge yet")
            && text.contains("\"approve_proposals\"")
            && text.contains(web)
            && text.contains(platform)
            && text.contains("organization admin"),
        "the answer says who confirms: {text}"
    );
    let proposed_by: Uuid = diesel::sql_query("SELECT proposed_by FROM edge_proposals")
        .get_result::<ProposedBy>(&mut conn)
        .expect("the proposal row")
        .proposed_by;
    assert_eq!(
        proposed_by,
        user_id(&mut conn, "alice@kairos.test"),
        "an agent key proposes as its person"
    );

    // 2. carol holds no approve_proposals. She edits both ends, and the
    //    confirm and the reject are refused. The refusal names the grant
    //    and the two boards. The proposal waits, and no edge exists.
    for verb in ["confirm", "reject"] {
        let (status, body) = send(
            &http,
            reqwest::Method::POST,
            url(&first, verb),
            &carol_token,
        )
        .await;
        assert_eq!(status, 403, "{verb}: {body}");
        let error = &body["error"];
        assert_eq!(error["code"], json!("FORBIDDEN"), "{body}");
        assert_eq!(
            error["details"]["required_capability"],
            json!("approve_proposals"),
            "{body}"
        );
        let any_of = &error["details"]["any_of"];
        assert_eq!(any_of[0]["end"], json!("source"), "{body}");
        assert_eq!(any_of[0]["board"], json!(web), "{body}");
        assert_eq!(any_of[0]["board_id"], json!(boards[0]), "{body}");
        assert_eq!(any_of[1]["end"], json!("target"), "{body}");
        assert_eq!(any_of[1]["board"], json!(platform), "{body}");
        assert_eq!(any_of[1]["board_id"], json!(boards[1]), "{body}");
        let message = error["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("approve_proposals")
                && message.contains(web)
                && message.contains(platform),
            "the refusal names the grant and the boards: {message}"
        );
    }
    let waiting = pending(&http, &base, &first_target, &carol_token).await;
    assert!(
        waiting.len() == 1 && waiting[0]["state"] == json!("pending"),
        "{waiting:?}"
    );
    assert_eq!(
        edges(&mut conn, &source, &first_target),
        0,
        "a refused confirm writes no edge"
    );

    // 3. The agent key of a holder on the board of the SOURCE confirms the
    //    proposal of its own person: the edge exists.
    let (status, body) = send(&http, reqwest::Method::POST, url(&first, "confirm"), &key).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], json!("confirmed"), "{body}");
    assert_eq!(
        edges(&mut conn, &source, &first_target),
        1,
        "the confirm wrote the edge"
    );

    // 4. A second decision of the same proposal: 409, and nothing changes.
    for verb in ["confirm", "reject"] {
        let (status, body) = send(
            &http,
            reqwest::Method::POST,
            url(&first, verb),
            &alice_token,
        )
        .await;
        assert_eq!(status, 409, "{verb}: {body}");
        assert_eq!(body["error"]["code"], json!("CONFLICT"), "{verb}: {body}");
    }
    assert_eq!(
        edges(&mut conn, &source, &first_target),
        1,
        "one edge, not two"
    );

    // 5. A holder on the board of the TARGET confirms.
    let (second, second_target, _) = propose(carol_token.clone(), "5: the target").await;
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        url(&second, "confirm"),
        &bob_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(edges(&mut conn, &source, &second_target), 1);

    // 6. An org admin with no grant confirms.
    let (third, third_target, _) = propose(carol_token.clone(), "6: the org admin").await;
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        url(&third, "confirm"),
        &svc_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(edges(&mut conn, &source, &third_target), 1);

    // 7. A service account that holds the grant confirms, with its key.
    let (fourth, fourth_target, _) = propose(maker_key.clone(), "7: the service account").await;
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        url(&fourth, "confirm"),
        &maker_key,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(edges(&mut conn, &source, &fourth_target), 1);

    // 8. The proposer, a holder, rejects their own proposal with a login.
    let (fifth, fifth_target, _) = propose(bob_token.clone(), "8: the proposer").await;
    let (status, body) = send(
        &http,
        reqwest::Method::POST,
        url(&fifth, "reject"),
        &bob_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], json!("rejected"), "{body}");
    assert_eq!(
        edges(&mut conn, &source, &fifth_target),
        0,
        "a reject writes no edge"
    );

    // 9. Two decisions at the same time: one wins, the other gets 409.
    let (sixth, sixth_target, _) = propose(carol_token.clone(), "9: two at once").await;
    let (by_alice, by_bob) = tokio::join!(
        send(
            &http,
            reqwest::Method::POST,
            url(&sixth, "confirm"),
            &alice_token
        ),
        send(
            &http,
            reqwest::Method::POST,
            url(&sixth, "reject"),
            &bob_token
        ),
    );
    let mut statuses = [by_alice.0, by_bob.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{by_alice:?} {by_bob:?}");
    let loser = if by_alice.0 == 409 {
        &by_alice.1
    } else {
        &by_bob.1
    };
    assert_eq!(loser["error"]["code"], json!("CONFLICT"), "{loser}");
    let expected_edges = i64::from(by_alice.0 == 200);
    assert_eq!(edges(&mut conn, &source, &sixth_target), expected_edges);
    assert!(
        pending(&http, &base, &sixth_target, &bob_token)
            .await
            .is_empty()
    );

    // 10. whoami and my_boards show the grant to its holder.
    let (is_error, text) = mcp_call(&base, &alice_token, "whoami", json!({})).await;
    assert!(!is_error && text.contains("approve_proposals"), "{text}");
    let (is_error, text) = mcp_call(&base, &alice_token, "my_boards", json!({})).await;
    assert!(
        !is_error && text.contains("approve_proposals: you confirm or reject the edge proposals"),
        "{text}"
    );
    let (_, text) = mcp_call(&base, &carol_token, "my_boards", json!({})).await;
    assert!(
        !text.contains("approve_proposals:"),
        "carol holds no grant: {text}"
    );
}
