//! KAIROS-T-0368 over REST: `POST /api/search` takes `board` and `team` by
//! slug, `updated_before`/`updated_after` and `metadata_missing`, and
//! `GET /api/boards/{id}/items?with_metadata=true` gives the metadata of
//! each card. The MCP side is `triage_tools_against_live_stack` in
//! `tests/mcp.rs`.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_search_triage_t0368_test`.

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

const SCRATCH_DB: &str = "kairos_search_triage_t0368_test";
const TENANT: [(&str, &str); 1] = [("x-tenant", "acme")];

fn codes(body: &Value) -> Vec<String> {
    body["results"]["tasks"]
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .map(|t| t["short_code"].as_str().expect("code").to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn search_and_board_items_answer_a_triage_pass_against_live_stack() {
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
    let token = user_token(&http, "svc").await;
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(
        Authenticator::discover(ISSUER, AUDIENCE)
            .await
            .expect("OIDC discovery against live Dex"),
    );
    let router = app::router(app::state_with(
        base_config(&scratch_url),
        pool.clone(),
        auth,
    ));
    let send = |method: Method, uri: &str, body: Option<Value>| {
        let router = router.clone();
        let token = token.clone();
        let uri = uri.to_string();
        async move { request(&router, method, &uri, Some(&token), &TENANT, body).await }
    };
    let _ = send(Method::GET, "/api/whoami", None).await;
    let svc: Uuid = users::table
        .filter(users::email.eq("svc@kairos.test"))
        .select(users::id)
        .first(&mut conn)
        .expect("svc provisioned");
    diesel::insert_into(organization_members::table)
        .values(NewOrganizationMember {
            organization_id: org_id,
            user_id: svc,
            role: OrgRole::Admin,
        })
        .execute(&mut conn)
        .expect("granting membership");
    diesel::sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");

    let (status, team) = send(
        Method::POST,
        "/api/teams",
        Some(json!({"name": "Payments", "slug": "payments", "code_prefix": "PAY"})),
    )
    .await;
    assert!(status.is_success(), "{status} {team}");
    let mut made = std::collections::HashMap::new();
    for (name, task_type) in [
        ("old_bug", "bug"),
        ("ranked_bug", "bug"),
        ("new_bug", "bug"),
    ] {
        let (status, task) = send(
            Method::POST,
            "/api/tasks",
            Some(json!({"board_id": "payments-delivery", "title": name, "task_type": task_type})),
        )
        .await;
        assert!(status.is_success(), "{status} {task}");
        made.insert(name, task["short_code"].as_str().expect("code").to_string());
    }
    let (status, answer) = send(
        Method::PATCH,
        &format!("/api/tasks/{}/metadata", made["ranked_bug"]),
        Some(json!({"values": {"priority": "high"}})),
    )
    .await;
    assert!(status.is_success(), "{status} {answer}");
    for name in ["old_bug", "ranked_bug"] {
        diesel::sql_query(
            "UPDATE tasks SET updated_at = now() - interval '120 days' WHERE short_code = $1",
        )
        .bind::<diesel::sql_types::Text, _>(&made[name])
        .execute(&mut conn)
        .expect("backdating");
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();

    // The triage question, with the board and the team by slug, and the
    // column by name within the board (KAIROS-T-0552).
    for scope in [
        json!({"board": "payments-delivery"}),
        json!({"team": "payments"}),
        json!({"board": "payments-delivery", "column": "Backlog"}),
    ] {
        let mut filter = json!({
            "task_type": ["bug"],
            "metadata_missing": ["priority"],
            "updated_before": cutoff,
        });
        filter
            .as_object_mut()
            .expect("object")
            .extend(scope.as_object().expect("object").clone());
        let (status, body) =
            send(Method::POST, "/api/search", Some(json!({"filter": filter}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            codes(&body),
            vec![made["old_bug"].clone()],
            "{scope}: {body}"
        );
    }
    let (status, body) = send(
        Method::POST,
        "/api/search",
        Some(json!({"filter": {"task_type": ["bug"], "updated_after": cutoff}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(codes(&body), vec![made["new_bug"].clone()], "{body}");

    // Refusals: 400 VALIDATION, and the field is named.
    for (filter, field) in [
        (
            json!({"metadata_missing": ["nope"]}),
            "filter.metadata_missing",
        ),
        (json!({"metadata_missing": []}), "filter.metadata_missing"),
        (json!({"board": "nope"}), "filter.board"),
        (json!({"team": "nope"}), "filter.team"),
        // KAIROS-T-0552: a column by name needs the board; an unknown name
        // is refused with the columns of the board; not with column_id.
        (json!({"column": "Backlog"}), "filter.column"),
        (
            json!({"board": "payments-delivery", "column": "Nope"}),
            "filter.column",
        ),
        (
            json!({"board": "payments-delivery", "column": "Backlog", "column_id": Uuid::nil().to_string()}),
            "filter.column",
        ),
        (
            json!({"board": "payments-delivery", "board_id": Uuid::nil().to_string()}),
            "filter.board",
        ),
        (
            json!({"updated_after": cutoff, "updated_before": cutoff}),
            "filter.updated_after",
        ),
    ] {
        let (status, body) =
            send(Method::POST, "/api/search", Some(json!({"filter": filter}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{filter}: {body}");
        assert_eq!(body["error"]["code"], "VALIDATION", "{body}");
        assert_eq!(body["error"]["details"]["field"], field, "{body}");
    }

    // board items: the metadata only on request.
    let (_, page) = send(Method::GET, "/api/boards/payments-delivery/items", None).await;
    assert!(page.get("item_metadata").is_none(), "{page}");
    let (status, page) = send(
        Method::GET,
        "/api/boards/payments-delivery/items?with_metadata=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["item_metadata"],
        json!({ made["ranked_bug"].clone(): {"priority": "high"} }),
        "{page}"
    );

    drop(pool);
    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
