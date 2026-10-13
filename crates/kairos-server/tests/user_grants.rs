//! KAIROS-T-0413 (KAIROS-A-0025) — the grants of one person on many
//! boards, against the booted production router.
//!
//! - `GET /api/members/{user_id}/grants`: an organization admin sees each
//!   board, by level, with the capabilities that apply at its level, and
//!   the teams of the person. A holder of `administer_members` sees only
//!   the boards where it holds it, and no teams. A caller that administers
//!   no board is refused.
//! - `PATCH /api/members/{user_id}/grants`: one change over many boards is
//!   one transaction. A board that the caller cannot administer refuses the
//!   whole change and is named. A grant that the person has already
//!   refuses the whole change. A grant that does not apply at the level of
//!   the board is refused. Each grant and revoke writes its activity row.
//!
//! Runs against the LIVE compose stack (`angreal services up`). The test
//! owns a scratch database.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, request, user_token,
    with_database,
};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_user_grants_t0413_test";
const TENANT: [(&str, &str); 1] = [("x-tenant", "acme")];

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// The number of activity rows of `action` about the grants of `user_id`.
fn activity(conn: &mut PgConnection, action: &str, user_id: Uuid) -> i64 {
    diesel::sql_query(format!(
        "SELECT count(*) AS n FROM org_acme.activity_log WHERE action = '{action}' \
         AND details LIKE '%user:{user_id}'"
    ))
    .get_result::<Count>(conn)
    .expect("activity")
    .n
}

/// The row of the board `slug` in a view.
fn row<'a>(view: &'a Value, slug: &str) -> &'a Value {
    view["boards"]
        .as_array()
        .expect("boards")
        .iter()
        .find(|b| b["board_slug"] == slug)
        .unwrap_or_else(|| panic!("no row for {slug} in {view}"))
}

/// The slugs of the rows of a view, in order.
fn slugs(view: &Value) -> Vec<String> {
    view["boards"]
        .as_array()
        .expect("boards")
        .iter()
        .map(|b| b["board_slug"].as_str().expect("slug").to_string())
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect()
}

#[tokio::test]
async fn the_grants_of_one_person_on_many_boards_against_live_stack() {
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
    let svc = user_token(&http, "svc").await;
    let alice = user_token(&http, "alice").await;
    let bob = user_token(&http, "bob").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let router = app::router(app::state_with(base_config(&scratch_url), pool, auth));

    // svc is the org admin; alice and bob are members with no grant.
    let mut ids = Vec::new();
    for (token, email, role) in [
        (&svc, "svc@kairos.test", OrgRole::Admin),
        (&alice, "alice@kairos.test", OrgRole::Member),
        (&bob, "bob@kairos.test", OrgRole::Member),
    ] {
        let _ = request(
            &router,
            Method::GET,
            "/api/whoami",
            Some(token),
            &TENANT,
            None,
        )
        .await;
        let id: Uuid = users::table
            .filter(users::email.eq(email))
            .select(users::id)
            .first(&mut conn)
            .expect("user provisioned");
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: id,
                role,
            })
            .execute(&mut conn)
            .expect("granting membership");
        ids.push(id);
    }
    let (alice_id, bob_id) = (ids[1], ids[2]);

    let as_user = |token: &str, method: Method, uri: &str, body: Option<Value>| {
        let (router, token, uri) = (router.clone(), token.to_string(), uri.to_string());
        async move { request(&router, method, &uri, Some(&token), &TENANT, body).await }
    };

    // Two teams, so two delivery boards.
    let mut team_ids = Vec::new();
    for (name, slug, prefix) in [("Alpha", "alpha", "ALPHA"), ("Beta", "beta", "BETA")] {
        let (status, body) = as_user(
            &svc,
            Method::POST,
            "/api/teams",
            Some(json!({ "name": name, "slug": slug, "code_prefix": prefix })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        team_ids.push(body["id"].as_str().expect("team id").to_string());
    }
    let mut board = std::collections::HashMap::new();
    for slug in [
        "strategy",
        "initiatives",
        "adrs",
        "alpha-delivery",
        "beta-delivery",
    ] {
        let (status, body) = as_user(&svc, Method::GET, &format!("/api/boards/{slug}"), None).await;
        assert_eq!(status, StatusCode::OK, "{slug}: {body}");
        board.insert(slug, body["id"].as_str().expect("board id").to_string());
    }
    let grants_of = |id: Uuid| format!("/api/members/{id}/grants");

    // --- An org admin sees each board, by level, and the teams.
    let (status, view) = as_user(&svc, Method::GET, &grants_of(bob_id), None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["email"], "bob@kairos.test");
    assert_eq!(
        slugs(&view),
        [
            "strategy",
            "initiatives",
            "alpha-delivery",
            "beta-delivery",
            "adrs"
        ]
    );
    assert_eq!(
        strings(&row(&view, "alpha-delivery")["applicable"]),
        [
            "manage_tasks",
            "manage_documents",
            "transition_items",
            "configure_boards",
            "administer_members"
        ]
    );
    assert!(strings(&row(&view, "strategy")["applicable"]).contains(&"manage_strategies".into()));
    assert!(!strings(&row(&view, "strategy")["applicable"]).contains(&"manage_tasks".into()));
    assert!(strings(&row(&view, "adrs")["applicable"]).contains(&"manage_adrs".into()));
    assert_eq!(view["teams"], json!([]));

    // --- A caller that administers no board is refused.
    let (status, body) = as_user(&bob, Method::GET, &grants_of(alice_id), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.to_string().contains("administer_members"), "{body}");

    // --- An unknown user is a 404.
    let (status, body) = as_user(&svc, Method::GET, &grants_of(Uuid::new_v4()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // --- One change over three boards, as the org admin.
    let before = activity(&mut conn, "capability_grant", bob_id);
    let (status, view) = as_user(
        &svc,
        Method::PATCH,
        &grants_of(bob_id),
        Some(json!({ "changes": [
            { "board_id": board["alpha-delivery"], "grant": ["manage_tasks", "transition_items"] },
            { "board_id": board["initiatives"], "grant": ["manage_initiatives"] },
            { "board_id": board["adrs"], "grant": ["manage_adrs"] },
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        strings(&row(&view, "alpha-delivery")["capabilities"]),
        ["manage_tasks", "transition_items"]
    );
    assert_eq!(
        strings(&row(&view, "initiatives")["capabilities"]),
        ["manage_initiatives"]
    );
    assert_eq!(
        activity(&mut conn, "capability_grant", bob_id),
        before + 4,
        "one activity row for each grant"
    );

    // --- A board owner: administer_members on alpha-delivery only.
    let (status, body) = as_user(
        &svc,
        Method::POST,
        &format!("/api/boards/{}/members", board["alpha-delivery"]),
        Some(json!({ "user_id": alice_id, "capabilities": ["administer_members"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, view) = as_user(&alice, Method::GET, &grants_of(bob_id), None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(slugs(&view), ["alpha-delivery"]);
    assert_eq!(view["teams"], Value::Null, "{view}");

    // A change that names a board she does not administer is refused as a
    // whole, and the refusal names that board.
    let grants_before = activity(&mut conn, "capability_grant", bob_id);
    let (status, body) = as_user(
        &alice,
        Method::PATCH,
        &grants_of(bob_id),
        Some(json!({ "changes": [
            { "board_id": board["alpha-delivery"], "grant": ["manage_documents"] },
            { "board_id": board["beta-delivery"], "grant": ["manage_tasks"] },
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.to_string().contains("beta-delivery"), "{body}");
    assert_eq!(body["error"]["details"]["board_slug"], "beta-delivery");
    assert_eq!(
        activity(&mut conn, "capability_grant", bob_id),
        grants_before
    );
    let (_, view) = as_user(&svc, Method::GET, &grants_of(bob_id), None).await;
    assert_eq!(
        strings(&row(&view, "alpha-delivery")["capabilities"]),
        ["manage_tasks", "transition_items"],
        "the first board of a refused change is unchanged"
    );
    assert_eq!(
        strings(&row(&view, "beta-delivery")["capabilities"]),
        Vec::<String>::new()
    );

    // On her own board, she grants and revokes in one change.
    let revokes_before = activity(&mut conn, "capability_revoke", bob_id);
    let (status, view) = as_user(
        &alice,
        Method::PATCH,
        &grants_of(bob_id),
        Some(json!({ "changes": [
            { "board_id": board["alpha-delivery"], "grant": ["manage_documents"], "revoke": ["transition_items"] },
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        strings(&row(&view, "alpha-delivery")["capabilities"]),
        ["manage_documents", "manage_tasks"]
    );
    assert_eq!(
        activity(&mut conn, "capability_revoke", bob_id),
        revokes_before + 1
    );
    assert_eq!(
        activity(&mut conn, "capability_grant", bob_id),
        grants_before + 1
    );

    // --- A write that fails on the second board undoes the first board.
    let grants_before = activity(&mut conn, "capability_grant", bob_id);
    let (status, body) = as_user(
        &svc,
        Method::PATCH,
        &grants_of(bob_id),
        Some(json!({ "changes": [
            { "board_id": board["beta-delivery"], "grant": ["configure_boards"] },
            { "board_id": board["initiatives"], "grant": ["manage_initiatives"] },
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("initiatives"), "{body}");
    let (_, view) = as_user(&svc, Method::GET, &grants_of(bob_id), None).await;
    assert_eq!(
        strings(&row(&view, "beta-delivery")["capabilities"]),
        Vec::<String>::new(),
        "the grant on the first board was rolled back"
    );
    assert_eq!(
        activity(&mut conn, "capability_grant", bob_id),
        grants_before
    );

    // --- A capability that does not apply at the level of the board.
    let (status, body) = as_user(
        &svc,
        Method::PATCH,
        &grants_of(bob_id),
        Some(json!({ "changes": [
            { "board_id": board["alpha-delivery"], "grant": ["manage_strategies"] },
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["details"]["board_slug"], "alpha-delivery");
    assert_eq!(body["error"]["details"]["capability"], "manage_strategies");

    // --- The form of the body.
    for (body, needle) in [
        (json!({ "changes": [] }), "changes"),
        (
            json!({ "changes": [
                { "board_id": board["adrs"], "grant": ["manage_documents"] },
                { "board_id": board["adrs"], "revoke": ["manage_adrs"] },
            ]}),
            "two times",
        ),
        (
            json!({ "changes": [{ "board_id": board["adrs"] }] }),
            "no capability",
        ),
        (
            json!({ "changes": [{ "board_id": board["adrs"], "grant": ["manage_documents"], "role": "lead" }] }),
            "role",
        ),
        (json!({ "changes": [], "preset": "lead" }), "preset"),
    ] {
        let (status, refusal) = as_user(&svc, Method::PATCH, &grants_of(bob_id), Some(body)).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
            "{status}: {refusal}"
        );
        assert!(refusal.to_string().contains(needle), "{needle}: {refusal}");
    }

    // --- A board owner can revoke her own last administer_members; the
    // answer then has no board.
    let (status, view) = as_user(
        &alice,
        Method::PATCH,
        &grants_of(alice_id),
        Some(json!({ "changes": [
            { "board_id": board["alpha-delivery"], "revoke": ["administer_members"] },
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["boards"], json!([]));
    let (status, body) = as_user(&alice, Method::GET, &grants_of(bob_id), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // --- The teams of the person.
    let (status, body) = as_user(
        &svc,
        Method::POST,
        &format!("/api/teams/{}/members", team_ids[0]),
        Some(json!({ "user_id": bob_id })),
    )
    .await;
    assert!(status.is_success(), "{status}: {body}");
    let (_, view) = as_user(&svc, Method::GET, &grants_of(bob_id), None).await;
    let teams = view["teams"].as_array().expect("teams");
    assert_eq!(teams.len(), 1, "{view}");
    assert_eq!(teams[0]["slug"], "alpha");

    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
