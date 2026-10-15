//! Integration test for issue intake (KAIROS-T-0482, decision
//! KAIROS-A-0028): an opened GitHub or GitLab issue that the forge filter
//! admits becomes ONE support task in the entry column of the delivery
//! board of the team of the repository, made by the forge actor, linked to
//! the issue.
//!
//! - a redelivered event makes no second task;
//! - closed, edited and reopened events change nothing;
//! - an excluded author makes no task;
//! - the issue link is on the item, and not in the in-flight rollups.
//!
//! The deliveries are recorded fixtures (no network). Runs against the LIVE
//! compose stack. Owns the scratch database `kairos_forge_issues_t0482_test`.

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
use kairos_client::types_forge::CreateForgeConnectionRequest;
use kairos_db::models::{BoardLevel, NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::forge::auth::github_signature;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_forge_issues_t0482_test";

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
        .unwrap_or_else(|e| panic!("user {email} not provisioned: {e}"))
}

/// One task made by intake, as stored.
#[derive(QueryableByName, Debug, Clone)]
struct IntakeTask {
    #[diesel(sql_type = diesel::sql_types::Text)]
    short_code: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    title: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content: String,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    board_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    column_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    task_type: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    work_class: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    repository_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    creator: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    creator_kind: String,
}

fn intake_tasks(conn: &mut PgConnection) -> Vec<IntakeTask> {
    sql_query(
        "SELECT t.short_code, t.title, t.content, t.board_id, t.column_id, t.task_type, \
                t.work_class, t.repository_id, u.display_name AS creator, \
                u.kind AS creator_kind \
           FROM org_acme.tasks t \
           JOIN org_acme.forge_issue_intake i ON i.item_id = t.id \
           JOIN public.users u ON u.id = t.created_by \
          ORDER BY t.short_code",
    )
    .load(conn)
    .expect("intake tasks")
}

#[tokio::test]
async fn issue_intake_against_live_stack() {
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
    let router = app::router(app::state_with(
        base_config(&scratch_url),
        pool.clone(),
        auth.clone(),
    ));
    let server = spawn_server(router).await;
    let svc = server.client(&svc_token, "acme");
    let _ = svc.whoami().await;
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: svc_id,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("granting admin");

    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    let team = common::seed_team(&mut conn, "Payments", "payments");
    let board = kairos_db::create_board(
        &mut conn,
        BoardLevel::Delivery,
        "Payments",
        "payments",
        kairos_db::CodePrefix::FromSlug,
        Some(team),
        Some(svc_id),
    )
    .expect("delivery board");
    let entry = kairos_db::boards::entry_column(&mut conn, board.id)
        .expect("entry column")
        .expect("the board has an entry column");

    // A GitHub and a GitLab repository of the team, each connected.
    let mut hooks = Vec::new();
    for (forge, name, slug) in [
        ("github", "acme/payments-api", "payments-api"),
        ("gitlab", "acme/portal-web", "portal-web"),
    ] {
        svc.create_repository(
            &kairos_client::types_repositories::CreateRepositoryRequest {
                slug: Some(slug.into()),
                forge: forge.into(),
                repo_full_name: name.into(),
                repo_url: format!("https://{forge}.com/{name}"),
                default_branch: None,
                team: team.to_string(),
                description: None,
            },
        )
        .await
        .expect("registering the repo");
        let created = svc
            .create_forge_connection(&CreateForgeConnectionRequest {
                repository: slug.into(),
            })
            .await
            .expect("connecting the repo");
        hooks.push((
            format!(
                "{}/webhooks/{forge}/acme/{}",
                server.base_url, created.connection.id
            ),
            created.webhook_secret,
        ));
    }
    let (github_hook, github_secret) = hooks[0].clone();
    let (gitlab_hook, gitlab_secret) = hooks[1].clone();
    let github = |body: String, event: &'static str| {
        let http = http.clone();
        let url = github_hook.clone();
        let signature = github_signature(&github_secret, body.as_bytes());
        async move {
            let response = http
                .post(url)
                .header("x-github-event", event)
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .expect("delivery");
            assert_eq!(response.status(), 200);
            response.json::<Value>().await.expect("json")
        }
    };
    let gitlab = |body: String, event: &'static str| {
        let http = http.clone();
        let url = gitlab_hook.clone();
        let token = gitlab_secret.clone();
        async move {
            let response = http
                .post(url)
                .header("x-gitlab-event", event)
                .header("x-gitlab-token", token)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .expect("delivery");
            assert_eq!(response.status(), 200);
            response.json::<Value>().await.expect("json")
        }
    };

    // =======================================================================
    // An opened issue makes one support task in the entry column
    // =======================================================================
    let answer = github(fixture("github_issue_opened.json"), "issues").await;
    assert_eq!(answer["status"], "issue taken", "{answer}");
    let tasks = intake_tasks(&mut conn);
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let task = tasks[0].clone();
    assert_eq!(answer["item"], task.short_code.as_str());
    assert_eq!(task.title, "Crash when the config file is empty");
    assert_eq!(task.board_id, board.id, "the delivery board of the team");
    assert_eq!(task.column_id, entry, "the entry column");
    assert_eq!(task.task_type, "support");
    assert_eq!(task.work_class, "support", "the Support lane");
    assert!(task.repository_id.is_some(), "the repository is set");
    assert_eq!(
        (task.creator.as_str(), task.creator_kind.as_str()),
        ("Forge", "system"),
        "the forge actor made it"
    );
    assert!(
        task.content
            .contains("[#12](https://github.com/acme/payments-api/issues/12)"),
        "{}",
        task.content
    );
    assert!(task.content.contains("opened by robin"));
    assert!(task.content.contains("It crashes."));
    // The link to the issue, on the item.
    let links = svc
        .item_links(kairos_client::EntityKind::Task, &task.short_code)
        .await
        .expect("item links");
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0].kind, "issue");
    assert_eq!(links[0].external_id, "12");
    assert_eq!(links[0].author, "robin");
    assert_eq!(
        links[0].url,
        "https://github.com/acme/payments-api/issues/12"
    );
    // Not in the in-flight rollups: an issue is not a branch or a PR.
    assert!(
        svc.team_links(&team.to_string(), None)
            .await
            .expect("rollup")
            .is_empty()
    );
    assert!(
        svc.get_repository("payments-api")
            .await
            .expect("detail")
            .in_flight
            .is_empty()
    );

    // =======================================================================
    // Redelivery makes no second task
    // =======================================================================
    let answer = github(fixture("github_issue_opened.json"), "issues").await;
    assert_eq!(answer["status"], "issue taken already", "{answer}");
    assert_eq!(intake_tasks(&mut conn).len(), 1);

    // =======================================================================
    // Closed, edited and reopened events change nothing
    // =======================================================================
    for name in [
        "github_issue_closed.json",
        "github_issue_edited.json",
        "github_issue_reopened.json",
    ] {
        let answer = github(fixture(name), "issues").await;
        assert_eq!(
            answer["status"], "event type not consumed",
            "{name}: {answer}"
        );
    }
    let after: Vec<IntakeTask> = intake_tasks(&mut conn);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].title, task.title, "no sync after intake");

    // =======================================================================
    // An excluded author makes no task
    // =======================================================================
    let (status, body) = svc
        .raw_request(
            reqwest::Method::PATCH,
            "/api/repositories/payments-api",
            Some(&json!({"forge_filters": {"issue_author": {"exclude": "^dependabot\\[bot\\]$"}}})),
        )
        .await
        .expect("PATCH");
    assert_eq!(status, 200, "{body}");
    let mut bot: Value =
        serde_json::from_str(&fixture("github_issue_opened.json")).expect("fixture json");
    bot["issue"]["id"] = json!(2401557099_i64);
    bot["issue"]["number"] = json!(13);
    bot["issue"]["user"]["login"] = json!("dependabot[bot]");
    let answer = github(bot.to_string(), "issues").await;
    assert_eq!(answer["status"], "filtered", "{answer}");
    assert_eq!(intake_tasks(&mut conn).len(), 1);
    // Another author with a new issue is taken.
    bot["issue"]["user"]["login"] = json!("sam");
    let answer = github(bot.to_string(), "issues").await;
    assert_eq!(answer["status"], "issue taken", "{answer}");
    assert_eq!(intake_tasks(&mut conn).len(), 2);

    // =======================================================================
    // GitLab: Issue Hook, action open; close and confidential change nothing
    // =======================================================================
    let answer = gitlab(fixture("gitlab_issue_open.json"), "Issue Hook").await;
    assert_eq!(answer["status"], "issue taken", "{answer}");
    let answer = gitlab(fixture("gitlab_issue_open.json"), "Issue Hook").await;
    assert_eq!(answer["status"], "issue taken already", "{answer}");
    for name in ["gitlab_issue_close.json", "gitlab_issue_confidential.json"] {
        let answer = gitlab(fixture(name), "Issue Hook").await;
        assert_eq!(
            answer["status"], "event type not consumed",
            "{name}: {answer}"
        );
    }
    let tasks = intake_tasks(&mut conn);
    assert_eq!(tasks.len(), 3, "{tasks:?}");
    let portal = tasks
        .iter()
        .find(|t| t.title == "Login page is blank")
        .expect("the GitLab issue");
    assert!(
        portal.content.contains("on gitlab, opened by sam"),
        "{}",
        portal.content
    );
    assert_eq!(portal.board_id, board.id);

    // One activity row of the forge actor per issue taken.
    let taken: i64 = sql_query(
        "SELECT count(*) AS count FROM org_acme.activity_log \
          WHERE action = 'forge_event' AND details LIKE 'issue %'",
    )
    .get_result::<Count>(&mut conn)
    .expect("count")
    .count;
    assert_eq!(taken, 3);

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}
