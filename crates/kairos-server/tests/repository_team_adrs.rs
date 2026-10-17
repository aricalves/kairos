//! Integration test for KAIROS-T-0599: `get_repository` lists the ADRs of
//! the owner team, so the grounding rule finds them.
//!
//! The rules:
//! - An ADR with an `impacts` link to the repository is in the section
//!   "Documents and ADRs that impact this repository" (COLLIERY-T-0269).
//! - An ADR on a board of the owner team with no `impacts` link is in the
//!   section "ADRs of the owner team", with its column.
//! - Each ADR shows once. An ADR that impacts another repository is not
//!   in the list of this repository.
//! - A superseded ADR and an ADR of another team are not in the list. An
//!   archived ADR is in the list with `include_deleted` only, marked.
//! - Newest first, 50 at most; a line says how many more there are.
//!
//! REST (`owner_team_adrs`, `owner_team_adrs_total`) and MCP read the same
//! rows.
//!
//! Runs against the LIVE compose stack. Owns the scratch database
//! `kairos_repository_team_adrs_t0599_test`. Cast: `svc` org admin.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_types::Text;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_client::types::{Adr, CreateAdrRequest};
use kairos_client::types_org::{CreateBoardRequest, CreateTeamRequest};
use kairos_client::types_repositories::CreateRepositoryRequest;
use kairos_client::{EntityKind, KairosClient};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_repository_team_adrs_t0599_test";

const TEAM_HEADING: &str = "\n## ADRs of the owner team\n";
const IMPACT_HEADING: &str = "\n## Documents and ADRs that impact this repository\n";

/// Minimal MCP session over HTTP (initialize, initialized, tools/call).
struct McpSession {
    http: reqwest::Client,
    url: String,
    token: String,
    next_id: u64,
}

impl McpSession {
    async fn open(base_url: &str, token: &str) -> Self {
        let http = reqwest::Client::new();
        let url = format!("{base_url}/mcp");
        let response = http
            .post(&url)
            .bearer_auth(token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "kairos-t0599-test", "version": "0.0.0"},
                },
            }))
            .send()
            .await
            .expect("initialize");
        assert_eq!(response.status(), 200, "initialize failed");
        let response = http
            .post(&url)
            .bearer_auth(token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .expect("initialized");
        assert_eq!(response.status(), 202, "initialized notify");
        Self {
            http,
            url,
            token: token.to_string(),
            next_id: 0,
        }
    }

    /// Call a tool; returns `(is_error, text)`.
    async fn call(&mut self, tool: &str, arguments: Value) -> (bool, String) {
        self.next_id += 1;
        let body = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("X-Tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .send()
            .await
            .expect("tools/call")
            .text()
            .await
            .expect("body");
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
        let result = &message["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content: {message}"))
            .to_string();
        (result["isError"].as_bool().unwrap_or(false), text)
    }
}

/// The text of one section of the `get_repository` reply: from its
/// heading to the next heading.
fn section<'a>(reply: &'a str, heading: &str) -> &'a str {
    let start = reply
        .find(heading)
        .unwrap_or_else(|| panic!("no {heading:?} in {reply}"))
        + heading.len();
    let rest = &reply[start..];
    &rest[..rest.find("\n## ").unwrap_or(rest.len())]
}

async fn team(svc: &KairosClient, slug: &str, prefix: &str) -> (String, String) {
    let team = svc
        .create_team(&CreateTeamRequest {
            name: slug.into(),
            slug: slug.into(),
            code_prefix: prefix.into(),
            team_type: None,
        })
        .await
        .unwrap_or_else(|e| panic!("team {slug}: {e}"));
    let board = svc
        .create_board(&CreateBoardRequest {
            name: format!("{slug} ADRs"),
            slug: format!("{slug}-adrs"),
            board_level: "adr".into(),
            team_id: Some(team.id.clone()),
            code_prefix: prefix.into(),
        })
        .await
        .unwrap_or_else(|e| panic!("the ADR board of {slug}: {e}"));
    (team.id, board.board.id)
}

async fn repository(svc: &KairosClient, slug: &str, team: &str) {
    svc.create_repository(&CreateRepositoryRequest {
        slug: Some(slug.into()),
        forge: "github".into(),
        repo_full_name: format!("acme/{slug}"),
        repo_url: format!("https://github.com/acme/{slug}"),
        default_branch: None,
        team: team.into(),
        description: None,
    })
    .await
    .unwrap_or_else(|e| panic!("repository {slug}: {e}"));
}

async fn adr(svc: &KairosClient, board: &str, title: &str) -> Adr {
    svc.create_adr(&CreateAdrRequest {
        board_id: Some(board.into()),
        column_id: None,
        title: title.into(),
        content: "The decision.".into(),
        decision_maker: None,
        decision_date: None,
    })
    .await
    .unwrap_or_else(|e| panic!("ADR {title:?}: {e}"))
}

/// Put an ADR in a column of its board, in the table (a fixture step).
fn put_in_column(conn: &mut PgConnection, short_code: &str, column: &str) {
    let changed = diesel::sql_query(
        "UPDATE org_acme.adrs a SET column_id = c.id \
           FROM org_acme.board_columns c \
          WHERE a.short_code = $1 AND c.board_id = a.board_id AND c.name = $2",
    )
    .bind::<Text, _>(short_code)
    .bind::<Text, _>(column)
    .execute(conn)
    .expect("moving the ADR");
    assert_eq!(changed, 1, "{short_code} to {column}");
}

#[tokio::test]
async fn repository_lists_the_adrs_of_the_owner_team() {
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
    let svc_token = user_token(&http, "svc").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let router = app::router(app::state_with(base_config(&scratch_url), pool, auth));
    let server = spawn_server(router).await;
    let svc = server.client(&svc_token, "acme");
    let _ = svc.whoami().await;
    let svc_id: Uuid = users::table
        .filter(users::email.eq("svc@kairos.test"))
        .select(users::id)
        .first(&mut conn)
        .expect("svc provisioned");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: svc_id,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("granting membership");

    let (platform, platform_adrs) = team(&svc, "platform", "PLAT").await;
    let (_, web_adrs) = team(&svc, "web", "WEB").await;
    repository(&svc, "app", &platform).await;
    repository(&svc, "lib", &platform).await;

    let linked = adr(&svc, &platform_adrs, "Linked to app").await;
    svc.add_impact(EntityKind::Adr, &linked.short_code, "app")
        .await
        .expect("linked impacts app");
    let of_lib = adr(&svc, &platform_adrs, "Linked to lib").await;
    svc.add_impact(EntityKind::Adr, &of_lib.short_code, "lib")
        .await
        .expect("of_lib impacts lib");
    let free = adr(&svc, &platform_adrs, "No link").await;
    let decided = adr(&svc, &platform_adrs, "No link, decided").await;
    put_in_column(&mut conn, &decided.short_code, "Decided");
    let superseded = adr(&svc, &platform_adrs, "No link, superseded").await;
    put_in_column(&mut conn, &superseded.short_code, "Superseded");
    let archived = adr(&svc, &platform_adrs, "No link, archived").await;
    svc.delete_adr(&archived.short_code).await.expect("archive");
    let of_web = adr(&svc, &web_adrs, "Of web").await;

    let mut mcp = McpSession::open(&server.base_url, &svc_token).await;

    // ---- MCP: each ADR in its section, once --------------------------------
    let (is_error, reply) = mcp
        .call("get_repository", json!({"repository": "app"}))
        .await;
    assert!(!is_error, "{reply}");
    assert_eq!(
        section(&reply, IMPACT_HEADING),
        format!(
            "- {} — Linked to app · adr · column: Draft\n",
            linked.short_code
        ),
        "{reply}"
    );
    assert_eq!(
        section(&reply, TEAM_HEADING),
        format!(
            "The ADRs on the boards of the team platform that no `impacts` link names, \
             newest first. A superseded ADR is not listed.\n\
             - {} — No link, decided · adr · column: Decided\n\
             - {} — No link · adr · column: Draft\n",
            decided.short_code, free.short_code
        ),
        "{reply}"
    );
    for code in [&linked.short_code, &free.short_code, &decided.short_code] {
        assert_eq!(reply.matches(code.as_str()).count(), 1, "{code}: {reply}");
    }
    for code in [
        &of_lib.short_code,
        &superseded.short_code,
        &archived.short_code,
        &of_web.short_code,
    ] {
        assert!(!reply.contains(code.as_str()), "{code}: {reply}");
    }

    // The ADR that impacts lib is in the first section of lib.
    let (_, lib) = mcp
        .call("get_repository", json!({"repository": "lib"}))
        .await;
    assert!(
        section(&lib, IMPACT_HEADING).contains(&of_lib.short_code),
        "{lib}"
    );
    assert!(
        !section(&lib, TEAM_HEADING).contains(&of_lib.short_code),
        "{lib}"
    );

    // include_deleted adds the archived ADR, marked.
    let (_, with_archived) = mcp
        .call(
            "get_repository",
            json!({"repository": "app", "include_deleted": true}),
        )
        .await;
    assert!(
        section(&with_archived, TEAM_HEADING).contains(&format!(
            "- {} — No link, archived · adr · column: Draft [archived]\n",
            archived.short_code
        )),
        "{with_archived}"
    );

    // ---- REST: the same rows ----------------------------------------------
    let detail = svc.get_repository("app").await.expect("the repository");
    let codes = |items: &[kairos_client::types_repositories::ImpactingItem]| {
        items
            .iter()
            .map(|item| (item.short_code.clone(), item.column.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        codes(&detail.impacted_by),
        vec![(linked.short_code.clone(), Some("Draft".to_string()))]
    );
    assert_eq!(
        codes(&detail.owner_team_adrs),
        vec![
            (decided.short_code.clone(), Some("Decided".to_string())),
            (free.short_code.clone(), Some("Draft".to_string())),
        ]
    );
    assert_eq!(detail.owner_team_adrs_total, 2);
    let detail = svc
        .get_repository_with_archived("app")
        .await
        .expect("with archived");
    assert_eq!(detail.owner_team_adrs_total, 3);
    assert!(
        detail
            .owner_team_adrs
            .iter()
            .any(|item| item.short_code == archived.short_code && item.archived_at.is_some())
    );

    // ---- the cap: 50, newest first, and a line for the rest ---------------
    let mut newest = String::new();
    for n in 0..49 {
        newest = adr(&svc, &platform_adrs, &format!("Bulk {n}"))
            .await
            .short_code;
    }
    let detail = svc.get_repository("app").await.expect("the repository");
    assert_eq!(detail.owner_team_adrs.len(), 50);
    assert_eq!(detail.owner_team_adrs_total, 51);
    assert_eq!(detail.owner_team_adrs[0].short_code, newest);
    assert!(
        detail
            .owner_team_adrs
            .iter()
            .all(|item| item.short_code != free.short_code),
        "the oldest ADR is the one cut"
    );
    let (_, reply) = mcp
        .call("get_repository", json!({"repository": "app"}))
        .await;
    let team_section = section(&reply, TEAM_HEADING);
    assert_eq!(team_section.matches(" · adr · ").count(), 50, "{reply}");
    assert!(
        team_section.ends_with("- 1 more: `team_pages platform` lists each ADR of the team.\n"),
        "{reply}"
    );

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
