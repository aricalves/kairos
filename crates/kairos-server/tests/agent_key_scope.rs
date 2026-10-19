//! The scope of an agent key (KAIROS-T-0536).
//!
//! 1. A new key with no scope gets the default: each capability but `purge`
//!    and `manage_structure`. The key of an org admin makes a task on any
//!    board (the admin role, in the scope). It cannot purge, make a team or
//!    make a service account: 403 `KEY_SCOPE`, and the details name the
//!    scope. The login of the same person can.
//! 2. A key with no scope (made before KAIROS-T-0536) keeps each right.
//! 3. A read-only key (`capabilities: []`) reads, and each write is
//!    `KEY_SCOPE`. whoami shows the scope; with a login, `agent_key` is null.
//! 4. A key with a list of boards writes on its board only; on another
//!    board of the person the refusal has `reason: board`.
//! 5. A scope never adds a right: a key with `purge` of a person who does
//!    not hold `purge` gets FORBIDDEN, not KEY_SCOPE.
//! 6. Unknown input is refused and named: an unknown capability, a glob,
//!    an unknown board, an empty list of boards, an unknown field of the
//!    scope. The key of a service account refuses `scope`.
//! 7. A rotation keeps the scope; the list shows it.
//! 8. MCP: whoami has the scope section; a write of a read-only key is a
//!    tool error with KEY_SCOPE.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_agent_key_scope_t0536_test`.

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
use kairos_client::types_org::{AddBoardMemberRequest, AddTeamMemberRequest, CreateTeamRequest};
use kairos_client::types_service_accounts::{
    CreateApiKeyRequest, CreateServiceAccountRequest, KeyScopeRequest,
};
use kairos_db::api_keys::NewApiKey;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_agent_key_scope_t0536_test";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

fn key_request(name: &str, scope: Option<KeyScopeRequest>) -> CreateApiKeyRequest {
    CreateApiKeyRequest {
        name: name.into(),
        expires_at: None,
        no_expiry: false,
        scope,
    }
}

fn task(board: &str, title: &str) -> CreateTaskRequest {
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

fn team(name: &str, slug: &str, prefix: &str) -> CreateTeamRequest {
    CreateTeamRequest {
        name: name.into(),
        slug: slug.into(),
        code_prefix: prefix.into(),
        team_type: None,
    }
}

/// The refusal is 403 KEY_SCOPE, and its details name the scope.
fn key_scope(result: Result<impl std::fmt::Debug, Error>, needs: &str, reason: &str) -> Value {
    match result {
        Err(Error::Forbidden { code, details, .. }) => {
            assert_eq!(code, "KEY_SCOPE", "{details}");
            assert_eq!(details["scope_needs"], needs, "{details}");
            assert_eq!(details["reason"], reason, "{details}");
            assert!(details["key_scope"]["capabilities"].is_array(), "{details}");
            details
        }
        other => panic!("expected 403 KEY_SCOPE, got {other:?}"),
    }
}

/// 422 VALIDATION that names `field` (and `value` when given).
fn validation(result: Result<impl std::fmt::Debug, Error>, field: &str, value: Option<&str>) {
    let err = result.expect_err("refused");
    assert_eq!(err.code(), Some("VALIDATION"), "{err:?}");
    let text = format!("{err:?}");
    assert!(text.contains(field), "{field}: {text}");
    if let Some(value) = value {
        assert!(text.contains(value), "{value}: {text}");
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
                    "clientInfo": {"name": "kairos-t0536-scope", "version": "0.0.0"},
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

#[tokio::test]
async fn an_agent_key_acts_inside_its_scope() {
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
    let server = spawn_server(app::router(app::state_with(
        base_config(&scratch_url),
        pool,
        auth,
    )))
    .await;
    let admin = server.client(&svc_token, "acme");
    let alice = server.client(&alice_token, "acme");
    for client in [&admin, &alice] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("alice@kairos.test", OrgRole::Member),
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
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    let alice_id = user_id(&mut conn, "alice@kairos.test").to_string();
    let web = admin
        .create_team(&team("Web", "web", "WEB"))
        .await
        .expect("team web");
    admin
        .add_team_member(
            &web.id,
            &AddTeamMemberRequest {
                user_id: alice_id.clone(),
            },
        )
        .await
        .expect("alice joins web");
    let web_board = web.delivery_board_id.clone().expect("web board");
    let ops = admin
        .create_team(&team("Ops", "ops", "OPS"))
        .await
        .expect("team ops");
    let ops_board = ops.delivery_board_id.clone().expect("ops board");
    // alice may make tasks on the ops board too, by a grant.
    admin
        .add_board_member(
            &ops_board,
            &AddBoardMemberRequest {
                user_id: alice_id.clone(),
                capabilities: vec!["manage_tasks".into()],
            },
        )
        .await
        .expect("alice gets manage_tasks on ops");

    // 1. The default scope of the key of an org admin.
    let made = admin
        .create_agent_key(&key_request("admin-default", None))
        .await
        .expect("the admin makes a key with the default scope");
    let scope = made.scope.clone().expect("a new key has a scope");
    assert!(scope.boards.is_none());
    assert!(!scope.capabilities.contains(&"purge".to_string()));
    assert!(!scope.capabilities.contains(&"manage_structure".to_string()));
    for capability in ["manage_tasks", "transition_items", "approve_proposals"] {
        assert!(
            scope.capabilities.contains(&capability.to_string()),
            "{capability}"
        );
    }
    let admin_agent = server.client(&made.key, "acme");
    let doomed = admin_agent
        .create_task(&task(&ops_board, "made by the admin key"))
        .await
        .expect("the admin role works inside the scope");
    admin
        .delete_task(&doomed.short_code)
        .await
        .expect("archive with the login");
    let details = key_scope(
        admin_agent.purge_task(&doomed.short_code).await,
        "purge",
        "capability",
    );
    assert_eq!(details["required_capability"], "purge");
    key_scope(
        admin_agent.create_team(&team("Data", "data", "DATA")).await,
        "manage_structure",
        "capability",
    );
    let sa_refusal = key_scope(
        admin_agent
            .create_service_account(&CreateServiceAccountRequest { name: "ci".into() })
            .await,
        "manage_structure",
        "capability",
    );
    assert_eq!(sa_refusal["required_capability"], "manage_service_accounts");
    // The login of the same person can.
    admin
        .purge_task(&doomed.short_code)
        .await
        .expect("the login purges");

    // 2. A key with no scope (made before KAIROS-T-0536) keeps each right.
    let legacy = kairos_server::service_accounts::auth::generate_key("acme");
    {
        let mut tenant_conn =
            PgConnection::establish(&scratch_url).expect("connecting to scratch database");
        diesel::sql_query("SET search_path TO org_acme, public")
            .execute(&mut tenant_conn)
            .expect("search_path");
        kairos_db::api_keys::create_key(
            &mut tenant_conn,
            NewApiKey {
                user_id: svc_id,
                name: "legacy".into(),
                token_hash: kairos_server::service_accounts::auth::hash_key(&legacy),
                prefix: kairos_server::service_accounts::auth::display_prefix(&legacy),
                created_by: svc_id,
                expires_at: None,
            },
        )
        .expect("a key with no scope");
    }
    let legacy_agent = server.client(&legacy, "acme");
    let me = legacy_agent
        .whoami()
        .await
        .expect("whoami of the legacy key");
    let key = me.agent_key.expect("an agent key");
    assert!(key.scope.is_none(), "no scope: no limit");
    legacy_agent
        .create_team(&team("Data", "data", "DATA"))
        .await
        .expect("a key with no scope changes the structure");

    // 3. A read-only key.
    let read_only = alice
        .create_agent_key(&key_request(
            "reader",
            Some(KeyScopeRequest {
                capabilities: Some(Vec::new()),
                boards: None,
            }),
        ))
        .await
        .expect("alice makes a read-only key");
    assert_eq!(
        read_only.scope.as_ref().map(|s| s.capabilities.len()),
        Some(0)
    );
    let reader = server.client(&read_only.key, "acme");
    let me = reader.whoami().await.expect("a read-only key reads");
    let key = me.agent_key.expect("whoami names the agent key");
    assert_eq!(key.id, read_only.id);
    assert_eq!(
        key.scope.expect("the scope").capabilities,
        Vec::<String>::new()
    );
    assert!(
        alice.whoami().await.expect("login").agent_key.is_none(),
        "a login has no agent key"
    );
    let own = alice
        .create_task(&task(&web_board, "alice's own task"))
        .await
        .expect("alice makes a task with her login");
    reader
        .get_task(&own.short_code)
        .await
        .expect("a read-only key reads a task");
    // With no manage_tasks, a create is a request (file_backlog), and the
    // scope has neither.
    key_scope(
        reader.create_task(&task(&web_board, "no")).await,
        "file_backlog",
        "capability",
    );
    // The creator of the task edits it, but not through a read-only key.
    key_scope(
        reader
            .update_task(
                &own.short_code,
                &kairos_client::types::UpdateContentRequest {
                    title: None,
                    content: "changed".into(),
                    version: own.version,
                },
            )
            .await,
        "manage_tasks",
        "capability",
    );
    // A request to another team (file_backlog) is a write too.
    match reader
        .create_task(&CreateTaskRequest {
            from_team: Some("web".into()),
            ..task(&ops_board, "a request")
        })
        .await
    {
        Err(Error::Forbidden { code, details, .. }) => {
            assert_eq!(code, "KEY_SCOPE", "{details}");
        }
        other => panic!("expected 403 KEY_SCOPE, got {other:?}"),
    }

    // 4. A key with a list of boards.
    let web_only = alice
        .create_agent_key(&key_request(
            "web-only",
            Some(KeyScopeRequest {
                capabilities: Some(vec!["manage_tasks".into(), "transition_items".into()]),
                boards: Some(vec!["web-delivery".into()]),
            }),
        ))
        .await
        .expect("alice makes a key for the board web");
    let boards = web_only
        .scope
        .as_ref()
        .and_then(|s| s.boards.clone())
        .expect("a list of boards");
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].id, web_board);
    assert_eq!(boards[0].slug.as_deref(), Some("web-delivery"));
    let web_agent = server.client(&web_only.key, "acme");
    web_agent
        .create_task(&task(&web_board, "on web"))
        .await
        .expect("the key writes on its board");
    let on_ops = alice
        .create_task(&task(&ops_board, "alice can write on ops"))
        .await
        .expect("the login of alice writes on ops");
    let details = key_scope(
        web_agent
            .update_task(
                &on_ops.short_code,
                &kairos_client::types::UpdateContentRequest {
                    title: None,
                    content: "changed".into(),
                    version: on_ops.version,
                },
            )
            .await,
        "manage_tasks",
        "board",
    );
    assert_eq!(details["board_id"], ops_board);
    assert_eq!(details["key_scope"]["boards"][0], web_board);
    // A create off its board falls to a request, which the scope lacks.
    key_scope(
        web_agent.create_task(&task(&ops_board, "not on ops")).await,
        "file_backlog",
        "capability",
    );

    // 5. A scope never adds a right.
    let wide = alice
        .create_agent_key(&key_request(
            "wide",
            Some(KeyScopeRequest {
                capabilities: Some(vec!["manage_tasks".into(), "purge".into()]),
                boards: None,
            }),
        ))
        .await
        .expect("alice may name purge in a scope");
    let wide_agent = server.client(&wide.key, "acme");
    let archived = alice
        .create_task(&task(&web_board, "to archive"))
        .await
        .expect("task");
    alice
        .delete_task(&archived.short_code)
        .await
        .expect("alice archives her task");
    match wide_agent.purge_task(&archived.short_code).await {
        Err(Error::Forbidden {
            code, capability, ..
        }) => {
            assert_eq!(code, "FORBIDDEN", "the person does not hold purge");
            assert_eq!(capability.as_deref(), Some("purge"));
        }
        other => panic!("expected 403 FORBIDDEN, got {other:?}"),
    }

    // 6. Unknown input is refused and named.
    for (scope, field, value) in [
        (
            KeyScopeRequest {
                capabilities: Some(vec!["manage_tasks".into(), "launch_rockets".into()]),
                boards: None,
            },
            "scope.capabilities",
            Some("launch_rockets"),
        ),
        (
            KeyScopeRequest {
                capabilities: Some(vec!["*".into()]),
                boards: None,
            },
            "scope.capabilities",
            None,
        ),
        (
            KeyScopeRequest {
                capabilities: None,
                boards: Some(vec!["no-such-board".into()]),
            },
            "scope.boards",
            Some("no-such-board"),
        ),
        (
            KeyScopeRequest {
                capabilities: None,
                boards: Some(Vec::new()),
            },
            "scope.boards",
            None,
        ),
    ] {
        validation(
            alice
                .create_agent_key(&key_request("bad", Some(scope)))
                .await,
            field,
            value,
        );
    }
    let (status, body) = {
        let response = http
            .post(format!("{}/api/me/agent-keys", server.base_url))
            .bearer_auth(&alice_token)
            .header("x-tenant", "acme")
            .json(&json!({"name": "bad", "scope": {"capabilities": [], "colour": "red"}}))
            .send()
            .await
            .expect("request");
        let status = response.status();
        (status, response.text().await.expect("body"))
    };
    assert_eq!(status, 422, "{body}");
    assert!(
        body.contains("colour"),
        "the refusal names the field: {body}"
    );
    let sa = admin
        .create_service_account(&CreateServiceAccountRequest { name: "ci".into() })
        .await
        .expect("service account");
    validation(
        admin
            .create_api_key(
                &sa.id,
                &key_request(
                    "ci",
                    Some(KeyScopeRequest {
                        capabilities: Some(Vec::new()),
                        boards: None,
                    }),
                ),
            )
            .await,
        "scope",
        None,
    );
    assert_eq!(
        alice.list_agent_keys().await.expect("list").total,
        3,
        "no refused request made a key"
    );

    // 7. A rotation keeps the scope; the list shows it.
    let rotated = alice
        .rotate_agent_key(&read_only.id)
        .await
        .expect("alice rotates the read-only key");
    let listed = alice.list_agent_keys().await.expect("list");
    let new_key = listed
        .items
        .iter()
        .find(|k| k.id == rotated.id)
        .expect("the new key is in the list");
    assert_eq!(
        new_key.scope.as_ref().map(|s| s.capabilities.len()),
        Some(0),
        "the new key is read-only too"
    );
    let listed_web = listed
        .items
        .iter()
        .find(|k| k.id == web_only.id)
        .and_then(|k| k.scope.clone())
        .expect("the scope of web-only");
    assert_eq!(
        listed_web.describe(),
        "manage_tasks, transition_items on web-delivery"
    );
    key_scope(
        server
            .client(&rotated.key, "acme")
            .create_task(&task(&web_board, "no"))
            .await,
        "file_backlog",
        "capability",
    );

    // 8. MCP.
    let mut mcp = Mcp::connect(&server.base_url, &rotated.key).await;
    let (text, error) = mcp.call("whoami", json!({})).await;
    assert!(!error, "{text}");
    assert!(text.contains("## Agent key scope"), "{text}");
    assert!(text.contains("read-only"), "{text}");
    let (text, error) = mcp
        .call(
            "create_item",
            json!({"item_type": "task", "title": "no", "board": web_board}),
        )
        .await;
    assert!(error && text.contains("KEY_SCOPE"), "{text}");
    let mut mcp = Mcp::connect(&server.base_url, &legacy).await;
    let (text, error) = mcp.call("whoami", json!({})).await;
    assert!(!error, "{text}");
    assert!(text.contains("with no scope"), "{text}");
}
