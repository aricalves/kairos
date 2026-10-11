//! The GitHub write-back (KAIROS-T-0386, COLLIERY-I-0614), against a fake
//! GitHub API (a local axum server; no request leaves the host).
//!
//! 1. No write token: a linked pull request gets nothing, and the
//!    repository, `GET …/write-token` and `get_repository` say write-back
//!    is off.
//! 2. A write token (sealed, never shown): each open pull request that
//!    links to an item gets one comment (code, title, column, claim, link)
//!    and the status `kairos` on its head commit.
//! 3. Each change of the item (column, claim) edits the same comment in
//!    place and sets the status again; no change sends nothing; a new head
//!    commit gets the status; a comment that a person removed is made
//!    again.
//! 4. A write that fails is tried again, and never blocks the change of the
//!    item; done is `success`, cancelled is `failure`.
//! 5. With the token removed, nothing is written.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_github_write_back_t0386_test`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::github_http::{Call, GithubHttp};
use common::{
    AUDIENCE, ISSUER, base_config, recreate_scratch_db, spawn_server, user_token, with_database,
};
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_auth::Secret;
use kairos_client::types_org::CreateTeamRequest;
use kairos_client::types_repositories::CreateRepositoryRequest;
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::forge::auth::github_signature;
use kairos_server::jobs::{Worker, kinds};
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;

const SCRATCH_DB: &str = "kairos_github_write_back_t0386_test";
const TOKEN: &str = "github_pat_11FAKEWRITETOKEN0386";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

/// A GitHub `pull_request` payload for PR `number` naming `codes`.
fn github_pr(number: i64, codes: &[&str], updated_at: &str) -> String {
    json!({
        "action": "opened",
        "repository": {
            "full_name": "acme/site",
            "html_url": "https://github.com/acme/site"
        },
        "sender": { "login": "robin" },
        "pull_request": {
            "number": number,
            "state": "open",
            "merged": false,
            "draft": false,
            "title": format!("Work on {}", codes.join(" ")),
            "body": "",
            "html_url": format!("https://github.com/acme/site/pull/{number}"),
            "updated_at": updated_at,
            "user": { "login": "robin" },
            "head": { "ref": "robin/work" }
        }
    })
    .to_string()
}

/// Run the worker until no write-back job is open.
async fn drain(worker: &Arc<Worker>, url: &str) {
    let url = url.to_string();
    let run = async {
        loop {
            worker.pass().await;
            let open = open_jobs(&url);
            if open == 0 && worker.running() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the write-back ended in time");
}

fn open_jobs(url: &str) -> usize {
    let mut conn = PgConnection::establish(url).expect("connect");
    kairos_db::jobs::list(&mut conn, kinds::GITHUB_WRITEBACK)
        .expect("jobs")
        .iter()
        .filter(|j| j.state == "queued" || j.state == "leased")
        .count()
}

fn all_jobs(url: &str) -> usize {
    let mut conn = PgConnection::establish(url).expect("connect");
    kairos_db::jobs::list(&mut conn, kinds::GITHUB_WRITEBACK)
        .expect("jobs")
        .len()
}

/// The calls of the fake API after the first `from`.
fn since(github: &GithubHttp, from: usize) -> Vec<Call> {
    github.calls().into_iter().skip(from).collect()
}

fn of<'a>(calls: &'a [Call], method: &str, path_part: &str) -> Vec<&'a Call> {
    calls
        .iter()
        .filter(|c| c.method == method && c.path.contains(path_part))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_pull_request_gets_one_comment_edited_in_place_and_the_kairos_status() {
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

    let github = common::github_http::serve().await;
    github.set_head(7, "aaaa000000000000000000000000000000000007");
    github.set_head(8, "bbbb000000000000000000000000000000000008");

    let http = reqwest::Client::new();
    let svc_token = user_token(&http, "svc").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([3u8; 32]));
    config.writeback.api_url = github.url.clone();
    config.writeback.retry_base = Duration::from_millis(10);
    config.jobs.retry_after = Duration::from_millis(20);
    let state = app::state_with(config, pool, auth);
    let worker = Arc::clone(&state.jobs);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let _ = admin.whoami().await;
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: svc_id,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("granting admin");

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
    let detail = admin.get_board(&board).await.expect("board");
    let column = |name: &str| {
        detail
            .columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.id.clone())
            .expect("column")
    };
    let repo = admin
        .create_repository(&CreateRepositoryRequest {
            slug: Some("site".into()),
            forge: "github".into(),
            repo_full_name: "acme/site".into(),
            repo_url: "https://github.com/acme/site".into(),
            default_branch: None,
            team: team.id.clone(),
            description: None,
        })
        .await
        .expect("repository");
    let connection = admin
        .create_forge_connection(&kairos_client::types_forge::CreateForgeConnectionRequest {
            repository: repo.slug.clone(),
        })
        .await
        .expect("connection");
    let secret = connection.webhook_secret.clone();
    let hook = format!(
        "{}/webhooks/github/acme/{}",
        server.base_url, connection.connection.id
    );
    let deliver = |body: String| {
        let http = http.clone();
        let hook = hook.clone();
        let signature = github_signature(&secret, body.as_bytes());
        async move {
            let response = http
                .post(hook)
                .header("x-github-event", "pull_request")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .expect("delivery");
            assert_eq!(response.status(), 200);
        }
    };
    let new_task = |title: &'static str| {
        let admin = &admin;
        let board = board.clone();
        async move {
            admin
                .create_task(&CreateTaskRequest {
                    board_id: Some(board),
                    column_id: None,
                    title: title.to_string(),
                    content: String::new(),
                    task_type: None,
                    work_class: None,
                    team_id: None,
                    repository: None,
                })
                .await
                .expect("task")
                .short_code
        }
    };
    let a = new_task("Fix the login").await;
    let b = new_task("Drop the old page").await;

    // --- 1. No write token: nothing is written -----------------------------
    deliver(github_pr(7, &[&a], "2026-10-12T10:00:00Z")).await;
    drain(&worker, &scratch_url).await;
    assert_eq!(all_jobs(&scratch_url), 0, "no token, no job");
    assert!(github.calls().is_empty(), "no token, no request");
    let status = admin
        .get_repository_write_back(&repo.slug)
        .await
        .expect("status");
    assert!(!status.enabled);
    assert_eq!(status.summary(), "off (no write token)");
    let listed = admin.get_repository(&repo.slug).await.expect("repository");
    assert!(!listed.repository.write_back.enabled);
    let mcp_text = mcp_get_repository(&server.base_url, &svc_token, &repo.slug).await;
    assert!(
        mcp_text.contains("- write-back: off (no write token)"),
        "{mcp_text}"
    );

    // --- 2. The token: the open pull request gets its comment and status --
    let status = admin
        .set_repository_write_token(&repo.slug, &Secret::new(TOKEN.to_string()))
        .await
        .expect("set the token");
    assert!(status.enabled);
    assert!(!serde_json::to_string(&status).unwrap().contains(TOKEN));
    let raw = http
        .get(format!(
            "{}/api/repositories/site/write-token",
            server.base_url
        ))
        .bearer_auth(&svc_token)
        .header("x-tenant", "acme")
        .send()
        .await
        .expect("get")
        .text()
        .await
        .expect("body");
    assert!(!raw.contains(TOKEN), "the token is never shown: {raw}");
    {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Bytea)]
            ciphertext: Vec<u8>,
        }
        let row: Row = diesel::sql_query("SELECT ciphertext FROM org_acme.repository_write_tokens")
            .get_result(&mut conn)
            .expect("the sealed token");
        assert!(
            !row.ciphertext
                .windows(TOKEN.len())
                .any(|w| w == TOKEN.as_bytes()),
            "the row holds no plain token"
        );
    }
    drain(&worker, &scratch_url).await;
    let calls = github.calls();
    let made = of(&calls, "POST", "/repos/acme/site/issues/7/comments");
    assert_eq!(made.len(), 1, "{calls:#?}");
    assert!(
        calls
            .iter()
            .all(|c| c.authorization == format!("Bearer {TOKEN}"))
    );
    let body = made[0].body["body"].as_str().unwrap().to_string();
    assert!(body.contains(&a), "{body}");
    assert!(body.contains("Fix the login"), "{body}");
    assert!(body.contains("| Backlog | - |"), "{body}");
    assert!(
        body.contains(&format!("https://kairos.test/items/{a}")),
        "{body}"
    );
    let statuses = of(&calls, "POST", "/statuses/aaaa");
    assert_eq!(statuses.len(), 1, "{calls:#?}");
    assert_eq!(statuses[0].body["state"], "pending");
    assert_eq!(statuses[0].body["context"], "kairos");
    assert_eq!(
        statuses[0].body["target_url"],
        format!("https://kairos.test/items/{a}")
    );
    let comment_id = 1000;

    // --- 3. A change edits the same comment, and sets the status ----------
    let before = github.calls().len();
    admin
        .transition_task(&a, &column("Todo"))
        .await
        .expect("to Todo");
    admin
        .transition_task(&a, &column("Active"))
        .await
        .expect("to Active");
    drain(&worker, &scratch_url).await;
    let calls = since(&github, before);
    assert!(of(&calls, "POST", "/comments").is_empty(), "{calls:#?}");
    let edits = of(&calls, "PATCH", &format!("/issues/comments/{comment_id}"));
    assert!(!edits.is_empty(), "{calls:#?}");
    let text = github.comment(comment_id).expect("the comment");
    assert!(text.contains("| Active |"), "{text}");
    assert!(text.contains(", since "), "the claim: {text}");
    let statuses = of(&calls, "POST", "/statuses/");
    assert!(!statuses.is_empty());
    assert_eq!(statuses.last().unwrap().body["state"], "pending");
    assert!(
        statuses.last().unwrap().body["description"]
            .as_str()
            .unwrap()
            .contains("Active"),
    );

    // No change: the comment and the status are not sent again.
    let before = github.calls().len();
    deliver(github_pr(7, &[&a], "2026-10-12T11:00:00Z")).await;
    drain(&worker, &scratch_url).await;
    let calls = since(&github, before);
    assert!(of(&calls, "PATCH", "/comments").is_empty(), "{calls:#?}");
    assert!(of(&calls, "POST", "/statuses/").is_empty(), "{calls:#?}");
    assert_eq!(of(&calls, "GET", "/pulls/7").len(), 1);

    // A new head commit (a push to the pull request) gets the status.
    github.set_head(7, "cccc000000000000000000000000000000000007");
    let before = github.calls().len();
    deliver(github_pr(7, &[&a], "2026-10-12T12:00:00Z")).await;
    drain(&worker, &scratch_url).await;
    let calls = since(&github, before);
    assert_eq!(of(&calls, "POST", "/statuses/cccc").len(), 1, "{calls:#?}");

    // A person removed the comment on GitHub: the next change makes a new
    // one.
    github.remove_comment(comment_id);
    let before = github.calls().len();
    admin.release_task(&a).await.expect("release the claim");
    drain(&worker, &scratch_url).await;
    let calls = since(&github, before);
    assert_eq!(of(&calls, "PATCH", "/comments/1000").len(), 1);
    assert_eq!(
        of(&calls, "POST", "/issues/7/comments").len(),
        1,
        "{calls:#?}"
    );
    let text = github.comment(1001).expect("the new comment");
    assert!(text.contains("| Active | - |"), "no claim now: {text}");

    // --- 4. A failed write is tried again; done is success -----------------
    github.fail_next(2);
    let before = github.calls().len();
    let started = std::time::Instant::now();
    admin
        .transition_task(&a, &column("Completed"))
        .await
        .expect("to Completed");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the change does not wait for GitHub"
    );
    drain(&worker, &scratch_url).await;
    let calls = since(&github, before);
    assert!(calls.len() >= 4, "{calls:#?}");
    let statuses = of(&calls, "POST", "/statuses/cccc");
    assert_eq!(
        statuses.last().unwrap().body["state"],
        "success",
        "{calls:#?}"
    );
    let text = github.comment(1001).expect("the comment");
    assert!(text.contains("Completed (done)"), "{text}");
    let status = admin
        .get_repository_write_back(&repo.slug)
        .await
        .expect("status");
    assert_eq!(status.last_write_ok, Some(true), "{status:?}");

    // A cancelled item is failure.
    deliver(github_pr(8, &[&b], "2026-10-12T13:00:00Z")).await;
    drain(&worker, &scratch_url).await;
    let before = github.calls().len();
    admin
        .cancel_task(&b, "The page stays")
        .await
        .expect("cancel");
    drain(&worker, &scratch_url).await;
    let calls = since(&github, before);
    let statuses = of(&calls, "POST", "/statuses/bbbb");
    assert_eq!(statuses.len(), 1, "{calls:#?}");
    assert_eq!(statuses[0].body["state"], "failure");
    assert!(
        statuses[0].body["description"]
            .as_str()
            .unwrap()
            .contains(&b)
    );
    let edits = of(&calls, "PATCH", "/comments/");
    assert!(
        edits[0].body["body"]
            .as_str()
            .unwrap()
            .contains("cancelled: The page stays")
    );

    // KAIROS_SECRETS_KEY rotates: the write token is sealed again.
    let ring = SecretsKey::from_bytes([4u8; 32]).with_old_bytes([3u8; 32]);
    let report =
        kairos_server::secrets_rotation::rotate_tenant(&mut conn, "acme", &ring).expect("rotate");
    assert_eq!(report.resealed, 1, "{report:?}");

    // --- 5. No token: nothing more ------------------------------------------
    let status = admin
        .remove_repository_write_token(&repo.slug)
        .await
        .expect("remove the token");
    assert!(!status.enabled);
    let jobs = all_jobs(&scratch_url);
    let before = github.calls().len();
    deliver(github_pr(7, &[&a], "2026-10-12T14:00:00Z")).await;
    drain(&worker, &scratch_url).await;
    assert_eq!(all_jobs(&scratch_url), jobs, "no token, no job");
    assert_eq!(github.calls().len(), before, "no token, no request");
}

/// The text of `get_repository` over MCP.
async fn mcp_get_repository(base_url: &str, token: &str, slug: &str) -> String {
    let http = reqwest::Client::new();
    let url = format!("{base_url}/mcp");
    let post = |body: Value, session: Option<String>| {
        let mut request = http
            .post(&url)
            .bearer_auth(token)
            .header("x-tenant", "acme")
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        request.send()
    };
    let response = post(
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "kairos-t0386", "version": "0.0.0"},
            },
        }),
        None,
    )
    .await
    .expect("initialize");
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_string());
    let _ = response.text().await;
    let _ = post(
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        session.clone(),
    )
    .await
    .expect("initialized");
    let body = post(
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "get_repository", "arguments": {"repository": slug}},
        }),
        session,
    )
    .await
    .expect("call")
    .text()
    .await
    .expect("body");
    let message: Value = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|value| value.get("jsonrpc").is_some())
        .or_else(|| serde_json::from_str(&body).ok())
        .expect("a JSON-RPC message");
    message["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
