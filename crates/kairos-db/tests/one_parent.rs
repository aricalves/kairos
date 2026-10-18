//! KAIROS-T-0572 — an item has one parent, in the database layer.
//!
//! - `link_items` refuses a second parent with `ParentExists`, and the
//!   unique index refuses a direct insert too;
//! - `set_parent` swaps the edge in one transaction, writes the remove and
//!   the add rows, and writes nothing when nothing changes; a refused swap
//!   keeps the old edge;
//! - the migration `2026-10-17-002200_one_parent` on a tenant with two
//!   parents: it keeps the oldest edge, removes the others, writes one
//!   activity row for each, and is re-runnable.
//!
//! Against real Postgres from the compose stack (A-0012 tier 2). Each test
//! owns a scratch database.

#[path = "common/scratch.rs"]
mod scratch;

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Text, Uuid as SqlUuid};
use uuid::Uuid;

use kairos_db::graph::{self, GraphError};
use kairos_db::items::{self, CreateInitiative, CreateStrategy, CreateTask};
use kairos_db::models::{
    ActivityAction, BoardLevel, NewItemRelationship, NewUser, RelationshipType, TaskType, User,
};
use kairos_db::{create_board, provision_tenant, run_public_migrations, schema};

/// The migration under test, verbatim: what a deployment runs is what this
/// test runs.
const UP: &str = include_str!("../migrations/tenant/2026-10-17-002200_one_parent/up.sql");
const DOWN: &str = include_str!("../migrations/tenant/2026-10-17-002200_one_parent/down.sql");

/// The actor of the activity rows of the migration: the system.
const SYSTEM: Uuid = Uuid::nil();

/// A scratch database with the tenant `acme`, pinned to its schema, and
/// the URL of the database.
fn tenant(base: &str) -> (PgConnection, String) {
    let name = scratch::scratch_db_name(base);
    let admin_url = scratch::admin_database_url();
    let mut admin = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!("cannot connect to compose postgres at {admin_url}: {e} (`angreal services up`)")
    });
    sql_query(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&mut admin)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {name}"))
        .execute(&mut admin)
        .expect("creating scratch database");
    let (base_url, _) = admin_url.rsplit_once('/').expect("a database path");
    let url = format!("{base_url}/{name}");
    let mut conn = PgConnection::establish(&url).expect("connecting to scratch");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    (conn, url)
}

/// The items of a fixture: two strategies, two initiatives, three tasks.
struct Fixture {
    alice: Uuid,
    s1: Uuid,
    s2: Uuid,
    i1: Uuid,
    i2: Uuid,
    i3: Uuid,
    t1: Uuid,
    t2: Uuid,
    t3: Uuid,
}

fn fixture(conn: &mut PgConnection) -> Fixture {
    let alice = diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: "dex|alice".into(),
            user_name: "alice".into(),
            email: "alice@acme.test".into(),
            display_name: "Alice".into(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("inserting user")
        .id;
    let board = |conn: &mut PgConnection, slug: &str| -> Uuid {
        schema::boards::table
            .filter(schema::boards::slug.eq(slug))
            .select(schema::boards::id)
            .first(conn)
            .expect("a seeded board")
    };
    let strategy_board = board(conn, "strategy");
    let initiative_board = board(conn, "initiatives");
    let team: Uuid = diesel::insert_into(schema::teams::table)
        .values(kairos_db::models::teams::NewTeam {
            name: "Delivery Team".into(),
            slug: "delivery-team".into(),
            team_type: kairos_db::models::TeamType::StreamAligned,
        })
        .returning(schema::teams::id)
        .get_result(conn)
        .expect("inserting team");
    let delivery_board = create_board(
        conn,
        BoardLevel::Delivery,
        "Delivery",
        "delivery",
        kairos_db::CodePrefix::Given("ACME"),
        Some(team),
        Some(alice),
    )
    .expect("creating delivery board")
    .id;
    let strategy = |conn: &mut PgConnection, title: &str| {
        items::create_strategy(
            conn,
            CreateStrategy {
                board_id: strategy_board,
                column_id: None,
                title,
                content: "",
                hypothesis: None,
            },
            alice,
        )
        .expect("creating strategy")
        .id
    };
    let initiative = |conn: &mut PgConnection, title: &str| {
        items::create_initiative(
            conn,
            CreateInitiative {
                board_id: initiative_board,
                column_id: None,
                title,
                content: "",
                complexity: None,
                bucket_type: None,
            },
            alice,
        )
        .expect("creating initiative")
        .id
    };
    let task = |conn: &mut PgConnection, title: &str| {
        items::create_task(
            conn,
            CreateTask {
                board_id: delivery_board,
                column_id: None,
                title,
                content: "",
                task_type: TaskType::Task,
                work_class: kairos_db::models::enums::WorkClass::Planned,
                repository_id: None,
            },
            alice,
        )
        .expect("creating task")
        .id
    };
    Fixture {
        alice,
        s1: strategy(conn, "Strategy One"),
        s2: strategy(conn, "Strategy Two"),
        i1: initiative(conn, "Initiative One"),
        i2: initiative(conn, "Initiative Two"),
        i3: initiative(conn, "Initiative Three"),
        t1: task(conn, "Task One"),
        t2: task(conn, "Task Two"),
        t3: task(conn, "Task Three"),
    }
}

/// The sources of the parent edges of `item`, oldest first.
fn parents(conn: &mut PgConnection, item: Uuid) -> Vec<Uuid> {
    schema::item_relationships::table
        .filter(schema::item_relationships::target_id.eq(item))
        .filter(schema::item_relationships::relationship.eq(RelationshipType::Parent))
        .order((
            schema::item_relationships::created_at.asc(),
            schema::item_relationships::id.asc(),
        ))
        .select(schema::item_relationships::source_id)
        .load(conn)
        .expect("reading parent edges")
}

/// The details of the relationship rows of `action` by `actor`, in order.
fn activity(conn: &mut PgConnection, action: ActivityAction, actor: Uuid) -> Vec<String> {
    schema::activity_log::table
        .filter(schema::activity_log::action.eq(action.as_str()))
        .filter(schema::activity_log::actor_id.eq(actor))
        .order((
            schema::activity_log::occurred_at.asc(),
            schema::activity_log::id.asc(),
        ))
        .select(schema::activity_log::details)
        .load(conn)
        .expect("reading activity_log")
}

fn code(conn: &mut PgConnection, id: Uuid) -> String {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = Text)]
        short_code: String,
    }
    sql_query("SELECT short_code FROM entity_directory WHERE id = $1")
        .bind::<SqlUuid, _>(id)
        .get_result::<Row>(conn)
        .expect("a short code")
        .short_code
}

#[test]
fn a_second_parent_is_refused_and_set_parent_swaps_the_edge() {
    static SCRATCH: scratch::ScratchName = scratch::ScratchName::new("kairos_one_parent_t0572");
    let (mut conn, url) = tenant(&SCRATCH);
    let _guard = scratch::ScratchGuard::new(&SCRATCH);
    let f = fixture(&mut conn);
    let (t1_code, i1_code, i2_code, i3_code) = (
        code(&mut conn, f.t1),
        code(&mut conn, f.i1),
        code(&mut conn, f.i2),
        code(&mut conn, f.i3),
    );

    // ---- link_items: one parent ----------------------------------------
    graph::link_items(&mut conn, f.i1, f.t1, RelationshipType::Parent, f.alice)
        .expect("the first parent");
    match graph::link_items(&mut conn, f.i2, f.t1, RelationshipType::Parent, f.alice) {
        Err(GraphError::ParentExists { item, parent }) => {
            assert_eq!(item, t1_code);
            assert_eq!(parent, i1_code);
        }
        other => panic!("a second parent must be ParentExists, got {other:?}"),
    }
    // The same parent again is the duplicate, as before.
    assert!(matches!(
        graph::link_items(&mut conn, f.i1, f.t1, RelationshipType::Parent, f.alice),
        Err(GraphError::AlreadyLinked { .. })
    ));
    assert_eq!(parents(&mut conn, f.t1), vec![f.i1]);

    // The database refuses a second parent by itself: the unique index.
    let direct = diesel::insert_into(schema::item_relationships::table)
        .values(NewItemRelationship {
            source_id: f.i2,
            target_id: f.t1,
            relationship: RelationshipType::Parent,
        })
        .execute(&mut conn);
    match direct {
        Err(diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::UniqueViolation,
            info,
        )) => assert_eq!(info.constraint_name(), Some(graph::ONE_PARENT_INDEX)),
        other => panic!("the index must refuse a second parent, got {other:?}"),
    }
    // Other relationships are not limited: two blocks edges to one task.
    graph::link_items(&mut conn, f.t2, f.t1, RelationshipType::Blocks, f.alice).expect("a blocker");
    graph::link_items(&mut conn, f.t3, f.t1, RelationshipType::Blocks, f.alice)
        .expect("a second blocker");

    // ---- set_parent: the swap ------------------------------------------
    let change = graph::set_parent(&mut conn, f.t1, Some(f.i2), f.alice).expect("the swap");
    assert_eq!(change.item, t1_code);
    assert_eq!(change.previous.as_deref(), Some(i1_code.as_str()));
    assert_eq!(change.parent.as_deref(), Some(i2_code.as_str()));
    assert!(change.changed);
    assert_eq!(parents(&mut conn, f.t1), vec![f.i2]);
    assert_eq!(
        activity(&mut conn, ActivityAction::RelationshipRemove, f.alice),
        vec![format!("relationship:parent:{i1_code}->{t1_code}")]
    );
    assert_eq!(
        activity(&mut conn, ActivityAction::RelationshipAdd, f.alice)
            .last()
            .cloned(),
        Some(format!("relationship:parent:{i2_code}->{t1_code}"))
    );
    // KAIROS-T-0638: each row of the swap names the source of its edge:
    // the old parent for the remove, the new parent for the add.
    let entity_of = |conn: &mut PgConnection, action: ActivityAction, details: String| {
        schema::activity_log::table
            .filter(schema::activity_log::actor_id.eq(f.alice))
            .filter(schema::activity_log::action.eq(action.as_str()))
            .filter(schema::activity_log::details.eq(details))
            .select((
                schema::activity_log::entity_id,
                schema::activity_log::entity_type,
            ))
            .first::<(Option<Uuid>, Option<String>)>(conn)
            .expect("the row of the swap")
    };
    assert_eq!(
        entity_of(
            &mut conn,
            ActivityAction::RelationshipRemove,
            format!("relationship:parent:{i1_code}->{t1_code}")
        ),
        (Some(f.i1), Some("initiative".to_string()))
    );
    assert_eq!(
        entity_of(
            &mut conn,
            ActivityAction::RelationshipAdd,
            format!("relationship:parent:{i2_code}->{t1_code}")
        ),
        (Some(f.i2), Some("initiative".to_string()))
    );

    // The same parent again writes nothing.
    let rows_before = activity(&mut conn, ActivityAction::RelationshipAdd, f.alice).len();
    let same = graph::set_parent(&mut conn, f.t1, Some(f.i2), f.alice).expect("no change");
    assert!(!same.changed);
    assert_eq!(same.previous, same.parent);
    assert_eq!(
        activity(&mut conn, ActivityAction::RelationshipAdd, f.alice).len(),
        rows_before
    );

    // A refused swap keeps the old edge: a strategy cannot be the parent of
    // a task, and the type rule refuses it before the old edge goes.
    assert!(matches!(
        graph::set_parent(&mut conn, f.t1, Some(f.s1), f.alice),
        Err(GraphError::Rule(_))
    ));
    assert_eq!(parents(&mut conn, f.t1), vec![f.i2]);

    // Two swaps at the same time: one wins, the other is ParentExists and
    // writes nothing. t2 has the parent i1. Writer B swaps it to i2 and
    // does not commit yet. Writer A swaps it to i3: its delete waits for B.
    // B commits; the insert of A meets the index, and A rolls back.
    graph::set_parent(&mut conn, f.t2, Some(f.i1), f.alice).expect("t2 under i1");
    let removes_before = activity(&mut conn, ActivityAction::RelationshipRemove, f.alice).len();
    let mut writer_b = PgConnection::establish(&url).expect("connection B");
    writer_b
        .batch_execute(&format!(
            "SET search_path TO org_acme, public; BEGIN; \
             DELETE FROM item_relationships WHERE target_id = '{t2}' AND relationship = 'parent'; \
             INSERT INTO item_relationships (source_id, target_id, relationship) \
             VALUES ('{i2}', '{t2}', 'parent');",
            t2 = f.t2,
            i2 = f.i2,
        ))
        .expect("writer B starts its swap");
    let writer_a = {
        let (url, t2, i3, alice) = (url.clone(), f.t2, f.i3, f.alice);
        std::thread::spawn(move || {
            let mut conn = PgConnection::establish(&url).expect("connection A");
            sql_query("SET search_path TO org_acme, public")
                .execute(&mut conn)
                .expect("pinning search_path");
            graph::set_parent(&mut conn, t2, Some(i3), alice)
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(500));
    writer_b.batch_execute("COMMIT").expect("writer B commits");
    match writer_a.join().expect("writer A ends") {
        Err(GraphError::ParentExists { item, parent }) => {
            assert_eq!(item, code(&mut conn, f.t2));
            assert_eq!(parent, i2_code);
        }
        other => panic!("the late writer must be ParentExists, got {other:?}"),
    }
    assert_eq!(parents(&mut conn, f.t2), vec![f.i2], "the swap of B stays");
    assert_eq!(
        activity(&mut conn, ActivityAction::RelationshipRemove, f.alice).len(),
        removes_before,
        "the refused swap of A wrote no row"
    );

    // Remove: null takes the edge away; a second remove changes nothing.
    let removed = graph::set_parent(&mut conn, f.t1, None, f.alice).expect("the remove");
    assert!(removed.changed);
    assert_eq!(removed.previous.as_deref(), Some(i2_code.as_str()));
    assert_eq!(removed.parent, None);
    assert!(parents(&mut conn, f.t1).is_empty());
    let again = graph::set_parent(&mut conn, f.t1, None, f.alice).expect("no change");
    assert!(!again.changed);

    // A set on an item with no parent is a plain add.
    let added = graph::set_parent(&mut conn, f.t1, Some(f.i3), f.alice).expect("the add");
    assert!(added.changed);
    assert_eq!(added.previous, None);
    assert_eq!(added.parent.as_deref(), Some(i3_code.as_str()));

    // The other level: an initiative under a strategy.
    graph::set_parent(&mut conn, f.i1, Some(f.s1), f.alice).expect("strategy parent");
    graph::set_parent(&mut conn, f.i1, Some(f.s2), f.alice).expect("strategy swap");
    assert_eq!(parents(&mut conn, f.i1), vec![f.s2]);
    // An item is not its own parent.
    assert!(matches!(
        graph::set_parent(&mut conn, f.i1, Some(f.i1), f.alice),
        Err(GraphError::SelfLink(_))
    ));
    // The parent of the parent stays a parent: no cycle through the swap.
    assert_eq!(
        graph::parent_of(&mut conn, f.i1)
            .expect("reading the parent")
            .map(|p| p.id),
        Some(f.s2)
    );
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

fn index_is_there(conn: &mut PgConnection) -> bool {
    sql_query(
        "SELECT count(*) AS count FROM pg_indexes \
          WHERE schemaname = 'org_acme' AND indexname = 'item_relationships_one_parent'",
    )
    .get_result::<Count>(conn)
    .expect("reading pg_indexes")
    .count
        == 1
}

/// One parent edge of old data, written directly, with the day of 2026-01
/// that it was made.
fn old_edge(conn: &mut PgConnection, source: Uuid, target: Uuid, day: u32) {
    sql_query(
        "INSERT INTO item_relationships (source_id, target_id, relationship, created_at) \
         VALUES ($1, $2, 'parent', $3::timestamptz)",
    )
    .bind::<SqlUuid, _>(source)
    .bind::<SqlUuid, _>(target)
    .bind::<Text, _>(format!("2026-01-{day:02}T00:00:00Z"))
    .execute(conn)
    .expect("writing an old edge");
}

#[test]
fn the_migration_keeps_the_oldest_parent_and_logs_the_others() {
    static SCRATCH: scratch::ScratchName =
        scratch::ScratchName::new("kairos_one_parent_migration_t0572");
    let (mut conn, _) = tenant(&SCRATCH);
    let _guard = scratch::ScratchGuard::new(&SCRATCH);
    let f = fixture(&mut conn);

    // The state of a tenant before the migration: no index.
    conn.batch_execute(DOWN).expect("the down migration");
    assert!(
        !index_is_there(&mut conn),
        "the down migration drops the index"
    );

    // ---- the old data ----------------------------------------------------
    // t1 has three parents: i2 is the oldest, then i1, then i3.
    old_edge(&mut conn, f.i1, f.t1, 10);
    old_edge(&mut conn, f.i2, f.t1, 3);
    old_edge(&mut conn, f.i3, f.t1, 20);
    // i1 has two parents: s1 is the oldest. The kept parent is archived;
    // it stays all the same (the oldest edge wins).
    old_edge(&mut conn, f.s2, f.i1, 15);
    old_edge(&mut conn, f.s1, f.i1, 1);
    sql_query("UPDATE strategies SET deleted_at = now() WHERE id = $1")
        .bind::<SqlUuid, _>(f.s1)
        .execute(&mut conn)
        .expect("archiving s1");
    // t2 has one parent, and t3 none: not changed.
    old_edge(&mut conn, f.i1, f.t2, 5);

    let codes: Vec<String> = [f.t1, f.i1, f.i2, f.i3, f.s1, f.s2]
        .into_iter()
        .map(|id| code(&mut conn, id))
        .collect();
    let [t1, i1, i2, i3, s1, s2] = codes.as_slice() else {
        unreachable!()
    };

    // ---- the migration ---------------------------------------------------
    conn.batch_execute(UP).expect("the up migration");

    assert_eq!(parents(&mut conn, f.t1), vec![f.i2], "t1 keeps the oldest");
    assert_eq!(parents(&mut conn, f.i1), vec![f.s1], "i1 keeps the oldest");
    assert_eq!(parents(&mut conn, f.t2), vec![f.i1], "t2 is not changed");
    assert!(parents(&mut conn, f.t3).is_empty(), "t3 is not changed");
    assert!(index_is_there(&mut conn), "the index is there");

    let mut rows = activity(&mut conn, ActivityAction::RelationshipRemove, SYSTEM);
    rows.sort();
    let mut expected = vec![
        format!(
            "relationship:parent:{i1}->{t1} (KAIROS-T-0572: one parent per item; {i2} stays the parent)"
        ),
        format!(
            "relationship:parent:{i3}->{t1} (KAIROS-T-0572: one parent per item; {i2} stays the parent)"
        ),
        format!(
            "relationship:parent:{s2}->{i1} (KAIROS-T-0572: one parent per item; {s1} stays the parent)"
        ),
    ];
    expected.sort();
    assert_eq!(rows, expected, "one system row for each removed edge");

    // Re-runnable: a second run removes nothing and writes nothing.
    conn.batch_execute(UP).expect("the second run");
    assert_eq!(
        activity(&mut conn, ActivityAction::RelationshipRemove, SYSTEM).len(),
        3
    );
    assert_eq!(parents(&mut conn, f.t1), vec![f.i2]);
}
