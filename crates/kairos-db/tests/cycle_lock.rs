//! KAIROS-T-0634 — the cycle check takes a lock, and the archive cascade
//! locks its rows in the order of the ids:
//!
//! - two `blocks` links in opposite directions: the second waits on the
//!   lock of the check, then sees the edge of the first and is refused with
//!   a cycle. Only one of the two edges is in the database.
//! - an archive and a blocker that closes: the close holds the lowest row
//!   of the cascade; the cascade waits on that row and holds no other row
//!   of the table. A blocker close locks its dependants in the same order
//!   ([`kairos_db::dispatch::on_dependants_freed`]), so the two cannot lock
//!   each other out.
//!
//! Each case is in order, not by time: connection A makes its change in an
//! open transaction; a thread on connection B starts the other change; A
//! commits only when Postgres shows B waiting on a lock.
//!
//! Against real Postgres from the compose stack (A-0012 tier 2), in its
//! own scratch database.

#[path = "common/scratch.rs"]
mod scratch;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Integer, Text, Uuid as SqlUuid};
use uuid::Uuid;

use kairos_core::short_code::ItemType;
use kairos_db::boards;
use kairos_db::graph::{self, GraphError};
use kairos_db::items::{CreateInitiative, CreateTask};
use kairos_db::models::enums::{BoardLevel, RelationshipType, TaskType, WorkClass};
use kairos_db::models::teams::NewTeam;
use kairos_db::models::{NewUser, TeamType, User};
use kairos_db::{create_board, items, provision_tenant, run_public_migrations, schema};

static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_cycle_lock_t0634");

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL must contain a database path segment");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

/// A connection to the scratch database, in the tenant schema.
fn tenant_conn(url: &str) -> PgConnection {
    let mut conn = PgConnection::establish(url).expect("connecting to scratch database");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    conn
}

fn insert_user(conn: &mut PgConnection, external_id: &str, name: &str) -> Uuid {
    diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: external_id.to_string(),
            user_name: external_id.to_string(),
            email: format!("{external_id}@acme.test"),
            display_name: name.to_string(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("inserting user")
        .id
}

fn seed_board(conn: &mut PgConnection, actor: Uuid) -> Uuid {
    let team_id: Uuid = diesel::insert_into(schema::teams::table)
        .values(NewTeam {
            name: "Web".to_string(),
            slug: "web".to_string(),
            team_type: TeamType::StreamAligned,
        })
        .returning(schema::teams::id)
        .get_result(conn)
        .expect("inserting team");
    create_board(
        conn,
        BoardLevel::Delivery,
        "Web",
        "web-delivery",
        kairos_db::CodePrefix::FromSlug,
        Some(team_id),
        Some(actor),
    )
    .expect("creating delivery board")
    .id
}

fn board_id_by_slug(conn: &mut PgConnection, slug: &str) -> Uuid {
    schema::boards::table
        .filter(schema::boards::slug.eq(slug))
        .select(schema::boards::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("board {slug:?} not found: {e}"))
}

fn column(conn: &mut PgConnection, board_id: Uuid, name: &str) -> Uuid {
    schema::board_columns::table
        .filter(schema::board_columns::board_id.eq(board_id))
        .filter(schema::board_columns::name.eq(name))
        .filter(schema::board_columns::deleted_at.is_null())
        .select(schema::board_columns::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("column {name}: {e}"))
}

fn task(conn: &mut PgConnection, board_id: Uuid, title: &str, actor: Uuid) -> Uuid {
    items::create_task(
        conn,
        CreateTask {
            board_id,
            column_id: None,
            title,
            content: "",
            task_type: TaskType::Task,
            work_class: WorkClass::Planned,
            repository_id: None,
        },
        actor,
    )
    .expect("creating task")
    .id
}

fn edge_count(conn: &mut PgConnection, source: Uuid, target: Uuid) -> i64 {
    schema::item_relationships::table
        .filter(schema::item_relationships::source_id.eq(source))
        .filter(schema::item_relationships::target_id.eq(target))
        .filter(schema::item_relationships::relationship.eq(RelationshipType::Blocks))
        .count()
        .get_result(conn)
        .expect("counting edges")
}

#[derive(QueryableByName)]
struct Pid {
    #[diesel(sql_type = Integer)]
    pid: i32,
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Wait until the backend `pid` waits on a lock of the kind `event`
/// (`advisory`, `tuple` or `transactionid`; `None` = any kind). Panics
/// after 30 s.
fn wait_for_lock_wait(watcher: &mut PgConnection, pid: i32, event: Option<&str>) {
    let start = Instant::now();
    loop {
        let waiting: Count = sql_query(
            "SELECT count(*) AS n FROM pg_stat_activity \
             WHERE pid = $1 AND wait_event_type = 'Lock' \
               AND ($2::text IS NULL OR wait_event = $2)",
        )
        .bind::<Integer, _>(pid)
        .bind::<diesel::sql_types::Nullable<Text>, _>(event)
        .get_result(watcher)
        .expect("pg_stat_activity");
        if waiting.n == 1 {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "connection B never waited on a lock ({event:?})"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run `first` on `conn_a` in an open transaction; start `second` on a new
/// connection in a thread; when B waits on a lock of the kind `event`, run
/// `while_b_waits` on a new connection, then commit A. Gives the result of
/// `second`.
fn race<T: Send + 'static>(
    url: &str,
    conn_a: &mut PgConnection,
    event: Option<&str>,
    first: impl FnOnce(&mut PgConnection),
    second: impl FnOnce(&mut PgConnection) -> T + Send + 'static,
    while_b_waits: impl FnOnce(&mut PgConnection),
) -> T {
    let mut watcher = tenant_conn(url);
    let (pid_tx, pid_rx) = mpsc::channel();
    let b_url = url.to_string();
    let b = conn_a
        .transaction::<_, diesel::result::Error, _>(|a| {
            first(a);
            let b = thread::spawn(move || {
                let mut conn_b = tenant_conn(&b_url);
                let me: Pid = sql_query("SELECT pg_backend_pid() AS pid")
                    .get_result(&mut conn_b)
                    .expect("pid");
                pid_tx.send(me.pid).expect("send pid");
                second(&mut conn_b)
            });
            let pid = pid_rx.recv().expect("pid of B");
            wait_for_lock_wait(&mut watcher, pid, event);
            while_b_waits(&mut watcher);
            Ok(b)
        })
        .expect("transaction A");
    b.join().expect("thread B")
}

/// Whether the row `id` of `tasks` can be locked now, with no wait. A row
/// that another transaction holds gives `lock_not_available`.
fn task_row_is_free(conn: &mut PgConnection, id: Uuid) -> bool {
    let locked = conn.transaction::<_, diesel::result::Error, _>(|conn| {
        sql_query("SELECT id FROM tasks WHERE id = $1 FOR UPDATE NOWAIT")
            .bind::<SqlUuid, _>(id)
            .execute(conn)
    });
    match locked {
        Ok(_) => true,
        Err(diesel::result::Error::DatabaseError(_, info))
            if info.message().contains("could not obtain lock") =>
        {
            false
        }
        Err(e) => panic!("probe of the row {id}: {e}"),
    }
}

#[test]
fn concurrent_links_and_the_cascade_take_their_locks_in_turn() {
    let admin_url = scratch::admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!(
            "cannot connect to compose postgres at {admin_url}: {e} \
             (is the stack up? `angreal services up`)"
        )
    });
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch database");
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let url = with_database(&admin_url, &SCRATCH_DB);
    {
        let mut conn = PgConnection::establish(&url).expect("connecting to scratch database");
        run_public_migrations(&mut conn).expect("running public migrations");
        provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    }
    let mut conn = tenant_conn(&url);
    let alice = insert_user(&mut conn, "alice", "Alice");
    let board = seed_board(&mut conn, alice);

    // --- two links in opposite directions -------------------------------------
    let t1 = task(&mut conn, board, "One", alice);
    let t2 = task(&mut conn, board, "Two", alice);
    let second = race(
        &url,
        &mut conn,
        Some("advisory"),
        |a| {
            graph::link_items(a, t1, t2, RelationshipType::Blocks, alice).expect("A links t1→t2");
        },
        move |b| graph::link_items(b, t2, t1, RelationshipType::Blocks, alice),
        |_| {},
    );
    assert!(
        matches!(
            second,
            Err(GraphError::CycleDetected {
                relationship: RelationshipType::Blocks,
                ..
            })
        ),
        "B waits for A, sees the edge t1→t2 and is refused with a cycle, got {second:?}"
    );
    assert_eq!(edge_count(&mut conn, t1, t2), 1, "the edge of A is there");
    assert_eq!(edge_count(&mut conn, t2, t1), 0, "the edge of B is not");

    // A transitive cycle the same way: t2→t3 in A, t3→t1 in B.
    let t3 = task(&mut conn, board, "Three", alice);
    let second = race(
        &url,
        &mut conn,
        Some("advisory"),
        |a| {
            graph::link_items(a, t2, t3, RelationshipType::Blocks, alice).expect("A links t2→t3");
        },
        move |b| graph::link_items(b, t3, t1, RelationshipType::Blocks, alice),
        |_| {},
    );
    assert!(
        matches!(second, Err(GraphError::CycleDetected { .. })),
        "B is refused with a cycle t1→t2→t3→t1, got {second:?}"
    );
    assert_eq!(edge_count(&mut conn, t3, t1), 0);

    // The check walks only the edges of its own relationship: an edge that
    // makes no cycle passes after the wait.
    let t4 = task(&mut conn, board, "Four", alice);
    let second = race(
        &url,
        &mut conn,
        Some("advisory"),
        |a| {
            graph::link_items(a, t3, t4, RelationshipType::Blocks, alice).expect("A links t3→t4");
        },
        move |b| graph::link_items(b, t1, t4, RelationshipType::Blocks, alice),
        |_| {},
    );
    assert!(second.is_ok(), "a diamond is not a cycle: {second:?}");

    // --- an archive and a blocker that closes ----------------------------------
    // An initiative with two tasks. `low` has the lower id. The blocker `x`
    // blocks `low`; its close locks `low`.
    let initiatives = board_id_by_slug(&mut conn, "initiatives");
    let initiative = items::create_initiative(
        &mut conn,
        CreateInitiative {
            board_id: initiatives,
            column_id: None,
            title: "Parent",
            content: "",
            complexity: None,
            bucket_type: None,
        },
        alice,
    )
    .expect("creating initiative")
    .id;
    // The row of `high` is first in the table, so an `UPDATE` in the order
    // of a scan takes `high` first. Each try gives a lower id with a
    // chance of one half.
    let high = task(&mut conn, board, "Child high", alice);
    let low = (0..64)
        .map(|n| task(&mut conn, board, &format!("Child low {n}"), alice))
        .find(|id| *id < high)
        .expect("a task with a lower id");
    for child in [low, high] {
        graph::link_items(
            &mut conn,
            initiative,
            child,
            RelationshipType::Parent,
            alice,
        )
        .expect("parent edge");
    }
    let x = task(&mut conn, board, "Blocker", alice);
    graph::link_items(&mut conn, x, low, RelationshipType::Blocks, alice).expect("x blocks low");
    for name in ["Todo", "Active"] {
        let to = column(&mut conn, board, name);
        boards::transition_task(&mut conn, x, to, alice).expect("move x");
    }
    let completed = column(&mut conn, board, "Completed");
    let probe_url = url.clone();

    let archived = race(
        &url,
        &mut conn,
        None,
        |a| {
            boards::transition_task(a, x, completed, alice).expect("A closes the blocker");
            assert!(
                !task_row_is_free(&mut tenant_conn(&probe_url), low),
                "the close holds the row of low"
            );
        },
        move |b| items::soft_delete_item(b, ItemType::Initiative, initiative, alice),
        |probe| {
            assert!(
                task_row_is_free(probe, high),
                "the cascade waits on the lowest row and holds no row after it"
            );
        },
    );
    let outcome = archived.expect("the archive passes after the close");
    assert_eq!(outcome.cascaded_short_codes.len(), 2, "{outcome:?}");
}
