//! Integration test for the forge filters of a repository (KAIROS-T-0479,
//! COLLIERY-I-0623): `forge_filters` on `GET` and `PATCH
//! /api/repositories/{slug}`.
//!
//! - every field reads with the default (include `.*`, exclude empty);
//! - only an org admin sets them; a member of the owner team is refused;
//! - an invalid or oversize pattern is refused with 422 and the pattern
//!   named in `details.field`, and nothing is written;
//! - a PATCH sends only what changes; the other patterns stay.
//!
//! What the filter does to webhook deliveries is in `forge_webhook.rs`.
//!
//! Runs against the LIVE compose stack. Owns the scratch database
//! `kairos_forge_filters_t0479_test`. Cast: `svc` org admin; `bob` member
//! of `platform`, the owner team.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_client::KairosClient;
use kairos_client::types_org::{AddTeamMemberRequest, CreateTeamRequest};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_forge_filters_t0479_test";

fn user_id(conn: &mut PgConnection, email: &str) -> Uuid {
    users::table
        .filter(users::email.eq(email))
        .select(users::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("user {email} not provisioned: {e}"))
}

async fn patch(client: &KairosClient, slug: &str, body: &Value) -> (u16, Value) {
    client
        .raw_request(
            Method::PATCH,
            &format!("/api/repositories/{slug}"),
            Some(body),
        )
        .await
        .expect("PATCH /api/repositories/{slug}")
}

#[tokio::test]
async fn forge_filters_against_live_stack() {
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
    let bob_token = user_token(&http, "bob").await;
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
    let bob = server.client(&bob_token, "acme");
    for client in [&svc, &bob] {
        let _ = client.whoami().await;
    }
    let svc_id = user_id(&mut conn, "svc@kairos.test");
    let bob_id = user_id(&mut conn, "bob@kairos.test");
    for (user_id, role) in [(svc_id, OrgRole::Admin), (bob_id, OrgRole::Member)] {
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
    }
    let platform = svc
        .create_team(&CreateTeamRequest {
            name: "Platform".into(),
            slug: "platform".into(),
            code_prefix: "PLATFORM".into(),
            team_type: None,
        })
        .await
        .expect("platform");
    svc.add_team_member(
        &platform.id,
        &AddTeamMemberRequest {
            user_id: bob_id.to_string(),
        },
    )
    .await
    .expect("bob → platform");
    let repo = svc
        .create_repository(
            &kairos_client::types_repositories::CreateRepositoryRequest {
                slug: Some("app".into()),
                forge: "github".into(),
                repo_full_name: "acme/app".into(),
                repo_url: "https://github.com/acme/app".into(),
                default_branch: None,
                team: platform.id.clone(),
                description: None,
            },
        )
        .await
        .expect("registering the repo");

    // ---- defaults: every field, include .*, exclude empty ----------------
    let filters = serde_json::to_value(&repo.forge_filters).expect("json");
    for field in kairos_client::types_repositories::ForgeFilters::FIELDS {
        assert_eq!(
            filters[field],
            json!({"include": ".*", "exclude": ""}),
            "{field}: {filters}"
        );
    }
    assert_eq!(repo.forge_filters.summary(), "default (admit everything)");

    // ---- a member of the owner team is not an org admin -------------------
    let body = json!({"forge_filters": {"pr_author": {"exclude": "^dependabot\\[bot\\]$"}}});
    let (status, refused) = patch(&bob, "app", &body).await;
    assert_eq!(status, 403, "{refused}");
    // bob can still change the description: only the filter is admin-only.
    let (status, _) = patch(&bob, "app", &json!({"description": "x"})).await;
    assert_eq!(status, 200);

    // ---- invalid and oversize patterns are refused and named --------------
    for (body, field) in [
        (
            json!({"forge_filters": {"issue_author": {"exclude": "dependabot[bot"}}}),
            "forge_filters.issue_author.exclude",
        ),
        (
            json!({"forge_filters": {"pr_title": {"include": "a".repeat(513)}}}),
            "forge_filters.pr_title.include",
        ),
        (
            // The good pattern of the same call is not written either.
            json!({"forge_filters": {
                "branch_name": {"include": "^feature/"},
                "pr_head_branch": {"include": "("}
            }}),
            "forge_filters.pr_head_branch.include",
        ),
    ] {
        let (status, refused) = patch(&svc, "app", &body).await;
        assert_eq!(status, 422, "{refused}");
        assert_eq!(refused["error"]["code"], "VALIDATION", "{refused}");
        assert_eq!(refused["error"]["details"]["field"], field, "{refused}");
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(field.trim_start_matches("forge_filters.")),
            "{message}"
        );
    }
    // An unknown field of the filter is refused, and named.
    let (status, refused) = patch(
        &svc,
        "app",
        &json!({"forge_filters": {"issue_body": {"exclude": "x"}}}),
    )
    .await;
    assert_eq!(status, 422, "{refused}");
    assert!(refused.to_string().contains("issue_body"), "{refused}");
    let after = svc.get_repository("app").await.expect("read");
    assert_eq!(
        after.repository.forge_filters.summary(),
        "default (admit everything)",
        "a refused call writes nothing"
    );

    // ---- an org admin sets them; what is not sent stays -------------------
    let (status, updated) = patch(&svc, "app", &body).await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(
        updated["forge_filters"]["pr_author"],
        json!({"include": ".*", "exclude": "^dependabot\\[bot\\]$"})
    );
    let (status, updated) = patch(
        &svc,
        "app",
        &json!({"forge_filters": {
            "pr_author": {"include": "^(robin|sam|dependabot)"},
            "issue_author": {"exclude": "dependabot"}
        }}),
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(
        updated["forge_filters"]["pr_author"],
        json!({"include": "^(robin|sam|dependabot)", "exclude": "^dependabot\\[bot\\]$"}),
        "the exclude that was not sent stays"
    );
    assert_eq!(
        updated["forge_filters"]["issue_author"],
        json!({"include": ".*", "exclude": "dependabot"})
    );
    // Back to the default removes the row.
    let (status, updated) = patch(
        &svc,
        "app",
        &json!({"forge_filters": {"issue_author": {"exclude": ""}}}),
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    let stored: i64 =
        diesel::sql_query("SELECT count(*) AS count FROM org_acme.repository_forge_filters")
            .get_result::<Count>(&mut conn)
            .expect("count")
            .count;
    assert_eq!(stored, 1, "only pr_author differs from the default");
    // The activity log names the change.
    let activity: i64 = diesel::sql_query(
        "SELECT count(*) AS count FROM org_acme.activity_log \
          WHERE details LIKE 'repository_forge_filters:app:%'",
    )
    .get_result::<Count>(&mut conn)
    .expect("count")
    .count;
    assert_eq!(activity, 3);

    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}
