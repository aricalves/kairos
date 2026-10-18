//! Integration test for KAIROS-T-0625: each refusal of the edit rule and
//! the link rule, with its status, code, text and details.
//!
//! The facts of the edit rule are one query on the table of the item type
//! and the capability check; the admin role comes from the request
//! (`TenantContext::role`). The refusals did not change. This test pins
//! each of them, by a call of the server function with the role that the
//! tenant layer gives.
//!
//! Paths:
//! 1. `require_item_edit`, an item on a board: the capability on the board.
//! 2. `require_item_edit`, an item with no board: the admin role.
//! 3. `require_item_archive` by a member of an initiative board, who may
//!    edit: the grant on the board.
//! 4. `require_edge_write`: the two ends, one on a board and one with none.
//! 5. `require_edge_remove` of the `supports` edge of a document.
//!
//! And the role: the admin role of the request lets an edit through, and a
//! member role refuses, whatever the database says.
//!
//! Runs against the compose Postgres (`angreal services up`). Owns the
//! scratch database `kairos_edit_refusals_t0625_test`.

mod common;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use serde_json::json;
use uuid::Uuid;

use common::{recreate_scratch_db, with_database};
use kairos_core::abac as rules;
use kairos_core::short_code::ItemType;
use kairos_db::abac;
use kairos_db::models::{
    BoardLevel, NewAdr, NewBoard, NewBoardColumn, NewDocument, NewInitiative,
    NewOrganizationMember, NewTask, NewTeam, NewUser, OrgRole, TaskType, TeamType, User, WorkClass,
};
use kairos_db::{provision_tenant, run_public_migrations, schema};
use kairos_server::api::{
    require_edge_remove, require_edge_write, require_item_archive, require_item_edit,
};
use kairos_server::error::ApiError;

const SCRATCH_DB: &str = "kairos_edit_refusals_t0625_test";

fn insert_user(conn: &mut PgConnection, name: &str) -> Uuid {
    diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: format!("dex|{name}"),
            user_name: name.into(),
            email: format!("{name}@refco.test"),
            display_name: name.into(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("inserting user")
        .id
}

/// The refusal must be this 403 `FORBIDDEN`, with this text and these
/// details.
fn assert_refusal(
    case: &str,
    result: Result<(), ApiError>,
    message: &str,
    details: serde_json::Value,
) {
    let err = result.expect_err(case);
    assert_eq!(err.status.as_u16(), 403, "{case}: status");
    assert_eq!(err.code, "FORBIDDEN", "{case}: code");
    assert_eq!(err.message, message, "{case}: text");
    assert_eq!(err.details, details, "{case}: details");
}

#[test]
fn each_refusal_of_the_edit_and_link_rules_is_unchanged() {
    let _admin = recreate_scratch_db(SCRATCH_DB);
    let url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "refco", "Refco").expect("provisioning refco");
    sql_query("SET search_path TO org_refco, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    let conn = &mut conn;

    let org_id: Uuid = schema::organizations::table
        .filter(schema::organizations::slug.eq("refco"))
        .select(schema::organizations::id)
        .first(conn)
        .expect("org row");
    let maker = insert_user(conn, "maker");
    let outsider = insert_user(conn, "outsider");
    let planner = insert_user(conn, "planner");
    let db_admin = insert_user(conn, "dbadmin");
    diesel::insert_into(schema::organization_members::table)
        .values(&[
            NewOrganizationMember {
                organization_id: org_id,
                user_id: maker,
                role: OrgRole::Member,
            },
            NewOrganizationMember {
                organization_id: org_id,
                user_id: outsider,
                role: OrgRole::Member,
            },
            NewOrganizationMember {
                organization_id: org_id,
                user_id: planner,
                role: OrgRole::Member,
            },
            NewOrganizationMember {
                organization_id: org_id,
                user_id: db_admin,
                role: OrgRole::Admin,
            },
        ])
        .execute(conn)
        .expect("org members");

    // A delivery board of a team with no members, and its column.
    let team_id: Uuid = diesel::insert_into(schema::teams::table)
        .values(NewTeam {
            name: "Platform".into(),
            slug: "platform".into(),
            team_type: TeamType::Platform,
        })
        .returning(schema::teams::id)
        .get_result(conn)
        .expect("team");
    let delivery: Uuid = diesel::insert_into(schema::boards::table)
        .values(NewBoard {
            name: "Platform Delivery".into(),
            slug: "platform-delivery".into(),
            board_level: BoardLevel::Delivery,
            team_id: Some(team_id),
            // A prefix of its own (COLLIERY-T-3099).
            code_prefix: format!("B{}", &Uuid::new_v4().simple().to_string()[..8]).to_uppercase(),
        })
        .returning(schema::boards::id)
        .get_result(conn)
        .expect("delivery board");
    let column: Uuid = diesel::insert_into(schema::board_columns::table)
        .values(NewBoardColumn {
            board_id: delivery,
            name: "Todo".into(),
            position: 0,
            is_done: false,
            claims: false,
            keeps_claims: false,
        })
        .returning(schema::board_columns::id)
        .get_result(conn)
        .expect("column");
    let initiatives: Uuid = schema::boards::table
        .filter(schema::boards::slug.eq("initiatives"))
        .select(schema::boards::id)
        .first(conn)
        .expect("initiatives board");
    let discovery: Uuid = schema::board_columns::table
        .filter(schema::board_columns::board_id.eq(initiatives))
        .order(schema::board_columns::position.asc())
        .select(schema::board_columns::id)
        .first(conn)
        .expect("initiative column");

    let task: Uuid = diesel::insert_into(schema::tasks::table)
        .values(NewTask {
            short_code: "PLAT-T-0001".into(),
            title: "Task".into(),
            content: "".into(),
            board_id: delivery,
            column_id: column,
            task_type: TaskType::Task,
            work_class: WorkClass::Planned,
            team_id: Some(team_id),
            repository_id: None,
            created_by: maker,
            updated_by: maker,
        })
        .returning(schema::tasks::id)
        .get_result(conn)
        .expect("task");
    let initiative: Uuid = diesel::insert_into(schema::initiatives::table)
        .values(NewInitiative {
            short_code: "I-0001".into(),
            title: "Initiative".into(),
            content: "".into(),
            board_id: initiatives,
            column_id: discovery,
            complexity: None,
            is_bucket: false,
            bucket_type: None,
            created_by: maker,
            updated_by: maker,
        })
        .returning(schema::initiatives::id)
        .get_result(conn)
        .expect("initiative");
    let document: Uuid = diesel::insert_into(schema::documents::table)
        .values(NewDocument {
            short_code: "D-0001".into(),
            title: "Document".into(),
            content: "".into(),
            template_id: None,
            board_id: delivery,
            created_by: maker,
            updated_by: maker,
        })
        .returning(schema::documents::id)
        .get_result(conn)
        .expect("document");
    let adr: Uuid = diesel::insert_into(schema::adrs::table)
        .values(NewAdr {
            short_code: "A-0001".into(),
            title: "Off board".into(),
            content: "".into(),
            board_id: None,
            column_id: None,
            decision_maker: None,
            decision_date: None,
            created_by: maker,
            updated_by: maker,
        })
        .returning(schema::adrs::id)
        .get_result(conn)
        .expect("adr");
    // The planner holds a grant on the initiative board: a member of it.
    abac::grant_capability(
        conn,
        initiatives,
        planner,
        rules::TRANSITION_ITEMS,
        db_admin,
    )
    .expect("grant");

    let member = OrgRole::Member;

    // 1. An item on a board: the capability on the board.
    assert_refusal(
        "1 edit of a task",
        require_item_edit(conn, member, outsider, task, ItemType::Task),
        &format!("This action requires the capability \"manage_tasks\" on the board {delivery}."),
        json!({"required_capability": "manage_tasks", "board_id": delivery}),
    );
    assert_refusal(
        "1 edit of a document",
        require_item_edit(conn, member, outsider, document, ItemType::Document),
        &format!(
            "This action requires the capability \"manage_documents\" on the board {delivery}."
        ),
        json!({"required_capability": "manage_documents", "board_id": delivery}),
    );
    // 2. An item with no board: the admin role.
    assert_refusal(
        "2 edit of an off-board ADR",
        require_item_edit(conn, member, outsider, adr, ItemType::Adr),
        "This action requires the organization admin role.",
        json!({"required_capability": "manage_adrs", "board_id": null}),
    );
    // An id of a different type than the caller says is not found, so it
    // is refused as an item with no board.
    assert_refusal(
        "2 a task id sent as a document",
        require_item_edit(conn, member, maker, task, ItemType::Document),
        "This action requires the organization admin role.",
        json!({"required_capability": "manage_documents", "board_id": null}),
    );

    // 3. A member of an initiative board edits, and does not archive.
    require_item_edit(conn, member, planner, initiative, ItemType::Initiative)
        .expect("3 a member of the board edits");
    assert_refusal(
        "3 archive by a member of the board",
        require_item_archive(conn, member, planner, initiative, ItemType::Initiative),
        &format!(
            "This action requires the capability \"manage_initiatives\" on the board \
             {initiatives}."
        ),
        json!({"required_capability": "manage_initiatives", "board_id": initiatives}),
    );

    // 4. The link rule: the two ends.
    assert_refusal(
        "4 an edge from a task to an off-board ADR",
        require_edge_write(
            conn,
            member,
            outsider,
            "blocks",
            (task, ItemType::Task),
            (adr, ItemType::Adr),
        ),
        "A blocks edge needs \"manage_tasks\" on the board of the source, or the \
         organization admin role for the target.",
        json!({
            "relationship": "blocks",
            "required_capability": "manage_tasks",
            "board_id": delivery,
            "any_of": [
                {"end": "source", "required_capability": "manage_tasks", "board_id": delivery},
                {"end": "target", "required_capability": "manage_adrs", "board_id": null},
            ],
        }),
    );
    assert_refusal(
        "4 an edge from an initiative to a task",
        require_edge_write(
            conn,
            member,
            outsider,
            "parent",
            (initiative, ItemType::Initiative),
            (task, ItemType::Task),
        ),
        "A parent edge needs \"manage_initiatives\" on the board of the source, or \
         \"manage_tasks\" on the board of the target.",
        json!({
            "relationship": "parent",
            "required_capability": "manage_initiatives",
            "board_id": initiatives,
            "any_of": [
                {
                    "end": "source",
                    "required_capability": "manage_initiatives",
                    "board_id": initiatives,
                },
                {"end": "target", "required_capability": "manage_tasks", "board_id": delivery},
            ],
        }),
    );
    // The remove of an edge that is not the supports edge of a document is
    // the link rule.
    assert_refusal(
        "4 the remove of a blocks edge",
        require_edge_remove(
            conn,
            member,
            outsider,
            "blocks",
            (task, ItemType::Task),
            (adr, ItemType::Adr),
        ),
        "A blocks edge needs \"manage_tasks\" on the board of the source, or the \
         organization admin role for the target.",
        json!({
            "relationship": "blocks",
            "required_capability": "manage_tasks",
            "board_id": delivery,
            "any_of": [
                {"end": "source", "required_capability": "manage_tasks", "board_id": delivery},
                {"end": "target", "required_capability": "manage_adrs", "board_id": null},
            ],
        }),
    );

    // 5. The supports edge of a document: the right to edit the document.
    // The planner may edit the initiative, and that is not enough.
    assert_refusal(
        "5 the remove of a supports edge of a document",
        require_edge_remove(
            conn,
            member,
            planner,
            "supports",
            (initiative, ItemType::Initiative),
            (document, ItemType::Document),
        ),
        "To remove a supports edge of a document, you must be able to edit the document. \
         You need \"manage_documents\" on the owner board of the document. The creator of \
         the document and an organization admin can also remove it.",
        json!({
            "relationship": "supports",
            "required_capability": "manage_documents",
            "board_id": delivery,
            "any_of": [
                {"end": "target", "required_capability": "manage_documents", "board_id": delivery},
            ],
        }),
    );

    // The creator passes each.
    require_item_edit(conn, member, maker, task, ItemType::Task).expect("the creator edits");
    require_item_archive(conn, member, maker, adr, ItemType::Adr).expect("the creator archives");
    require_edge_remove(
        conn,
        member,
        maker,
        "supports",
        (initiative, ItemType::Initiative),
        (document, ItemType::Document),
    )
    .expect("the creator of the document removes its supports edge");

    // The role is the one of the request: the admin role lets the edit
    // through, and the database row of the user is not read again.
    require_item_edit(conn, OrgRole::Admin, outsider, adr, ItemType::Adr)
        .expect("the admin role of the request edits an item with no board");
    require_edge_write(
        conn,
        OrgRole::Admin,
        outsider,
        "blocks",
        (task, ItemType::Task),
        (adr, ItemType::Adr),
    )
    .expect("the admin role of the request writes the edge");
    assert_refusal(
        "the member role of the request refuses an org admin of the database",
        require_item_edit(conn, member, db_admin, adr, ItemType::Adr),
        "This action requires the organization admin role.",
        json!({"required_capability": "manage_adrs", "board_id": null}),
    );
}
