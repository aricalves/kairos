//! KAIROS-T-0649: the time in the column, the notes count and the done
//! limit of the board route `GET /api/boards/{id}/items`.
//!
//! 1. Each card has `entered_column_at`: the time of its creation, then
//!    the time of its last move. Each card has `notes_count`.
//! 2. `?done_limit=N` keeps the last N cards of each done column, by the
//!    time of entry. The other columns keep each card. `item_count` stays
//!    the count of the full column, and `total` counts the cards after the
//!    limit. `?code=` works with it.
//! 3. A `done_limit` under 1 is a 422 `VALIDATION` that names the field.
//!
//! Runs against the LIVE compose stack (`angreal services up`). Owns the
//! scratch database `kairos_board_card_marks_t0649_test`.

mod common;

use std::sync::Arc;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use common::{
    AUDIENCE, ISSUER, base_config, drop_scratch_db, recreate_scratch_db, spawn_server, user_token,
    with_database,
};
use kairos_client::types::CreateTaskRequest;
use kairos_client::types_org::{BoardItemsQuery, BoardItemsResponse, CreateTeamRequest};
use kairos_client::{EntityKind, Error};
use kairos_db::models::{NewOrganizationMember, OrgRole};
use kairos_db::schema::{organization_members, organizations, users};
use kairos_db::{TenantPool, provision_tenant, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;

const SCRATCH_DB: &str = "kairos_board_card_marks_t0649_test";

/// Move the history of the task `id` back by `days`: its creation and
/// each of its activity rows.
fn back_date(conn: &mut PgConnection, id: &str, days: i32) {
    for table in ["activity_log", "tasks"] {
        let (column, key) = if table == "tasks" {
            ("created_at", "id")
        } else {
            ("occurred_at", "entity_id")
        };
        diesel::sql_query(format!(
            "UPDATE org_acme.{table} SET {column} = {column} - make_interval(days => $1) \
             WHERE {key} = $2"
        ))
        .bind::<diesel::sql_types::Integer, _>(days)
        .bind::<diesel::sql_types::Uuid, _>(id.parse::<Uuid>().expect("uuid"))
        .execute(conn)
        .expect("back-dating");
    }
}

/// The task cards of the column `column_id`, by short code.
fn codes_in(items: &BoardItemsResponse, column_id: &str) -> Vec<String> {
    items
        .columns
        .iter()
        .find(|group| group.column.id == column_id)
        .map(|group| group.tasks.iter().map(|t| t.short_code.clone()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn cards_have_the_time_in_the_column_and_the_notes_and_done_columns_have_a_limit() {
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
    let state = app::state_with(base_config(&scratch_url), pool.clone(), auth);
    let server = spawn_server(app::router(state)).await;
    let admin = server.client(&svc_token, "acme");
    let _ = admin.whoami().await;
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
            .unwrap_or_else(|| panic!("a column {name}"))
            .id
            .clone()
    };
    let (todo, active, completed) = (column("Todo"), column("Active"), column("Completed"));
    let new_task = |title: String| CreateTaskRequest {
        board_id: Some(board.clone()),
        column_id: None,
        title,
        content: "one".into(),
        task_type: None,
        work_class: None,
        team_id: None,
        repository: None,
        from_team: None,
        parent: None,
    };

    // --- 1. the time in the column and the notes ----------------------------
    let fresh = admin
        .create_task(&new_task("Fresh".into()))
        .await
        .expect("task");
    admin
        .append_note(EntityKind::Task, &fresh.short_code, "First.")
        .await
        .expect("note");
    admin
        .append_note(EntityKind::Task, &fresh.short_code, "Second.")
        .await
        .expect("note");
    let items = admin
        .board_items(&board, &BoardItemsQuery::default())
        .await
        .expect("board items");
    let card = items
        .columns
        .iter()
        .flat_map(|g| g.tasks.iter())
        .find(|t| t.short_code == fresh.short_code)
        .expect("the card")
        .clone();
    assert_eq!(card.notes_count, Some(2), "{card:?}");
    let entered = card.entered_column_at.clone().expect("entered_column_at");
    let parse = |at: &str| chrono::DateTime::parse_from_rfc3339(at).expect("RFC 3339");
    assert_eq!(
        parse(&entered),
        parse(&card.created_at),
        "a new card entered its column at its creation"
    );
    back_date(&mut conn, &fresh.id, 3);
    admin
        .transition_task(&fresh.short_code, &todo)
        .await
        .expect("to Todo");
    let items = admin
        .board_items(
            &board,
            &BoardItemsQuery {
                code: Some(fresh.short_code.clone()),
                ..BoardItemsQuery::default()
            },
        )
        .await
        .expect("one card");
    let card = items.columns.iter().flat_map(|g| g.tasks.iter()).next();
    let card = card.expect("the card");
    let moved = parse(
        card.entered_column_at
            .as_deref()
            .expect("entered_column_at"),
    );
    let created = parse(&card.created_at);
    assert!(
        moved - created > chrono::Duration::days(2),
        "the move gives the time of entry: {moved} after {created}"
    );
    assert_eq!(card.column_id, todo);

    // --- 2. the done limit ---------------------------------------------------
    // Five done tasks. The first entered Completed 1 day ago, the others
    // 10, 9, 8 and 7 days ago: the order of entry is not that of the codes.
    let mut done_codes = Vec::new();
    for (i, days) in [1, 10, 9, 8, 7].into_iter().enumerate() {
        let task = admin
            .create_task(&new_task(format!("Done {i}")))
            .await
            .expect("task");
        for to in [&todo, &active, &completed] {
            admin
                .transition_task(&task.short_code, to)
                .await
                .expect("move");
        }
        back_date(&mut conn, &task.id, days);
        done_codes.push(task.short_code);
    }

    let all = admin
        .board_items(&board, &BoardItemsQuery::default())
        .await
        .expect("board items");
    assert_eq!(codes_in(&all, &completed).len(), 5);
    let limited = admin
        .board_items(
            &board,
            &BoardItemsQuery {
                done_limit: Some(2),
                ..BoardItemsQuery::default()
            },
        )
        .await
        .expect("board items with done_limit");
    let mut expected = vec![done_codes[0].clone(), done_codes[4].clone()];
    expected.sort();
    assert_eq!(
        codes_in(&limited, &completed),
        expected,
        "the last two cards by the time of entry, in short-code order"
    );
    let done_group = limited
        .columns
        .iter()
        .find(|g| g.column.id == completed)
        .expect("Completed");
    assert_eq!(done_group.item_count, 5, "item_count is the full column");
    assert_eq!(limited.total, all.total - 3, "total counts after the limit");
    assert_eq!(
        codes_in(&limited, &todo),
        codes_in(&all, &todo),
        "the open columns keep each card"
    );
    // `?code=` with the limit: a card that the limit cuts is not there.
    let cut = admin
        .board_items(
            &board,
            &BoardItemsQuery {
                done_limit: Some(2),
                code: Some(done_codes[1].clone()),
                ..BoardItemsQuery::default()
            },
        )
        .await
        .expect("one card");
    assert_eq!(cut.item_count(), 0, "the limit cuts {}", done_codes[1]);

    // --- 3. a done_limit under 1 is refused ----------------------------------
    let err = admin
        .board_items(
            &board,
            &BoardItemsQuery {
                done_limit: Some(0),
                ..BoardItemsQuery::default()
            },
        )
        .await
        .expect_err("done_limit 0");
    assert!(
        matches!(&err, Error::Validation { status: 422, field: Some(f), .. } if f == "done_limit"),
        "{err:?}"
    );

    drop(server);
    drop(pool);
    drop(conn);
    drop_scratch_db(&mut admin_conn, SCRATCH_DB);
}
