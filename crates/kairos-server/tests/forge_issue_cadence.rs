//! Integration test for the cadence of the issue import (KAIROS-T-0670,
//! COLLIERY-I-0623): a repository takes its open issues on an interval, with
//! no webhook.
//!
//! The cadence is a sync. Each run lists the open issues and compares them
//! with the record of taken issues (`forge_issue_intake`). Sections:
//!
//! - the setting: org admin only, the form and the limits are refused with
//!   the field named, an unknown field is refused;
//! - 1: a new issue becomes one task within 2 runs, a run with no change
//!   makes none; the cadence runs with no webhook connection and no
//!   `KAIROS_PUBLIC_URL` (3);
//! - 1a: 3 issues while the cadence is off: the first run after it makes 3
//!   tasks, none missed and none twice;
//! - 1c: an old issue that is new to Kairos still becomes a task;
//! - 1b: a run that stops half way keeps its tasks, shows as not complete,
//!   and the next run makes the rest;
//! - 4: a rate limit makes the run wait; a refused token fails the run with
//!   the permission named, and the next run tries again;
//! - 2: a webhook and the cadence make one task for one issue;
//! - 5: two servers on one database run one import of a repository at a
//!   time.
//!
//! The forge is a local fake API (no network). Runs against the compose
//! stack. Owns the scratch database `kairos_forge_issue_cadence_t0670_test`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_client::KairosClient;
use kairos_client::types_auth::Secret;
use kairos_client::types_forge::CreateForgeConnectionRequest;
use kairos_client::types_org::CreateTeamRequest;
use kairos_client::types_repositories::{CreateRepositoryRequest, IssueImport};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::forge::auth::github_signature;
use kairos_server::forge::sync::queue_due;
use kairos_server::jobs::Worker;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;

const SCRATCH_DB: &str = "kairos_forge_issue_cadence_t0670_test";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("user {email} not provisioned: {e}"))
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

fn count(conn: &mut PgConnection, sql: &str) -> i64 {
    sql_query(sql)
        .get_result::<Count>(conn)
        .expect("count")
        .count
}

/// The record of a repository: the issues that Kairos took.
fn record(conn: &mut PgConnection, slug: &str) -> i64 {
    count(
        conn,
        &format!(
            "SELECT count(*) AS count FROM org_acme.forge_issue_intake i \
               JOIN org_acme.repositories r ON r.id = i.repository_id WHERE r.slug = '{slug}'"
        ),
    )
}

/// The support tasks that the intake made for a repository.
fn support_tasks(conn: &mut PgConnection, slug: &str) -> i64 {
    count(
        conn,
        &format!(
            "SELECT count(*) AS count FROM org_acme.tasks t \
               JOIN org_acme.forge_issue_intake i ON i.item_id = t.id \
               JOIN org_acme.repositories r ON r.id = i.repository_id \
              WHERE r.slug = '{slug}' AND t.task_type = 'support'"
        ),
    )
}

fn issue(id: u64, number: u64, title: &str, created_at: &str) -> Value {
    json!({
        "id": id,
        "number": number,
        "title": title,
        "body": "Text of the issue.",
        "html_url": format!("https://github.com/acme/payments-api/issues/{number}"),
        "state": "open",
        "user": { "login": "robin" },
        "created_at": created_at,
        "updated_at": created_at
    })
}

async fn set_interval(client: &KairosClient, slug: &str, interval: &str) -> (u16, Value) {
    client
        .raw_request(
            reqwest::Method::PATCH,
            &format!("/api/repositories/{slug}"),
            Some(&json!({ "issue_import_interval": interval })),
        )
        .await
        .expect("PATCH")
}

/// One look of the pass at the time `at`, then the jobs. The names of the
/// repositories that got a job.
async fn look(
    jobs: &Arc<Worker>,
    url: &str,
    at: DateTime<Utc>,
) -> kairos_server::forge::sync::Queued {
    let url = url.to_string();
    let queued = tokio::task::spawn_blocking(move || {
        let mut conn = PgConnection::establish(&url).expect("connection");
        conn.batch_execute("SET search_path TO org_acme, public")
            .expect("search_path");
        queue_due(&mut conn, "acme", at, 3).expect("the look")
    })
    .await
    .expect("the look task");
    jobs.drain().await;
    queued
}

async fn status(client: &KairosClient, slug: &str) -> IssueImport {
    client.issue_import(slug).await.expect("import status")
}

fn at(base: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
    base + chrono::Duration::seconds(secs)
}

#[tokio::test]
async fn issue_cadence_against_live_stack() {
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

    let forge = common::issues_http::serve().await;
    let http = reqwest::Client::new();
    let svc_token = user_token(&http, "svc").await;
    let bob_token = user_token(&http, "bob").await;
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    // Server 1: no KAIROS_PUBLIC_URL and no KAIROS_WEBHOOK_SIGNING_KEY. The
    // cadence needs neither (criterion 3).
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([5u8; 32]));
    config.writeback.api_url = forge.url.clone();
    config.public_url = None;
    config.webhook_signing_key = None;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let state = app::state_with(config, pool, Arc::clone(&auth));
    let jobs = Arc::clone(&state.jobs);
    let server = spawn_server(app::router(state)).await;
    // Server 2: the webhook is on (criterion 2).
    let mut config2 = base_config(&scratch_url);
    config2.secrets_key = Some(SecretsKey::from_bytes([5u8; 32]));
    config2.writeback.api_url = forge.url.clone();
    let pool2 = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let state2 = app::state_with(config2, pool2, Arc::clone(&auth));
    let jobs2 = Arc::clone(&state2.jobs);
    let server2 = spawn_server(app::router(state2)).await;

    let svc = server.client(&svc_token, "acme");
    let bob = server.client(&bob_token, "acme");
    let svc2 = server2.client(&svc_token, "acme");
    for client in [&svc, &bob] {
        let _ = client.whoami().await;
    }
    for (email, role) in [
        ("svc@kairos.test", OrgRole::Admin),
        ("bob@kairos.test", OrgRole::Member),
    ] {
        let id = user_id(&mut conn, email);
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let team = svc
        .create_team(&CreateTeamRequest {
            name: "Payments".into(),
            slug: "payments".into(),
            code_prefix: "PAY".into(),
            team_type: None,
        })
        .await
        .expect("team");
    for (name, slug) in [
        ("acme/payments-api", "payments-api"),
        ("acme/ledger", "ledger"),
        ("acme/atlas", "atlas"),
    ] {
        svc.create_repository(&CreateRepositoryRequest {
            slug: Some(slug.into()),
            forge: "github".into(),
            repo_full_name: name.into(),
            repo_url: format!("https://github.com/{name}"),
            default_branch: None,
            team: team.id.clone(),
            description: None,
        })
        .await
        .expect("repository");
        svc.set_repository_credential(slug, &Secret::new("github_pat_fixture0001"))
            .await
            .expect("read token");
    }

    // ---- the setting ---------------------------------------------------------
    let (status_code, body) = set_interval(&bob, "payments-api", "1m").await;
    assert_eq!(status_code, 403, "a member does not set it: {body}");
    for (bad, needle) in [
        ("often", "not valid"),
        ("30s", "shortest"),
        ("31d", "longest"),
    ] {
        let (status_code, body) = set_interval(&svc, "payments-api", bad).await;
        assert_eq!(status_code, 422, "{bad}: {body}");
        let text = body.to_string();
        assert!(
            text.contains("issue_import_interval") && text.contains(needle),
            "{bad}: {text}"
        );
    }
    let (status_code, body) = svc
        .raw_request(
            reqwest::Method::PATCH,
            "/api/repositories/payments-api",
            Some(&json!({ "issue_import_intervall": "1m" })),
        )
        .await
        .expect("PATCH");
    assert_eq!(status_code, 422, "{body}");
    assert!(
        body.to_string().contains("issue_import_intervall"),
        "{body}"
    );
    let repository = svc
        .get_repository("payments-api")
        .await
        .expect("repository");
    assert_eq!(repository.repository.issue_import_interval, "off");
    assert_eq!(repository.repository.issue_import_interval_secs, None);
    assert!(!repository.repository.has_webhook, "no webhook connection");
    let none = look(&jobs, &scratch_url, Utc::now()).await;
    assert!(none.queued.is_empty(), "the cadence is off: {none:?}");

    // ---- 1 and 3: a new issue becomes one task within 2 runs; no webhook ----
    forge.set_github(vec![]);
    let (status_code, body) = set_interval(&svc, "payments-api", "1m").await;
    assert_eq!(status_code, 200, "{body}");
    assert_eq!(body["issue_import_interval"], "1m");
    assert_eq!(body["issue_import_interval_secs"], 60);
    let t0 = Utc::now();
    let first = look(&jobs, &scratch_url, t0).await;
    assert_eq!(first.queued, ["payments-api"], "{first:?}");
    let ended = status(&svc, "payments-api").await;
    assert_eq!(ended.state, "done", "{ended:?}");
    assert_eq!(ended.interval, "1m");
    assert!(ended.sync.last_run_complete, "{ended:?}");
    assert!(ended.sync.last_complete_at.is_some());
    assert!(ended.sync.next_run_at.is_some());
    assert_eq!(ended.sync.record_count, 0);
    // Not due before its time
    forge.set_github(vec![issue(7001, 1, "First issue", "2026-10-01T09:00:00Z")]);
    let early = look(&jobs, &scratch_url, at(t0, 30)).await;
    assert!(
        early.queued.is_empty(),
        "not due after 30 seconds: {early:?}"
    );
    assert_eq!(record(&mut conn, "payments-api"), 0);
    // Run 1 after the issue
    let second = look(&jobs, &scratch_url, at(t0, 61)).await;
    assert_eq!(second.queued, ["payments-api"]);
    assert_eq!(record(&mut conn, "payments-api"), 1);
    assert_eq!(support_tasks(&mut conn, "payments-api"), 1);
    let run = status(&svc, "payments-api").await;
    assert_eq!(run.result.imported, 1, "{run:?}");
    assert_eq!(
        (run.sync.last_run_listed, run.sync.last_run_imported),
        (1, 1)
    );
    // Run 2: no change, no task
    let third = look(&jobs, &scratch_url, at(t0, 122)).await;
    assert_eq!(third.queued, ["payments-api"]);
    assert_eq!(record(&mut conn, "payments-api"), 1);
    assert_eq!(support_tasks(&mut conn, "payments-api"), 1);
    let run = status(&svc, "payments-api").await;
    assert_eq!(run.result.imported, 0, "{run:?}");
    assert_eq!(run.result.skipped, 1);
    assert_eq!(
        (run.sync.last_run_listed, run.sync.last_run_imported),
        (1, 0)
    );
    assert_eq!(run.sync.record_count, 1);
    // The task is a support task of the delivery board, made by Forge, and
    // its issue link has the repository and no connection.
    assert_eq!(
        count(
            &mut conn,
            "SELECT count(*) AS count FROM org_acme.item_links \
              WHERE kind = 'issue' AND connection_id IS NULL AND repository_id IS NOT NULL"
        ),
        1
    );
    let code = run_task_code(&mut conn);
    let (status_code, links) = svc
        .raw_request(
            reqwest::Method::GET,
            &format!("/api/tasks/{code}/links"),
            None,
        )
        .await
        .expect("links");
    assert_eq!(status_code, 200, "{links}");
    assert!(
        links
            .to_string()
            .contains("github.com/acme/payments-api/issues/1"),
        "{links}"
    );

    // ---- 1a: no gap. 3 issues while the cadence is off --------------------------
    let (status_code, _) = set_interval(&svc, "payments-api", "off").await;
    assert_eq!(status_code, 200);
    let mut rows = vec![issue(7001, 1, "First issue", "2026-10-01T09:00:00Z")];
    for n in 2..=4u64 {
        rows.push(issue(
            7000 + n,
            n,
            &format!("Issue {n}"),
            "2026-10-02T09:00:00Z",
        ));
    }
    forge.set_github(rows.clone());
    for minutes in [10, 20, 30] {
        let off = look(&jobs, &scratch_url, at(t0, minutes * 60)).await;
        assert!(off.queued.is_empty(), "the cadence is off: {off:?}");
    }
    assert_eq!(record(&mut conn, "payments-api"), 1);
    let off_state = svc.get_repository("payments-api").await.unwrap().repository;
    assert_eq!(off_state.issue_import_interval, "off");
    assert!(off_state.issue_sync.next_run_at.is_none(), "{off_state:?}");
    let (status_code, _) = set_interval(&svc, "payments-api", "1m").await;
    assert_eq!(status_code, 200);
    let back = look(&jobs, &scratch_url, at(t0, 40 * 60)).await;
    assert_eq!(back.queued, ["payments-api"]);
    let run = status(&svc, "payments-api").await;
    assert_eq!(run.result.imported, 3, "none missed: {run:?}");
    assert_eq!(run.result.skipped, 1, "none twice: {run:?}");
    assert_eq!(record(&mut conn, "payments-api"), 4);
    assert_eq!(support_tasks(&mut conn, "payments-api"), 4);

    // ---- 1c: the record decides, not a time ------------------------------------
    rows.push(issue(6000, 5, "An old issue", "2019-01-01T00:00:00Z"));
    forge.set_github(rows.clone());
    look(&jobs, &scratch_url, at(t0, 41 * 60 + 5)).await;
    let run = status(&svc, "payments-api").await;
    assert_eq!(
        run.result.imported, 1,
        "the old issue is new to Kairos: {run:?}"
    );
    assert_eq!(record(&mut conn, "payments-api"), 5);

    // ---- 1b: a run that stops half way ------------------------------------------
    // 150 new issues: page 1 has 100 (5 known + 95 new), page 2 has 55.
    for n in 0..150u64 {
        rows.push(issue(
            8000 + n,
            100 + n,
            &format!("Bulk issue {n}"),
            "2026-10-03T09:00:00Z",
        ));
    }
    forge.set_github(rows.clone());
    forge.refuse_at(2);
    look(&jobs, &scratch_url, at(t0, 42 * 60 + 10)).await;
    let half = status(&svc, "payments-api").await;
    assert_eq!(half.state, "failed", "{half:?}");
    assert!(!half.sync.last_run_complete, "not complete: {half:?}");
    assert_eq!(half.sync.last_run_imported, 95, "{half:?}");
    assert!(
        half.sync
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("Issues: read"),
        "{half:?}"
    );
    assert!(
        half.sync.last_complete_at.is_some(),
        "the earlier complete run stays"
    );
    let kept = record(&mut conn, "payments-api");
    assert_eq!(kept, 5 + 95, "the first page keeps its tasks");
    // The repository shows the error
    let shown = svc.get_repository("payments-api").await.unwrap().repository;
    assert!(shown.issue_sync.last_error.is_some());
    assert!(!shown.issue_sync.last_run_complete);
    // The next run makes the rest, and no second task for the first page
    forge.refuse_at(0);
    look(&jobs, &scratch_url, at(t0, 44 * 60)).await;
    let done = status(&svc, "payments-api").await;
    assert_eq!(done.state, "done", "{done:?}");
    assert_eq!(done.result.imported, 55, "{done:?}");
    assert_eq!(done.result.skipped, 100, "{done:?}");
    assert!(done.sync.last_run_complete, "{done:?}");
    assert!(done.sync.last_error.is_none(), "the error clears: {done:?}");
    assert_eq!(record(&mut conn, "payments-api"), 5 + 150);
    assert_eq!(support_tasks(&mut conn, "payments-api"), 5 + 150);

    // ---- 4: a rate limit waits; a refused token names the permission ------------
    rows.push(issue(
        9001,
        900,
        "After the rate limit",
        "2026-10-04T09:00:00Z",
    ));
    forge.set_github(rows.clone());
    forge.rate_limit_at(1, 1);
    let calls_before = forge.calls().len();
    look(&jobs, &scratch_url, at(t0, 46 * 60)).await;
    let waited = status(&svc, "payments-api").await;
    assert_eq!(waited.state, "done", "{waited:?}");
    assert_eq!(waited.result.imported, 1, "{waited:?}");
    let asked: Vec<String> = forge
        .calls()
        .into_iter()
        .skip(calls_before)
        .map(|c| c.path_and_query)
        .collect();
    assert_eq!(
        asked.len(),
        3,
        "page 1, page 1 again after the wait, page 2: {asked:?}"
    );
    assert!(
        asked[0].ends_with("&page=1") && asked[1].ends_with("&page=1"),
        "{asked:?}"
    );
    forge.rate_limit_at(0, 0);
    forge.refuse(true);
    look(&jobs, &scratch_url, at(t0, 48 * 60)).await;
    let refused = status(&svc, "payments-api").await;
    assert_eq!(refused.state, "failed", "{refused:?}");
    assert!(
        refused
            .error
            .clone()
            .unwrap_or_default()
            .contains("Issues: read")
    );
    assert!(
        refused
            .sync
            .last_error
            .clone()
            .unwrap_or_default()
            .contains("Issues: read")
    );
    forge.refuse(false);
    look(&jobs, &scratch_url, at(t0, 50 * 60)).await;
    let again = status(&svc, "payments-api").await;
    assert_eq!(again.state, "done", "the next run tries again: {again:?}");
    assert!(again.sync.last_error.is_none());
    assert!(again.sync.last_run_complete);
    // No read token: the run fails and names it
    svc.remove_repository_credential("atlas")
        .await
        .expect("remove token");
    let (status_code, _) = set_interval(&svc, "atlas", "5m").await;
    assert_eq!(status_code, 200);
    look(&jobs, &scratch_url, at(t0, 51 * 60)).await;
    let no_token = status(&svc, "atlas").await;
    assert_eq!(no_token.state, "failed", "{no_token:?}");
    assert!(
        no_token
            .sync
            .last_error
            .clone()
            .unwrap_or_default()
            .contains("read token"),
        "{no_token:?}"
    );
    svc.set_repository_credential("atlas", &Secret::new("github_pat_fixture0001"))
        .await
        .expect("read token");
    let (status_code, _) = set_interval(&svc, "atlas", "off").await;
    assert_eq!(status_code, 200);

    // ---- 2: a webhook and the cadence make one task for one issue ----------------
    forge.set_github(vec![
        issue(
            5001,
            1,
            "Taken by the webhook first",
            "2026-10-05T09:00:00Z",
        ),
        issue(
            5002,
            2,
            "Taken by the cadence first",
            "2026-10-05T10:00:00Z",
        ),
    ]);
    let hook = svc2
        .create_forge_connection(&CreateForgeConnectionRequest {
            repository: "ledger".into(),
        })
        .await
        .expect("connection");
    let deliver = |row: Value| {
        let body = json!({
            "action": "opened",
            "issue": row,
            "repository": { "full_name": "acme/ledger" },
            "sender": { "login": "robin" }
        })
        .to_string();
        let http = http.clone();
        let url = format!(
            "{}/webhooks/github/acme/{}",
            server2.base_url, hook.connection.id
        );
        let secret = hook.webhook_secret.clone();
        async move {
            http.post(url)
                .header("x-github-event", "issues")
                .header(
                    "x-hub-signature-256",
                    github_signature(&secret, body.as_bytes()),
                )
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .expect("delivery")
        }
    };
    let first_row = issue(
        5001,
        1,
        "Taken by the webhook first",
        "2026-10-05T09:00:00Z",
    );
    let second_row = issue(
        5002,
        2,
        "Taken by the cadence first",
        "2026-10-05T10:00:00Z",
    );
    assert_eq!(deliver(first_row.clone()).await.status(), 200);
    assert_eq!(record(&mut conn, "ledger"), 1);
    let (status_code, _) = set_interval(&svc, "ledger", "1m").await;
    assert_eq!(status_code, 200);
    let t1 = Utc::now();
    look(&jobs, &scratch_url, t1).await;
    let ledger = status(&svc, "ledger").await;
    assert_eq!(ledger.result.imported, 1, "{ledger:?}");
    assert_eq!(
        ledger.result.skipped, 1,
        "the webhook took the first: {ledger:?}"
    );
    // The webhook sends the issue that the cadence took
    let response = deliver(second_row.clone()).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap_or_default();
    assert!(text.contains("taken already"), "{text}");
    assert_eq!(record(&mut conn, "ledger"), 2);
    assert_eq!(
        support_tasks(&mut conn, "ledger"),
        2,
        "one task for one issue"
    );
    let (status_code, _) = set_interval(&svc, "ledger", "off").await;
    assert_eq!(status_code, 200);
    jobs2.drain().await;

    // ---- 5: two servers on one database run one import at a time ------------------
    let (status_code, _) = set_interval(&svc, "payments-api", "off").await;
    assert_eq!(status_code, 200);
    forge.set_github(vec![issue(4001, 1, "One import", "2026-10-06T09:00:00Z")]);
    forge.delay(400);
    let (status_code, _) = set_interval(&svc, "atlas", "1m").await;
    assert_eq!(status_code, 200);
    let t2 = Utc::now() + chrono::Duration::hours(2);
    let calls_before = forge.calls().len();
    let run_jobs = |jobs: &Arc<Worker>| {
        let jobs = Arc::clone(jobs);
        tokio::spawn(async move { jobs.run().await })
    };
    let (w1, w2) = (run_jobs(&jobs), run_jobs(&jobs2));
    let (u1, u2) = (scratch_url.clone(), scratch_url.clone());
    let look_now = |url: String, when: DateTime<Utc>| {
        tokio::task::spawn_blocking(move || {
            let mut conn = PgConnection::establish(&url).expect("connection");
            conn.batch_execute("SET search_path TO org_acme, public")
                .unwrap();
            queue_due(&mut conn, "acme", when, 3).expect("the look")
        })
    };
    // Both servers look at the same time: one gets the run
    let (a, b) = tokio::join!(look_now(u1, t2), look_now(u2, t2));
    let (a, b) = (a.unwrap(), b.unwrap());
    let queued = a.queued.len() + b.queued.len();
    assert_eq!(queued, 1, "one job for one due run: {a:?} {b:?}");
    // While it runs, the next look finds the import busy: no second job
    let mut busy_seen = false;
    for _ in 0..40 {
        let later = look_now(scratch_url.clone(), t2 + chrono::Duration::minutes(5))
            .await
            .unwrap();
        assert!(later.queued.is_empty(), "no second import: {later:?}");
        busy_seen |= later.busy.contains(&"atlas".to_string());
        if status(&svc, "atlas").await.state == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(busy_seen, "a look found the import still running");
    let atlas_calls = forge
        .calls()
        .into_iter()
        .skip(calls_before)
        .filter(|c| c.path_and_query.starts_with("/repos/acme/atlas/"))
        .count();
    assert_eq!(atlas_calls, 1, "one list of one page: one import ran");
    assert_eq!(record(&mut conn, "atlas"), 1);
    w1.abort();
    w2.abort();
    forge.delay(0);

    // ---- the pass of the periodic mechanism queues the job ------------------------
    forge.set_github(vec![issue(3001, 9, "By the pass", "2026-10-07T09:00:00Z")]);
    let before = status(&svc, "atlas").await.sync.last_run_at;
    // A new interval makes the next run due now.
    let (status_code, _) = set_interval(&svc, "atlas", "2m").await;
    assert_eq!(status_code, 200);
    jobs.add_periodic(
        kairos_server::periodic::ISSUE_SYNC,
        kairos_server::forge::sync::TICK,
        kairos_server::forge::sync::pass(scratch_url.clone(), 3),
    )
    .await
    .expect("the pass");
    jobs.drain().await;
    let by_pass = status(&svc, "atlas").await;
    assert_eq!(by_pass.state, "done", "{by_pass:?}");
    assert_ne!(by_pass.sync.last_run_at, before, "the pass ran the import");
    assert_eq!(by_pass.result.imported, 1, "{by_pass:?}");
    assert_eq!(record(&mut conn, "atlas"), 2);

    Arc::clone(&jobs).shutdown(Duration::from_secs(5)).await;
    Arc::clone(&jobs2).shutdown(Duration::from_secs(5)).await;
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}

/// The short code of the first task that the intake made.
fn run_task_code(conn: &mut PgConnection) -> String {
    #[derive(QueryableByName)]
    struct Code {
        #[diesel(sql_type = diesel::sql_types::Text)]
        short_code: String,
    }
    sql_query(
        "SELECT t.short_code FROM org_acme.tasks t \
           JOIN org_acme.forge_issue_intake i ON i.item_id = t.id \
           JOIN org_acme.repositories r ON r.id = i.repository_id \
          WHERE r.slug = 'payments-api' ORDER BY i.created_at LIMIT 1",
    )
    .get_result::<Code>(conn)
    .expect("a task")
    .short_code
}
