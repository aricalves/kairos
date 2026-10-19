//! The issue write-back (KAIROS-T-0575): a task born from a GitHub or
//! GitLab issue comments on the issue and closes it when the task is done
//! or cancelled, and reopens it when the task is reopened. Against a fake
//! forge API (a local axum server; no request leaves the host), with the
//! recorded issue fixtures of the intake.
//!
//! 1. No write token: a done task writes nothing, and no job is queued.
//! 2. A new write token writes the issue whose task is done: one comment
//!    ("Done in <code>; PR #37.", no link to a private host) and the
//!    issue closed as completed. No change writes nothing again.
//! 3. A reopen opens the issue, with no comment.
//! 4. A cancel writes the reason and closes the issue as not planned. A
//!    failed close is tried again with no second comment.
//! 5. GitLab: a note and `state_event` close or reopen, with the private
//!    token. A refusal (403) is not tried again, and the token row names
//!    the scope.
//! 6. With the token removed, nothing is written.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_forge_issue_writeback_t0575_test`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use serde_json::{Value, json};
use uuid::Uuid;

use common::github_http::{Call, GithubHttp};
use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_client::types_auth::Secret;
use kairos_client::types_forge::CreateForgeConnectionRequest;
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

const SCRATCH_DB: &str = "kairos_forge_issue_writeback_t0575_test";
const GITHUB_TOKEN: &str = "github_pat_11FAKEISSUETOKEN0575";
const GITLAB_TOKEN: &str = "glpat-FAKEISSUETOKEN0575";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../kairos-core/tests/fixtures/forge/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
}

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .expect("user row")
}

fn jobs_of(url: &str, kind: &str) -> Vec<kairos_db::jobs::Job> {
    let mut conn = PgConnection::establish(url).expect("connect");
    kairos_db::jobs::list(&mut conn, kind).expect("jobs")
}

/// Run the worker until no write-back job is open.
async fn drain(worker: &Arc<Worker>, url: &str) {
    let run = async {
        loop {
            worker.pass().await;
            let open = [kinds::ISSUE_WRITEBACK, kinds::GITHUB_WRITEBACK]
                .iter()
                .flat_map(|kind| jobs_of(url, kind))
                .filter(|j| j.state == "queued" || j.state == "leased")
                .count();
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

/// The calls of the fake API to the issue `path_part`, after the first
/// `from` calls.
fn issue_calls(forge: &GithubHttp, from: usize, path_part: &str) -> Vec<Call> {
    forge
        .calls()
        .into_iter()
        .skip(from)
        .filter(|c| c.path.contains(path_part))
        .collect()
}

#[derive(QueryableByName, Debug)]
struct IntakeRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    item_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    short_code: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    written_state: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    comment_state: Option<String>,
}

fn intake(conn: &mut PgConnection, number: i64) -> IntakeRow {
    sql_query(
        "SELECT i.item_id, t.short_code, i.written_state, i.comment_state \
           FROM org_acme.forge_issue_intake i JOIN org_acme.tasks t ON t.id = i.item_id \
          WHERE i.number = $1",
    )
    .bind::<diesel::sql_types::BigInt, _>(number)
    .get_result(conn)
    .expect("intake row")
}

#[derive(QueryableByName, Debug)]
struct LastWrite {
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Bool>)]
    last_write_ok: Option<bool>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    last_write_error: Option<String>,
}

fn last_write(conn: &mut PgConnection, slug: &str) -> LastWrite {
    sql_query(
        "SELECT w.last_write_ok, w.last_write_error \
           FROM org_acme.repository_write_tokens w \
           JOIN org_acme.repositories r ON r.id = w.repository_id WHERE r.slug = $1",
    )
    .bind::<diesel::sql_types::Text, _>(slug)
    .get_result(conn)
    .expect("token row")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_born_from_an_issue_comments_on_it_and_closes_or_reopens_it() {
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

    let forge = common::github_http::serve().await;
    let http = reqwest::Client::new();
    let svc_token = user_token(&http, "svc").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([7u8; 32]));
    config.writeback.api_url = forge.url.clone();
    config.writeback.retry_base = Duration::from_millis(10);
    config.jobs.retry_after = Duration::from_millis(20);
    // A tailnet name: the webhooks work, and the comments name the code with
    // no link (an issue can be public).
    config.public_url = Some("https://kairos.tail0000.ts.net".to_string());
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
            name: "Payments".into(),
            slug: "payments".into(),
            code_prefix: "PAY".into(),
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
    // The GitLab API is on the host of the URL of the repository: the fake.
    let mut hooks = Vec::new();
    for (forge_name, name, slug, url) in [
        (
            "github",
            "acme/payments-api",
            "payments-api",
            "https://github.com/acme/payments-api".to_string(),
        ),
        (
            "gitlab",
            "acme/portal-web",
            "portal-web",
            format!("{}/acme/portal-web", forge.url),
        ),
    ] {
        admin
            .create_repository(&CreateRepositoryRequest {
                slug: Some(slug.into()),
                forge: forge_name.into(),
                repo_full_name: name.into(),
                repo_url: url,
                default_branch: None,
                team: team.id.clone(),
                description: None,
            })
            .await
            .expect("repository");
        let created = admin
            .create_forge_connection(&CreateForgeConnectionRequest {
                repository: slug.into(),
            })
            .await
            .expect("connection");
        hooks.push((
            format!(
                "{}/webhooks/{forge_name}/acme/{}",
                server.base_url, created.connection.id
            ),
            created.webhook_secret,
        ));
    }
    let deliver = |forge_name: &'static str, event: &'static str, body: String| {
        let http = http.clone();
        let (url, secret) = if forge_name == "github" {
            hooks[0].clone()
        } else {
            hooks[1].clone()
        };
        async move {
            let request = http
                .post(url)
                .header("content-type", "application/json")
                .body(body.clone());
            let request = if forge_name == "github" {
                request.header("x-github-event", event).header(
                    "x-hub-signature-256",
                    github_signature(&secret, body.as_bytes()),
                )
            } else {
                request
                    .header("x-gitlab-event", event)
                    .header("x-gitlab-token", secret)
            };
            let response = request.send().await.expect("delivery");
            assert_eq!(response.status(), 200);
            response.json::<Value>().await.expect("json")
        }
    };

    // The intake: GitHub issue #12 and GitLab issue 4 become tasks.
    let answer = deliver("github", "issues", fixture("github_issue_opened.json")).await;
    assert_eq!(answer["status"], "issue taken", "{answer}");
    let answer = deliver("gitlab", "Issue Hook", fixture("gitlab_issue_open.json")).await;
    assert_eq!(answer["status"], "issue taken", "{answer}");
    let gh = intake(&mut conn, 12);
    let gl = intake(&mut conn, 4);
    assert_eq!(gh.written_state, "open", "an intake issue is open");
    assert_eq!(gh.comment_state, None);
    // PR #37 of the same repository fixes the GitHub task.
    let pr = json!({
        "action": "opened",
        "repository": { "full_name": "acme/payments-api" },
        "sender": { "login": "robin" },
        "pull_request": {
            "number": 37, "state": "open", "merged": false, "draft": false,
            "title": format!("Fix the crash {}", gh.short_code), "body": "",
            "html_url": "https://github.com/acme/payments-api/pull/37",
            "updated_at": "2026-10-18T09:00:00Z",
            "user": { "login": "robin" }, "head": { "ref": "robin/fix" }
        }
    });
    deliver("github", "pull_request", pr.to_string()).await;

    // =======================================================================
    // 1. No write token: a done task writes nothing
    // =======================================================================
    for to in ["Todo", "Active", "Completed"] {
        admin
            .transition_task(&gh.short_code, &column(to))
            .await
            .unwrap_or_else(|e| panic!("move to {to}: {e}"));
    }
    drain(&worker, &scratch_url).await;
    assert!(
        jobs_of(&scratch_url, kinds::ISSUE_WRITEBACK).is_empty(),
        "no token: no job"
    );
    assert!(issue_calls(&forge, 0, "/issues/12").is_empty());
    assert_eq!(intake(&mut conn, 12).written_state, "open");

    // =======================================================================
    // 2. A new write token writes the done issue: one comment, then closed
    // =======================================================================
    admin
        .set_repository_write_token("payments-api", &Secret::new(GITHUB_TOKEN.to_string()))
        .await
        .expect("write token");
    drain(&worker, &scratch_url).await;
    let calls = issue_calls(&forge, 0, "/repos/acme/payments-api/issues/12");
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert_eq!(
        (calls[0].method.as_str(), calls[0].path.as_str()),
        ("POST", "/repos/acme/payments-api/issues/12/comments")
    );
    let body = calls[0].body["body"].as_str().expect("comment body");
    assert!(body.starts_with("<!-- kairos:issue-writeback "), "{body}");
    assert!(
        body.contains(&format!("Done in {}; PR #37.", gh.short_code)),
        "{body}"
    );
    assert!(
        !body.contains("http") && !body.contains("ts.net"),
        "no link to a private host: {body}"
    );
    assert_eq!(calls[0].authorization, format!("Bearer {GITHUB_TOKEN}"));
    assert_eq!(
        (calls[1].method.as_str(), calls[1].path.as_str()),
        ("PATCH", "/repos/acme/payments-api/issues/12")
    );
    assert_eq!(
        calls[1].body,
        json!({"state": "closed", "state_reason": "completed"})
    );
    let row = intake(&mut conn, 12);
    assert_eq!(
        (row.written_state.as_str(), row.comment_state.as_deref()),
        ("completed", Some("completed"))
    );
    assert_eq!(
        last_write(&mut conn, "payments-api").last_write_ok,
        Some(true)
    );
    // A change that does not change the state queues nothing.
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    assert_eq!(
        kairos_db::issue_writeback::on_item_changed(&mut conn, row.item_id).expect("queue"),
        0,
        "the issue has the state of the task"
    );

    // =======================================================================
    // 3. A reopen opens the issue, with no comment
    // =======================================================================
    let from = forge.calls().len();
    admin.reopen_task(&gh.short_code).await.expect("reopen");
    drain(&worker, &scratch_url).await;
    let calls = issue_calls(&forge, from, "/repos/acme/payments-api/issues/12");
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0].method, "PATCH");
    assert_eq!(calls[0].body, json!({"state": "open"}));
    let row = intake(&mut conn, 12);
    assert_eq!(
        (row.written_state.as_str(), row.comment_state.as_deref()),
        ("open", None)
    );

    // =======================================================================
    // 4. A cancel writes the reason and closes as not planned; a failed
    //    close is tried again with no second comment
    // =======================================================================
    let from = forge.calls().len();
    forge.fail_next_on("PATCH", "/repos/acme/payments-api/issues/12", 1);
    admin
        .cancel_task(&gh.short_code, "Duplicate of #9")
        .await
        .expect("cancel");
    drain(&worker, &scratch_url).await;
    let calls = issue_calls(&forge, from, "/repos/acme/payments-api/issues/12");
    let comments: Vec<&Call> = calls.iter().filter(|c| c.method == "POST").collect();
    let closes: Vec<&Call> = calls.iter().filter(|c| c.method == "PATCH").collect();
    assert_eq!(comments.len(), 1, "one comment over 2 tries: {calls:#?}");
    let body = comments[0].body["body"].as_str().expect("comment body");
    assert!(
        body.contains(&format!("Cancelled in {}: Duplicate of #9.", gh.short_code)),
        "{body}"
    );
    assert_eq!(closes.len(), 2, "the failed close is tried again");
    assert_eq!(
        closes[1].body,
        json!({"state": "closed", "state_reason": "not_planned"})
    );
    assert_eq!(intake(&mut conn, 12).written_state, "not_planned");
    let job = jobs_of(&scratch_url, kinds::ISSUE_WRITEBACK)
        .into_iter()
        .last()
        .expect("the cancel job");
    assert_eq!((job.state.as_str(), job.tries), ("done", 2), "{job:?}");

    // =======================================================================
    // 5. GitLab: a note and state_event, with the private token; a refusal
    //    is not tried again
    // =======================================================================
    admin
        .set_repository_write_token("portal-web", &Secret::new(GITLAB_TOKEN.to_string()))
        .await
        .expect("a GitLab repository takes a write token");
    drain(&worker, &scratch_url).await;
    assert!(
        issue_calls(&forge, 0, "/api/v4/").is_empty(),
        "an open task writes nothing"
    );
    let gitlab_issue = "/api/v4/projects/acme%2Fportal-web/issues/4";
    admin
        .cancel_task(&gl.short_code, "Not planned")
        .await
        .expect("cancel");
    drain(&worker, &scratch_url).await;
    let calls = issue_calls(&forge, 0, gitlab_issue);
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert_eq!(
        (calls[0].method.as_str(), calls[0].path.as_str()),
        ("POST", "/api/v4/projects/acme%2Fportal-web/issues/4/notes")
    );
    assert!(
        calls[0].body["body"]
            .as_str()
            .unwrap_or("")
            .contains(&format!("Cancelled in {}: Not planned.", gl.short_code)),
        "{:?}",
        calls[0].body
    );
    assert_eq!(
        calls[0].authorization,
        format!("PRIVATE-TOKEN {GITLAB_TOKEN}")
    );
    assert_eq!(
        (calls[1].method.as_str(), calls[1].body.clone()),
        ("PUT", json!({"state_event": "close"}))
    );
    let from = forge.calls().len();
    admin.reopen_task(&gl.short_code).await.expect("reopen");
    drain(&worker, &scratch_url).await;
    let calls = issue_calls(&forge, from, gitlab_issue);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0].body, json!({"state_event": "reopen"}));

    // A refusal: one request, the job fails, the token row names the scope.
    let from = forge.calls().len();
    forge.refuse_next(1);
    for to in ["Todo", "Active", "Completed"] {
        admin
            .transition_task(&gl.short_code, &column(to))
            .await
            .unwrap_or_else(|e| panic!("move to {to}: {e}"));
    }
    drain(&worker, &scratch_url).await;
    let calls = issue_calls(&forge, from, gitlab_issue);
    assert_eq!(calls.len(), 1, "a refusal is not tried again: {calls:#?}");
    let job = jobs_of(&scratch_url, kinds::ISSUE_WRITEBACK)
        .into_iter()
        .last()
        .expect("the refused job");
    assert_eq!((job.state.as_str(), job.tries), ("failed", 1), "{job:?}");
    let written = last_write(&mut conn, "portal-web");
    assert_eq!(written.last_write_ok, Some(false));
    let error = written.last_write_error.unwrap_or_default();
    assert!(
        error.contains("GitLab refused") && error.contains("the scope api"),
        "{error}"
    );
    assert!(!error.contains(GITLAB_TOKEN), "no token in the error");
    assert_eq!(intake(&mut conn, 4).written_state, "open");

    // =======================================================================
    // 6. With the token removed, nothing is written
    // =======================================================================
    admin
        .remove_repository_write_token("payments-api")
        .await
        .expect("remove");
    let before = jobs_of(&scratch_url, kinds::ISSUE_WRITEBACK).len();
    let from = forge.calls().len();
    admin.reopen_task(&gh.short_code).await.expect("reopen");
    drain(&worker, &scratch_url).await;
    assert_eq!(
        jobs_of(&scratch_url, kinds::ISSUE_WRITEBACK).len(),
        before,
        "no token: no job"
    );
    assert!(issue_calls(&forge, from, "/issues/12").is_empty());

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
