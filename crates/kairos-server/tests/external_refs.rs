//! KAIROS-T-0508 (COLLIERY-I-0630) — an item links to a URI with a label,
//! the relationship `external`:
//!
//! - REST `POST|GET /api/{family}/{code}/external` and
//!   `DELETE .../external/{id}`: link, read, unlink; a `javascript:` URI
//!   is refused with 422 `VALIDATION` that names the scheme; a URI the
//!   item has is `ALREADY_LINKED`; an unknown field is refused; the edit
//!   rule gives 403; a remove of an unknown id is 404.
//! - MCP `link_items` with `relationship: external` and `label`;
//!   `get_item` prints a line `external` per reference; `label` with a
//!   different relationship is refused and named; `unlink_items` removes
//!   by URI; `related_work` ends with the references of the item and of
//!   the proposals; the tool descriptions name `external`.
//! - Each write is one `relationship_add` or `relationship_remove`
//!   activity row.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_external_refs_t0508_test`.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Text, Uuid as SqlUuid};
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, request, spawn_server,
    user_token, with_database,
};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_embed::DeterministicProvider;
use kairos_server::app;
use kairos_server::embedding::EmbeddingService;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_external_refs_t0508_test";
const TENANT: [(&str, &str); 1] = [("x-tenant", "acme")];
const RFC: &str = "https://example.com/wiki/rfc-42";
const LABEL: &str = "RFC: zen as the policy engine, accepted 2026-10-05";

/// Minimal MCP session over HTTP (initialize → initialized → tools/call).
struct McpSession {
    http: reqwest::Client,
    url: String,
    token: String,
    next_id: u64,
}

impl McpSession {
    async fn open(base_url: &str, token: &str) -> Self {
        let http = reqwest::Client::new();
        let url = format!("{base_url}/mcp?profile=all");
        let session = Self {
            http,
            url,
            token: token.to_string(),
            next_id: 0,
        };
        let response = session
            .post(&json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "kairos-t0508-test", "version": "0.0.0"},
                },
            }))
            .await;
        assert_eq!(response.status(), 200, "initialize failed");
        let response = session
            .post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert_eq!(response.status(), 202, "initialized notify");
        session
    }

    async fn post(&self, body: &Value) -> reqwest::Response {
        self.http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(body)
            .send()
            .await
            .expect("mcp request")
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let response = self
            .post(&json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params,
            }))
            .await;
        assert_eq!(response.status(), 200, "{method} HTTP status");
        let body = response.text().await.expect("body");
        let message: Value = serde_json::from_str::<Value>(&body)
            .ok()
            .filter(|v| v.get("jsonrpc").is_some())
            .or_else(|| {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
                    .find(|v| v.get("jsonrpc").is_some())
            })
            .unwrap_or_else(|| panic!("no JSON-RPC message in response body: {body:?}"));
        message["result"].clone()
    }

    /// Call a tool; returns `(is_error, text)`.
    async fn call(&mut self, tool: &str, arguments: Value) -> (bool, String) {
        let result = self
            .rpc("tools/call", json!({"name": tool, "arguments": arguments}))
            .await;
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content: {result}"))
            .to_string();
        (is_error, text)
    }
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

fn activity_rows(conn: &mut PgConnection, item: Uuid, details: &str) -> i64 {
    sql_query(
        "SELECT count(*) AS count FROM activity_log \
          WHERE entity_id = $1 AND details = $2",
    )
    .bind::<SqlUuid, _>(item)
    .bind::<Text, _>(details)
    .get_result::<CountRow>(conn)
    .expect("counting activity")
    .count
}

#[tokio::test]
async fn an_item_links_to_a_uri_with_a_label_against_live_stack() {
    let mut admin_conn = recreate_scratch_db(SCRATCH_DB);
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
    let token = user_token(&http, "svc").await;
    let bob_token = user_token(&http, "bob").await;
    let pool = TenantPool::new(&scratch_url, 8).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut state = app::state_with(base_config(&scratch_url), pool.clone(), auth);
    // `related_work` needs a service; the deterministic provider gives
    // the lexical answer, which is enough for the references.
    state.embedding = Some(Arc::new(EmbeddingService::new(Arc::new(
        DeterministicProvider::default(),
    ))));
    let router = app::router(state);
    for (user, user_token, role) in [
        ("svc", &token, OrgRole::Admin),
        ("bob", &bob_token, OrgRole::Member),
    ] {
        let _ = request(
            &router,
            Method::GET,
            "/api/whoami",
            Some(user_token),
            &TENANT,
            None,
        )
        .await;
        let user_id: Uuid = users::table
            .filter(users::email.eq(format!("{user}@kairos.test")))
            .select(users::id)
            .first(&mut conn)
            .expect("user provisioned");
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }

    let send = |method: Method, uri: String, token: String, body: Option<Value>| {
        let router = router.clone();
        async move { request(&router, method, &uri, Some(&token), &TENANT, body).await }
    };

    let (status, team) = send(
        Method::POST,
        "/api/teams".into(),
        token.clone(),
        Some(json!({"name": "Payments", "slug": "payments", "code_prefix": "PAYMENTS"})),
    )
    .await;
    assert!(status.is_success(), "{status} {team}");
    let task = |title: &str| {
        let title = title.to_string();
        let token = token.clone();
        let send = &send;
        async move {
            let (status, task) = send(
                Method::POST,
                "/api/tasks".into(),
                token,
                Some(json!({
                    "board_id": "payments-delivery",
                    "title": title,
                    "content": format!(
                        "# {title}\n\nZen is the policy engine of the payments service, and the \
                         rules of a payment are the rules of the engine. This text is long \
                         enough to be a section of its own.\n"
                    ),
                })),
            )
            .await;
            assert!(status.is_success(), "{status} {task}");
            task
        }
    };
    let subject = task("Zen as the policy engine").await;
    let other = task("Zen policy engine rules").await;
    let code = subject["short_code"]
        .as_str()
        .expect("short code")
        .to_string();
    let id: Uuid = subject["id"].as_str().expect("id").parse().expect("uuid");
    let other_code = other["short_code"]
        .as_str()
        .expect("short code")
        .to_string();
    let external = format!("/api/tasks/{code}/external");

    // --- REST: link -------------------------------------------------------------
    let (status, made) = send(
        Method::POST,
        external.clone(),
        token.clone(),
        Some(json!({"uri": RFC, "label": LABEL})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["relationship"], "external");
    assert_eq!(made["uri"], RFC);
    assert_eq!(made["label"], LABEL);
    let made_id = made["id"].as_str().expect("id").to_string();
    diesel::sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    assert_eq!(
        activity_rows(
            &mut conn,
            id,
            &format!("relationship:external:{code}->uri:{RFC}")
        ),
        1
    );

    // --- REST: the refusals ------------------------------------------------------
    let (status, answer) = send(
        Method::POST,
        external.clone(),
        token.clone(),
        Some(json!({"uri": "javascript:alert(1)", "label": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "VALIDATION", "{answer}");
    assert_eq!(
        answer["error"]["details"]["scheme"], "javascript",
        "{answer}"
    );
    assert!(
        answer["error"]["message"]
            .as_str()
            .expect("message")
            .contains("\"javascript\""),
        "{answer}"
    );
    let (status, answer) = send(
        Method::POST,
        external.clone(),
        token.clone(),
        Some(json!({"uri": "HTTPS://example.com/wiki/rfc-42"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["code"], "ALREADY_LINKED", "{answer}");
    let (status, answer) = send(
        Method::POST,
        external.clone(),
        token.clone(),
        Some(json!({"uri": "https://example.com/x", "title": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .expect("message")
            .contains("title"),
        "the refusal names the unknown field: {answer}"
    );
    let (status, answer) = send(
        Method::POST,
        external.clone(),
        bob_token.clone(),
        Some(json!({"uri": "https://example.com/bob"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    let (status, answer) = send(
        Method::POST,
        external.clone(),
        token.clone(),
        Some(json!({"uri": "https://example.com/x", "label": "y".repeat(201)})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"]["details"]["field"], "label", "{answer}");

    // --- REST: read ------------------------------------------------------------------
    let (status, listed) = send(Method::GET, external.clone(), bob_token.clone(), None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["short_code"], code.as_str());
    assert_eq!(
        listed["external"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    assert_eq!(listed["external"][0]["id"], made_id.as_str());
    let (status, got) = send(
        Method::GET,
        format!("/api/documents/{code}/external"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "wrong family: {got}");

    // --- MCP: link, get_item, the label on a different relationship -----------------
    let server = spawn_server(router.clone()).await;
    let mut mcp = McpSession::open(&server.base_url, &token).await;
    let (is_error, text) = mcp
        .call(
            "link_items",
            json!({
                "source": code, "target": "mailto:robin@example.com",
                "relationship": "external", "label": "Robin, the author",
            }),
        )
        .await;
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        format!("Linked {code} -[external]-> Robin, the author <mailto:robin@example.com>.")
    );
    let (is_error, text) = mcp
        .call(
            "link_items",
            json!({"source": code, "target": "https://example.com/plain", "relationship": "external"}),
        )
        .await;
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        format!("Linked {code} -[external]-> <https://example.com/plain>.")
    );
    let (is_error, text) = mcp
        .call(
            "link_items",
            json!({"source": code, "target": "file:///etc/passwd", "relationship": "external"}),
        )
        .await;
    assert!(is_error, "{text}");
    assert!(text.contains("\"file\""), "{text}");
    let (is_error, text) = mcp
        .call(
            "link_items",
            json!({"source": code, "target": other_code, "relationship": "blocks", "label": "x"}),
        )
        .await;
    assert!(is_error, "{text}");
    assert!(
        text.contains("label") && text.contains("external"),
        "the refusal names label: {text}"
    );
    let (is_error, text) = mcp
        .call(
            "link_items",
            json!({"source": code, "target": other_code, "relationship": "nope"}),
        )
        .await;
    assert!(is_error, "{text}");
    assert!(
        text.contains("external"),
        "the vocabulary names external: {text}"
    );

    let (is_error, text) = mcp.call("get_item", json!({"short_code": code})).await;
    assert!(!is_error, "{text}");
    let expected = format!(
        "- external: {LABEL} <{RFC}>\n\
         - external: Robin, the author <mailto:robin@example.com>\n\
         - external: <https://example.com/plain>\n"
    );
    assert!(text.contains("## Relationships\n"), "{text}");
    assert!(text.contains(&expected), "{text}");

    // --- MCP: related_work ends with the references -------------------------------------
    let (is_error, text) = mcp
        .call(
            "link_items",
            json!({"source": other_code, "target": "https://example.com/design", "relationship": "external", "label": "Design page"}),
        )
        .await;
    assert!(!is_error, "{text}");
    let (is_error, text) = mcp.call("related_work", json!({"short_code": code})).await;
    assert!(!is_error, "{text}");
    assert!(text.contains("## External references\n"), "{text}");
    assert!(
        text.contains(&format!("- {code}: {LABEL} <{RFC}>\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "- {other_code}: Design page <https://example.com/design>\n"
        )),
        "the proposals carry their references: {text}"
    );

    // --- MCP: the descriptions name external ------------------------------------------
    let tools = mcp.rpc("tools/list", json!({})).await;
    let description = |name: &str| -> String {
        tools["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no tool {name}"))["description"]
            .as_str()
            .expect("description")
            .to_string()
    };
    for name in ["link_items", "unlink_items", "get_item", "related_work"] {
        assert!(
            description(name).contains("external"),
            "{name}: {}",
            description(name)
        );
    }

    // --- MCP: unlink by URI --------------------------------------------------------------
    let (is_error, text) = mcp
        .call(
            "unlink_items",
            json!({"source": code, "target": "MAILTO:robin@example.com", "relationship": "external"}),
        )
        .await;
    assert!(!is_error, "{text}");
    assert_eq!(
        text,
        format!("Unlinked {code} -[external]-> Robin, the author <mailto:robin@example.com>.")
    );
    let (is_error, text) = mcp
        .call(
            "unlink_items",
            json!({"source": code, "target": "mailto:robin@example.com", "relationship": "external"}),
        )
        .await;
    assert!(is_error, "{text}");
    assert!(text.contains("has no external reference"), "{text}");
    assert_eq!(
        activity_rows(
            &mut conn,
            id,
            &format!("relationship:external:{code}->uri:mailto:robin@example.com")
        ),
        2,
        "one add and one remove"
    );

    // --- REST: delete by id --------------------------------------------------------------
    let (status, answer) = send(
        Method::DELETE,
        format!("{external}/{made_id}"),
        bob_token.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    let (status, answer) = send(
        Method::DELETE,
        format!("{external}/{made_id}"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["uri"], RFC);
    let (status, answer) = send(
        Method::DELETE,
        format!("{external}/{made_id}"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    let (status, answer) = send(
        Method::DELETE,
        format!("{external}/not-a-uuid"),
        token.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    let (_, listed) = send(Method::GET, external.clone(), token.clone(), None).await;
    assert_eq!(
        listed["external"]
            .as_array()
            .expect("list")
            .iter()
            .map(|r| r["uri"].as_str().expect("uri"))
            .collect::<Vec<_>>(),
        ["https://example.com/plain"],
        "{listed}"
    );

    drop(pool);
    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
