//! KAIROS-T-0633 — 20 creates at the same time on one board.
//!
//! The number of a short code comes from the row (prefix, type) of
//! `short_code_sequences`, and the row stays locked until the create
//! commits. This test starts 20 task creates on one board at the same
//! time, on 20 connections, and checks:
//!
//! - each create passes;
//! - each code is unique, and the numbers are contiguous (no gaps);
//! - the history row, the `activity_log` row, the mention notification and
//!   the `item_created` event of each task name its final code.
//!
//! It prints the wall time of the 20 creates on one board, and of 20
//! creates on 20 boards (no shared row), for the record on the task.
//!
//! Against real Postgres from the compose stack (A-0012 tier 2), in its
//! own scratch database.

#[path = "common/scratch.rs"]
mod scratch;

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use uuid::Uuid;

use kairos_db::items::CreateTask;
use kairos_db::models::enums::{ActivityAction, BoardLevel, TaskType, WorkClass};
use kairos_db::models::teams::NewTeam;
use kairos_db::models::{NewUser, TeamType, User};
use kairos_db::{create_board, items, provision_tenant, run_public_migrations};

static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_short_code_t0633");

const CREATES: usize = 20;
const ROUNDS: usize = 8;

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
    use kairos_db::schema::users;
    diesel::insert_into(users::table)
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

/// A team and its delivery board with the prefix `prefix`.
fn seed_board(conn: &mut PgConnection, slug: &str, prefix: &str, actor: Uuid) -> Uuid {
    use kairos_db::schema::teams;
    let team_id: Uuid = diesel::insert_into(teams::table)
        .values(NewTeam {
            name: slug.to_string(),
            slug: slug.to_string(),
            team_type: TeamType::StreamAligned,
        })
        .returning(teams::id)
        .get_result(conn)
        .expect("inserting team");
    create_board(
        conn,
        BoardLevel::Delivery,
        slug,
        &format!("{slug}-delivery"),
        kairos_db::CodePrefix::Given(prefix),
        Some(team_id),
        Some(actor),
    )
    .expect("creating delivery board")
    .id
}

/// Create one task on each board of `boards` at the same time, one thread
/// and one connection each. Gives the wall time from the start signal to
/// the last commit, and the (id, code) of each task.
fn create_at_once(url: &str, boards: &[Uuid], actor: Uuid) -> (Duration, Vec<(Uuid, String)>) {
    let barrier = Arc::new(Barrier::new(boards.len() + 1));
    let handles: Vec<_> = boards
        .iter()
        .enumerate()
        .map(|(i, &board_id)| {
            let url = url.to_string();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut conn = tenant_conn(&url);
                let title = format!("Task {i}");
                barrier.wait();
                let task = items::create_task(
                    &mut conn,
                    CreateTask {
                        board_id,
                        column_id: None,
                        title: &title,
                        content: "For @bob.",
                        task_type: TaskType::Task,
                        work_class: WorkClass::Planned,
                        repository_id: None,
                    },
                    actor,
                )
                .unwrap_or_else(|e| panic!("create {i}: {e}"));
                (task.id, task.short_code, Instant::now())
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    let mut out = Vec::new();
    let mut last = start;
    for handle in handles {
        let (id, code, done) = handle.join().expect("create thread");
        last = last.max(done);
        out.push((id, code));
    }
    (last - start, out)
}

/// The number of a code `PREFIX-T-NNNN`.
fn number(code: &str) -> i64 {
    code.rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{code} has no number"))
}

#[test]
fn twenty_creates_at_once_on_one_board_get_contiguous_unique_codes() {
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
    let bob = insert_user(&mut conn, "bob", "Bob");
    // A mention notifies a member of the organization only.
    {
        use kairos_db::models::{NewOrganizationMember, OrgRole};
        use kairos_db::schema::{organization_members, organizations};
        let org_id: Uuid = organizations::table
            .filter(organizations::slug.eq("acme"))
            .select(organizations::id)
            .first(&mut conn)
            .expect("acme");
        diesel::insert_into(organization_members::table)
            .values(NewOrganizationMember {
                organization_id: org_id,
                user_id: bob,
                role: OrgRole::Member,
            })
            .execute(&mut conn)
            .expect("membership");
    }
    let board = seed_board(&mut conn, "web", "WEB", alice);
    let others: Vec<Uuid> = (0..CREATES)
        .map(|i| seed_board(&mut conn, &format!("b{i:02}"), &format!("B{i:02}"), alice))
        .collect();

    let mut listener = tenant_conn(&url);
    sql_query("LISTEN kairos_events")
        .execute(&mut listener)
        .expect("LISTEN");

    // A warm-up create, so the row (WEB, T) is there and the first create
    // of the race does not make it.
    let (_, warm) = create_at_once(&url, &[board], alice);
    let first = number(&warm[0].1) + 1;

    // --- 20 creates on one board --------------------------------------------
    let (_, created) = create_at_once(&url, &vec![board; CREATES], alice);
    let codes: BTreeSet<String> = created.iter().map(|(_, c)| c.clone()).collect();
    assert_eq!(codes.len(), CREATES, "each code is unique: {codes:?}");
    let numbers: BTreeSet<i64> = codes.iter().map(|c| number(c)).collect();
    let expected: BTreeSet<i64> = (first..first + CREATES as i64).collect();
    assert_eq!(numbers, expected, "the numbers are contiguous: {codes:?}");
    assert!(codes.iter().all(|c| c.starts_with("WEB-T-")), "{codes:?}");

    // --- each record names the final code ------------------------------------
    for (id, code) in &created {
        use kairos_db::schema::{activity_log, item_history, notifications, tasks};
        let stored: String = tasks::table
            .filter(tasks::id.eq(id))
            .select(tasks::short_code)
            .first(&mut conn)
            .expect("task row");
        assert_eq!(&stored, code, "the row has the code the create gave");
        let history: i64 = item_history::table
            .filter(item_history::item_id.eq(id))
            .filter(item_history::version.eq(1))
            .count()
            .get_result(&mut conn)
            .expect("history");
        assert_eq!(history, 1, "{code}: the v1 history row");
        let details: Vec<String> = activity_log::table
            .filter(activity_log::entity_id.eq(id))
            .filter(activity_log::action.eq(ActivityAction::Create))
            .select(activity_log::details)
            .load(&mut conn)
            .expect("activity");
        assert_eq!(details, vec![format!("short_code:{code}")], "{code}");
        let texts: Vec<String> = notifications::table
            .filter(notifications::item_id.eq(id))
            .filter(notifications::user_id.eq(bob))
            .select(notifications::text)
            .load(&mut conn)
            .expect("notifications");
        assert_eq!(texts.len(), 1, "{code}: one mention notification");
        assert!(
            texts[0].contains(code.as_str()),
            "{code}: the mention names the final code: {texts:?}"
        );
    }

    // Each `item_created` event of the race names a final code.
    thread::sleep(Duration::from_millis(300));
    let event_codes: BTreeSet<String> = listener
        .notifications_iter()
        .map(|n| n.expect("notification").payload)
        .filter_map(|payload| {
            let v: serde_json::Value = serde_json::from_str(&payload).expect("event json");
            (v["event"] == "item_created" && v["entity_type"] == "task")
                .then(|| v["short_code"].as_str().expect("short_code").to_string())
        })
        .filter(|code| code != &warm[0].1)
        .collect();
    assert_eq!(event_codes, codes, "one item_created event per final code");

    // --- timings: one board, and 20 boards (no shared row) -------------------
    // The median of ROUNDS rounds of each. Each round on one board also
    // gets contiguous numbers.
    let mut one = Vec::new();
    let mut many = Vec::new();
    for _ in 0..ROUNDS {
        let (took, round) = create_at_once(&url, &vec![board; CREATES], alice);
        let mut numbers: Vec<i64> = round.iter().map(|(_, c)| number(c)).collect();
        numbers.sort();
        assert!(
            numbers.windows(2).all(|w| w[1] == w[0] + 1),
            "contiguous: {numbers:?}"
        );
        one.push(took);
        let (took, spread) = create_at_once(&url, &others, alice);
        assert_eq!(spread.len(), CREATES);
        many.push(took);
    }
    let ms = |mut v: Vec<Duration>| {
        v.sort();
        v[v.len() / 2].as_secs_f64() * 1000.0
    };
    eprintln!(
        "KAIROS-T-0633: {CREATES} creates at once, median of {} rounds: one board {:.1} ms, \
         {CREATES} boards {:.1} ms",
        ROUNDS,
        ms(one),
        ms(many),
    );
}
