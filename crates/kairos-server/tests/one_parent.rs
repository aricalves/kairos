//! KAIROS-T-0572 — one parent per item, an atomic reparent, and a parent
//! on the REST create.
//!
//! - `POST /api/relationships` and MCP `link_items` refuse a second parent
//!   with 422 `PARENT_EXISTS`, which names the parent and the fix;
//! - `PUT /api/{family}/{code}/parent` swaps the edge, removes it with
//!   `null`, changes nothing for the same parent, refuses a body with no
//!   `parent`, a type with no parent, and a caller whom the link rule
//!   refuses; a retired code of the item is refused;
//! - MCP `set_parent` does the same, by the same function;
//! - `parent` on `POST /api/tasks` and `POST /api/initiatives` is written
//!   with the item; a refused parent leaves no item, also when the refusal
//!   comes after the insert (`PARENT_NOT_ACTIVE` in a claims column).
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_one_parent_t0572_server_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Text};
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::types::{CreateInitiativeRequest, CreateStrategyRequest, CreateTaskRequest};
use kairos_client::types_meta::CreateRelationshipRequest;
use kairos_client::types_org::CreateTeamRequest;
use kairos_client::{EntityKind, KairosClient};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_one_parent_t0572_server_test";

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
    async fn post(&mut self, body: Value) -> (StatusCode, String) {
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
                    "clientInfo": {"name": "kairos-t0572", "version": "0.0.0"},
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

/// The short codes of the incoming `parent` edges of an item.
async fn parents_of(client: &KairosClient, kind: EntityKind, code: &str) -> Vec<String> {
    client
        .relationships(kind, code)
        .await
        .expect("relationships")
        .incoming
        .into_iter()
        .filter(|group| group.relationship == "parent")
        .flat_map(|group| group.items.into_iter().map(|item| item.short_code))
        .collect()
}

/// How many live rows of `table` have this title.
fn rows_titled(conn: &mut PgConnection, table: &str, title: &str) -> i64 {
    #[derive(QueryableByName)]
    struct Count {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }
    diesel::sql_query(format!(
        "SELECT count(*) AS count FROM org_acme.{table} WHERE title = $1"
    ))
    .bind::<Text, _>(title)
    .get_result::<Count>(conn)
    .expect("counting rows")
    .count
}

fn task_request(title: &str, parent: Option<&str>, column: Option<&str>) -> CreateTaskRequest {
    CreateTaskRequest {
        board_id: Some("web-delivery".into()),
        column_id: column.map(str::to_string),
        title: title.into(),
        content: String::new(),
        task_type: None,
        work_class: None,
        team_id: None,
        repository: None,
        from_team: None,
        parent: parent.map(str::to_string),
    }
}

/// A raw `PUT .../parent` with any body: the status and the JSON.
async fn put_parent(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = http
        .put(format!("{base}{path}"))
        .bearer_auth(token)
        .header("x-tenant", "acme")
        .json(&body)
        .send()
        .await
        .expect("PUT");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_parent_reparent_and_parent_on_create() {
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
    for client in [&admin, &alice, &bob] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
        ("bob@kairos.test", OrgRole::Member),
    ] {
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: user_id(&mut conn, email),
                role,
            })
            .on_conflict_do_nothing()
            .execute(&mut conn)
            .expect("membership");
    }
    for (name, slug, prefix) in [("Web", "web", "WEB"), ("Ops", "ops", "OPS")] {
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
    let web = admin.get_board("web-delivery").await.expect("board");
    let active = web
        .columns
        .iter()
        .find(|c| c.name == "Active")
        .expect("Active")
        .id
        .clone();

    let strategy = admin
        .create_strategy(&CreateStrategyRequest {
            board_id: "strategy".into(),
            column_id: None,
            title: "Bet".into(),
            content: String::new(),
            hypothesis: None,
        })
        .await
        .expect("strategy")
        .short_code;
    let initiative = |title: &str, parent: Option<&str>| CreateInitiativeRequest {
        board_id: "initiatives".into(),
        column_id: None,
        title: title.into(),
        content: String::new(),
        complexity: None,
        bucket_type: None,
        parent: parent.map(str::to_string),
    };

    // ---- REST create with a parent ----------------------------------------
    let i1 = admin
        .create_initiative(&initiative("First", Some(&strategy)))
        .await
        .expect("initiative under the strategy")
        .short_code;
    assert_eq!(
        parents_of(&admin, EntityKind::Initiative, &i1).await,
        vec![strategy.clone()]
    );
    let i2 = admin
        .create_initiative(&initiative("Second", None))
        .await
        .expect("initiative")
        .short_code;
    let t1 = admin
        .create_task(&task_request("Under first", Some(&i1), None))
        .await
        .expect("task under i1")
        .short_code;
    assert_eq!(
        parents_of(&admin, EntityKind::Task, &t1).await,
        vec![i1.clone()]
    );

    // A parent of the wrong type is refused before the write: no task.
    let err = admin
        .create_task(&task_request("Wrong parent", Some(&strategy), None))
        .await
        .expect_err("a strategy is not the parent of a task");
    assert_eq!(err.status(), Some(422), "{err:?}");
    assert_eq!(err.code(), Some("VALIDATION"), "{err:?}");
    assert_eq!(
        err.details().and_then(|d| d["field"].as_str()),
        Some("parent"),
        "{err:?}"
    );
    assert_eq!(rows_titled(&mut conn, "tasks", "Wrong parent"), 0);
    // An unknown parent too.
    let err = admin
        .create_initiative(&initiative("Lost", Some("ACME-S-9999")))
        .await
        .expect_err("an unknown parent");
    assert_eq!(err.code(), Some("VALIDATION"), "{err:?}");
    assert_eq!(rows_titled(&mut conn, "initiatives", "Lost"), 0);

    // A refusal AFTER the insert rolls the task back: a create into the
    // claims column Active under an initiative in Discovery.
    let err = admin
        .create_task(&task_request("Too early", Some(&i1), Some(&active)))
        .await
        .expect_err("PARENT_NOT_ACTIVE");
    assert_eq!(err.code(), Some("PARENT_NOT_ACTIVE"), "{err:?}");
    assert_eq!(
        rows_titled(&mut conn, "tasks", "Too early"),
        0,
        "the refused create leaves no task"
    );
    // With no parent, the same create passes (KAIROS-A-0040 decision 3).
    admin
        .create_task(&task_request("Free", None, Some(&active)))
        .await
        .expect("no parent, no gate");

    // ---- one parent ---------------------------------------------------------
    let err = admin
        .create_relationship(&CreateRelationshipRequest {
            source_short_code: i2.clone(),
            target_short_code: t1.clone(),
            relationship: "parent".into(),
        })
        .await
        .expect_err("a second parent");
    assert_eq!(err.status(), Some(422), "{err:?}");
    assert_eq!(err.code(), Some("PARENT_EXISTS"), "{err:?}");
    assert_eq!(
        err.details().and_then(|d| d["parent"].as_str()),
        Some(i1.as_str()),
        "{err:?}"
    );
    assert!(err.to_string().contains("/parent"), "{err}");
    assert_eq!(
        parents_of(&admin, EntityKind::Task, &t1).await,
        vec![i1.clone()]
    );

    // ---- PUT .../parent -----------------------------------------------------
    let swap = admin
        .set_parent(EntityKind::Task, &t1, Some(&i2))
        .await
        .expect("the swap");
    assert!(swap.changed);
    assert_eq!(swap.short_code, t1);
    assert_eq!(swap.previous_parent.as_deref(), Some(i1.as_str()));
    assert_eq!(swap.parent.as_deref(), Some(i2.as_str()));
    assert_eq!(
        parents_of(&admin, EntityKind::Task, &t1).await,
        vec![i2.clone()]
    );
    let same = admin
        .set_parent(EntityKind::Task, &t1, Some(&i2))
        .await
        .expect("the same parent");
    assert!(!same.changed);
    // A lower-case code is a code (KAIROS-T-0560).
    let lower = admin
        .set_parent(EntityKind::Task, &t1, Some(&i1.to_lowercase()))
        .await
        .expect("a lower-case parent");
    assert_eq!(lower.parent.as_deref(), Some(i1.as_str()));
    // The initiative level.
    let up = admin
        .set_parent(EntityKind::Initiative, &i2, Some(&strategy))
        .await
        .expect("a strategy parent");
    assert!(up.changed && up.previous_parent.is_none());
    // A task is not the parent of a task.
    let err = admin
        .set_parent(EntityKind::Task, &t1, Some(&strategy))
        .await
        .expect_err("a strategy under a task");
    assert_eq!(err.code(), Some("RELATIONSHIP_RULE"), "{err:?}");
    // null removes it.
    let removed = admin
        .set_parent(EntityKind::Task, &t1, None)
        .await
        .expect("the remove");
    assert!(removed.changed && removed.parent.is_none());
    assert!(parents_of(&admin, EntityKind::Task, &t1).await.is_empty());

    // The body must name `parent`: an empty body changes nothing.
    let (status, body) = put_parent(
        &http,
        &base,
        &svc_token,
        &format!("/api/tasks/{t1}/parent"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(common::error_code(&body), "VALIDATION");
    let (status, body) = put_parent(
        &http,
        &base,
        &svc_token,
        &format!("/api/tasks/{t1}/parent"),
        json!({"parent": null, "extra": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // A strategy has no parent.
    let (status, body) = put_parent(
        &http,
        &base,
        &svc_token,
        &format!("/api/strategies/{strategy}/parent"),
        json!({"parent": null}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("has no parent"),
        "{body}"
    );
    // The family must match the code.
    let (status, body) = put_parent(
        &http,
        &base,
        &svc_token,
        &format!("/api/initiatives/{t1}/parent"),
        json!({"parent": null}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // ---- the link rule --------------------------------------------------------
    // alice sends a request with a parent: she created the task, so she can
    // give it the parent, and change it.
    let mine = alice
        .create_task(&task_request("Request of alice", Some(&i1), None))
        .await
        .expect("a request with a parent")
        .short_code;
    assert_eq!(
        parents_of(&admin, EntityKind::Task, &mine).await,
        vec![i1.clone()]
    );
    let err = bob
        .set_parent(EntityKind::Task, &mine, Some(&i2))
        .await
        .expect_err("bob can edit neither end");
    assert_eq!(err.status(), Some(403), "{err:?}");
    assert_eq!(
        parents_of(&admin, EntityKind::Task, &mine).await,
        vec![i1.clone()],
        "the refused swap keeps the old edge"
    );
    alice
        .set_parent(EntityKind::Task, &mine, Some(&i2))
        .await
        .expect("alice changes the parent of her task");
    let err = bob
        .set_parent(EntityKind::Task, &mine, None)
        .await
        .expect_err("bob cannot remove it either");
    assert_eq!(err.status(), Some(403), "{err:?}");

    // ---- a retired code of the item is refused --------------------------------
    let moved = admin
        .move_task_with(&mine, "ops-delivery", true)
        .await
        .expect("a move with a rename")
        .short_code;
    assert_ne!(moved, mine);
    let err = admin
        .set_parent(EntityKind::Task, &mine, None)
        .await
        .expect_err("a write with a retired code");
    assert_eq!(err.status(), Some(404), "{err:?}");
    admin
        .set_parent(EntityKind::Task, &moved, Some(&i1))
        .await
        .expect("the current code");

    // ---- MCP ------------------------------------------------------------------
    let mut mcp = Mcp::connect(&base, &svc_token).await;
    let text = mcp
        .ok("set_parent", json!({"short_code": t1, "parent": i1}))
        .await;
    assert!(
        text.contains(&format!("Set the parent of {t1}: {i1}")),
        "{text}"
    );
    let text = mcp
        .refused(
            "link_items",
            json!({"source": i2, "target": t1, "relationship": "parent"}),
            "PARENT_EXISTS",
        )
        .await;
    assert!(text.contains("set_parent"), "{text}");
    let item = mcp.ok("get_item", json!({"short_code": t1})).await;
    assert!(item.contains(&format!("parent chain: {i1}")), "{item}");
    let text = mcp
        .ok("set_parent", json!({"short_code": t1, "parent": i1}))
        .await;
    assert!(text.contains("Nothing changed"), "{text}");
    mcp.refused("set_parent", json!({"short_code": t1}), "VALIDATION")
        .await;
    mcp.refused(
        "set_parent",
        json!({"short_code": strategy, "parent": null}),
        "VALIDATION",
    )
    .await;
    let text = mcp
        .ok("set_parent", json!({"short_code": t1, "parent": null}))
        .await;
    assert!(text.contains("Removed the parent"), "{text}");
    let item = mcp.ok("get_item", json!({"short_code": t1})).await;
    assert!(!item.contains("parent chain"), "{item}");
    // MCP create_item with a parent: one transaction, as before.
    let text = mcp
        .ok(
            "create_item",
            json!({"item_type": "task", "board": "web-delivery", "title": "By MCP", "parent": i2}),
        )
        .await;
    assert!(text.contains(&format!("parent: {i2}")), "{text}");
}
