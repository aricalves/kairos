//! Integration test for the import of the open issues of a repository
//! (KAIROS-T-0483, COLLIERY-I-0623).
//!
//! - an import twice creates nothing the second time;
//! - filtered issues are counted, not created; an issue that a webhook
//!   took is skipped; GitHub pull requests and GitLab confidential issues
//!   are not issues to import;
//! - the read token of the repository goes to the forge, page by page;
//! - no token, no connection, or a refused token are refused with a
//!   message that names what is missing; the rate limit stops the import
//!   and says so.
//!
//! The forge is a local fake API with recorded fixtures (no network). Runs
//! against the LIVE compose stack. Owns the scratch database
//! `kairos_forge_issue_import_t0483_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use serde_json::{Value, json};
use uuid::Uuid;

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
use kairos_server::middleware::auth::Authenticator;
use kairos_server::secrets::SecretsKey;

const SCRATCH_DB: &str = "kairos_forge_issue_import_t0483_test";

fn rows(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/forge_issues/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"));
    serde_json::from_str(&text).expect("fixture json")
}

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

fn intake_count(conn: &mut PgConnection, source: &str) -> i64 {
    sql_query("SELECT count(*) AS count FROM org_acme.forge_issue_intake WHERE source = $1")
        .bind::<diesel::sql_types::Text, _>(source)
        .get_result::<Count>(conn)
        .expect("count")
        .count
}

fn error_of<T: std::fmt::Debug>(result: Result<T, kairos_client::Error>) -> String {
    match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn issue_import_against_live_stack() {
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
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let mut config = base_config(&scratch_url);
    config.secrets_key = Some(SecretsKey::from_bytes([5u8; 32]));
    config.writeback.api_url = forge.url.clone();
    let server = spawn_server(app::router(app::state_with(config, pool, auth))).await;
    let svc = server.client(&svc_token, "acme");
    let bob = server.client(&bob_token, "acme");
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
    // The GitLab API is on the host of the URL of the repository: the fake.
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
        svc.create_repository(&CreateRepositoryRequest {
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
    }

    // ---- refusals before the forge is asked ------------------------------
    let err = error_of(svc.import_issues("payments-api").await);
    assert!(err.contains("no webhook connection"), "{err}");
    let mut hooks = Vec::new();
    for slug in ["payments-api", "portal-web"] {
        hooks.push(
            svc.create_forge_connection(&CreateForgeConnectionRequest {
                repository: slug.into(),
            })
            .await
            .expect("connection"),
        );
    }
    let err = error_of(svc.import_issues("payments-api").await);
    assert!(
        err.contains("no read token") && err.contains("Issues: read"),
        "{err}"
    );
    let err = error_of(svc.import_issues("portal-web").await);
    assert!(err.contains("read_api"), "{err}");
    assert!(forge.calls().is_empty(), "nothing was asked of the forge");
    for slug in ["payments-api", "portal-web"] {
        svc.set_repository_credential(slug, &Secret::new("github_pat_fixture0001"))
            .await
            .expect("read token");
    }
    let err = error_of(bob.import_issues("payments-api").await);
    assert!(err.contains("manage_teams") || err.contains("403"), "{err}");

    // ---- an issue that a webhook took first --------------------------------
    let webhook_issue = json!({
        "action": "opened",
        "issue": rows("github_issues.json")[0].clone(),
        "repository": { "full_name": "acme/payments-api" },
        "sender": { "login": "robin" }
    })
    .to_string();
    let response = http
        .post(format!(
            "{}/webhooks/github/acme/{}",
            server.base_url, hooks[0].connection.id
        ))
        .header("x-github-event", "issues")
        .header(
            "x-hub-signature-256",
            github_signature(&hooks[0].webhook_secret, webhook_issue.as_bytes()),
        )
        .header("content-type", "application/json")
        .body(webhook_issue)
        .send()
        .await
        .expect("delivery");
    assert_eq!(response.status(), 200);
    assert_eq!(intake_count(&mut conn, "webhook"), 1);

    // ---- GitHub: two pages; PRs skipped, filtered counted ------------------
    let (status, body) = svc
        .raw_request(
            reqwest::Method::PATCH,
            "/api/repositories/payments-api",
            Some(&json!({"forge_filters": {"issue_author": {"exclude": "^dependabot\\[bot\\]$"}}})),
        )
        .await
        .expect("PATCH");
    assert_eq!(status, 200, "{body}");
    let mut github_rows = rows("github_issues.json");
    let template = github_rows[0].clone();
    for n in 0..100 {
        let mut row = template.clone();
        row["id"] = json!(3_000_000 + n);
        row["number"] = json!(100 + n);
        row["title"] = json!(format!("Old issue {n}"));
        row["html_url"] = json!(format!(
            "https://github.com/acme/payments-api/issues/{}",
            100 + n
        ));
        github_rows.push(row);
    }
    forge.set_github(github_rows);
    let first = svc.import_issues("payments-api").await.expect("import");
    assert_eq!(first.imported, 100, "{first:?}");
    assert_eq!(first.skipped, 1, "the issue that the webhook took");
    assert_eq!(first.filtered, 1, "the dependabot issue is counted");
    assert_eq!(first.not_taken, 0);
    assert!(first.complete, "{first:?}");
    assert_eq!(first.items.len(), 100);
    assert!(first.message.contains("imported 100"), "{}", first.message);
    assert_eq!(intake_count(&mut conn, "import"), 100);
    let calls = forge.calls();
    assert_eq!(calls.len(), 2, "103 rows are 2 pages of 100: {calls:?}");
    assert_eq!(
        calls[0].path_and_query,
        "/repos/acme/payments-api/issues?state=open&per_page=100&page=1"
    );
    assert!(calls[1].path_and_query.ends_with("&page=2"));
    assert!(
        calls
            .iter()
            .all(|c| c.authorization == "Bearer github_pat_fixture0001"),
        "the read token of the repository"
    );
    // A task from the import is the same as one from a webhook.
    let task = svc.get_task(&first.items[0]).await.expect("task");
    assert_eq!(task.task_type, "support");
    assert_eq!(task.work_class, "support");

    // Twice: nothing the second time.
    let second = svc.import_issues("payments-api").await.expect("import");
    assert_eq!(second.imported, 0, "{second:?}");
    assert_eq!(second.skipped, 101);
    assert_eq!(second.filtered, 1);
    assert_eq!(intake_count(&mut conn, "import"), 100);

    // ---- GitLab: the confidential issue stays on the forge ------------------
    forge.set_gitlab(rows("gitlab_issues.json"));
    let gitlab = svc.import_issues("portal-web").await.expect("import");
    assert_eq!(
        (gitlab.imported, gitlab.skipped, gitlab.filtered),
        (1, 0, 0),
        "{gitlab:?}"
    );
    let again = svc.import_issues("portal-web").await.expect("import");
    assert_eq!((again.imported, again.skipped), (0, 1), "{again:?}");
    let gitlab_calls: Vec<_> = forge
        .calls()
        .into_iter()
        .filter(|c| c.path_and_query.starts_with("/api/v4/"))
        .collect();
    assert_eq!(
        gitlab_calls[0].path_and_query,
        "/api/v4/projects/acme%2Fportal-web/issues?state=opened&per_page=100&page=1"
    );
    assert_eq!(gitlab_calls[0].private_token, "github_pat_fixture0001");

    // ---- the rate limit stops the import, and says so -----------------------
    forge.rate_limit_at(1);
    let limited = svc.import_issues("payments-api").await.expect("import");
    assert!(!limited.complete, "{limited:?}");
    assert!(
        limited.message.contains("rate limit"),
        "{}",
        limited.message
    );
    forge.rate_limit_at(0);

    // ---- a refused token is named -------------------------------------------
    forge.refuse(true);
    let err = error_of(svc.import_issues("payments-api").await);
    assert!(err.contains("Issues: read"), "{err}");
    let err = error_of(svc.import_issues("portal-web").await);
    assert!(err.contains("read_api"), "{err}");

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
