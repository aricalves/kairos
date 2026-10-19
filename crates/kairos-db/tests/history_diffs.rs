//! KAIROS-T-0651: `item_history` keeps a diff, with a full row at most
//! every 10 versions (amends KAIROS-A-0004).
//!
//! Runs against the real compose Postgres (`angreal services up`), in a
//! scratch database of its own. Covered:
//! - an item with 25 versions: the full rows are v1, v11 and v21; each
//!   other row is a diff from the newest full row before it; each version
//!   gives its exact title and text back; the storage is smaller
//! - a rollback to an old diff version writes that text as a new version
//! - the full rows from before the change stay readable, and the next
//!   version is a diff from the newest of them
//! - a diff row without its base is an error, not a wrong text

#[path = "common/scratch.rs"]
mod scratch;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::BigInt;
use uuid::Uuid;

use kairos_core::short_code::ItemType;
use kairos_db::history::{self, HistoryError};
use kairos_db::items::{self, ContentUpdate, CreateStrategy, ItemError};
use kairos_db::models::{NewUser, User};
use kairos_db::{provision_tenant, run_public_migrations, schema};

/// Same default as `.angreal/task_db.py`'s `DATABASE_URL`.
const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";

static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_history_diffs_test");

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL must contain a database path segment");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

fn insert_user(conn: &mut PgConnection, external_id: &str, email: &str, name: &str) -> Uuid {
    diesel::insert_into(schema::users::table)
        .values(NewUser {
            external_id: external_id.into(),
            user_name: external_id.into(),
            email: email.into(),
            display_name: name.into(),
        })
        .returning(User::as_returning())
        .get_result(conn)
        .expect("inserting user")
        .id
}

fn board_id_by_slug(conn: &mut PgConnection, slug: &str) -> Uuid {
    schema::boards::table
        .filter(schema::boards::slug.eq(slug))
        .select(schema::boards::id)
        .first(conn)
        .unwrap_or_else(|e| panic!("board {slug:?} not found: {e}"))
}

/// The text of version `v` of the fixture document: 60 lines, one line
/// changes in each version, and each version adds a line.
fn text_of(v: i32) -> String {
    let mut lines: Vec<String> = (0..60)
        .map(|i| format!("Line {i}: the plan of the item, with some words to read.\n"))
        .collect();
    for u in 2..=v {
        let at = ((u * 7) % 60) as usize;
        lines[at] = format!("Line {at}: version {u} changed this line.\n");
        lines.push(format!("Step {u}: one more line for the record.\n"));
    }
    format!("# Plan\n\n{}", lines.concat())
}

fn title_of(v: i32) -> String {
    if v % 4 == 0 {
        format!("Plan, title of v{v}")
    } else {
        "Plan".to_string()
    }
}

/// `(version, base_version)` of each row of the item, in version order.
fn forms(conn: &mut PgConnection, item: Uuid) -> Vec<(i32, Option<i32>)> {
    schema::item_history::table
        .filter(schema::item_history::item_id.eq(item))
        .order(schema::item_history::version.asc())
        .select((
            schema::item_history::version,
            schema::item_history::base_version,
        ))
        .load(conn)
        .expect("loading the history forms")
}

#[derive(QueryableByName)]
struct Bytes {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// The bytes that the history of the item stores for its texts.
fn stored_bytes(conn: &mut PgConnection, item: Uuid) -> i64 {
    sql_query(
        "SELECT coalesce(sum(octet_length(content) + coalesce(octet_length(content_diff), 0)), 0)::bigint AS n \
           FROM item_history WHERE item_id = $1",
    )
    .bind::<diesel::sql_types::Uuid, _>(item)
    .get_result::<Bytes>(conn)
    .expect("measuring the history")
    .n
}

fn edit(conn: &mut PgConnection, item: Uuid, expected: i32, title: &str, text: &str, actor: Uuid) {
    items::update_item_content(
        conn,
        ItemType::Strategy,
        item,
        ContentUpdate {
            new_title: Some(title),
            new_content: text,
            expected_version: expected,
        },
        actor,
    )
    .unwrap_or_else(|e| panic!("editing to v{}: {e}", expected + 1));
}

fn create(conn: &mut PgConnection, board: Uuid, actor: Uuid) -> Uuid {
    items::create_strategy(
        conn,
        CreateStrategy {
            board_id: board,
            column_id: None,
            title: &title_of(1),
            content: &text_of(1),
            hypothesis: None,
        },
        actor,
    )
    .expect("creating the strategy")
    .id
}

fn assert_each_version(conn: &mut PgConnection, item: Uuid, last: i32) {
    for v in 1..=last {
        let row = history::version_row(conn, item, v)
            .unwrap_or_else(|e| panic!("reading v{v}: {e}"))
            .unwrap_or_else(|| panic!("v{v} is in the history"));
        assert_eq!(row.version, v);
        assert_eq!(row.title, title_of(v), "the title of v{v}");
        assert_eq!(row.content, text_of(v), "the text of v{v}");
    }
}

#[test]
fn history_keeps_diffs_with_a_full_row_every_ten_versions() {
    let admin_url = admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url).unwrap_or_else(|e| {
        panic!("cannot connect to compose postgres at {admin_url}: {e} (`angreal services up`)")
    });
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch database");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch database");
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let url = with_database(&admin_url, &SCRATCH_DB);
    let mut conn = PgConnection::establish(&url).expect("connecting to scratch database");
    run_public_migrations(&mut conn).expect("running public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .expect("pinning search_path");
    let robin = insert_user(&mut conn, "dex|robin", "robin@acme.test", "Robin");
    let board = board_id_by_slug(&mut conn, "strategy");

    // ---- 25 versions: full rows at v1, v11, v21; each text comes back ------
    let item = create(&mut conn, board, robin);
    for v in 2..=25 {
        edit(&mut conn, item, v - 1, &title_of(v), &text_of(v), robin);
    }
    let expected_forms: Vec<(i32, Option<i32>)> = (1..=25)
        .map(|v| {
            let base = (v - 1) / 10 * 10 + 1;
            (v, (base != v).then_some(base))
        })
        .collect();
    assert_eq!(forms(&mut conn, item), expected_forms);
    assert_each_version(&mut conn, item, 25);

    let full_bytes: i64 = (1..=25).map(|v| text_of(v).len() as i64).sum();
    let stored = stored_bytes(&mut conn, item);
    eprintln!("history of 25 versions: {full_bytes} bytes as full rows, {stored} bytes stored");
    assert!(
        stored * 3 < full_bytes,
        "the history stores less than a third: {stored} of {full_bytes} bytes"
    );

    // ---- a rollback to an old diff version ---------------------------------
    let new_version = items::rollback_item(&mut conn, ItemType::Strategy, item, 3, robin)
        .expect("rolling back to v3");
    assert_eq!(new_version, 26);
    let live: (String, String) = schema::strategies::table
        .filter(schema::strategies::id.eq(item))
        .select((schema::strategies::title, schema::strategies::content))
        .first(&mut conn)
        .expect("reading the strategy");
    assert_eq!(live, (title_of(3), text_of(3)));
    let v26 = history::version_row(&mut conn, item, 26)
        .expect("reading v26")
        .expect("v26");
    assert_eq!((v26.title, v26.content), (title_of(3), text_of(3)));
    assert_eq!(v26.base_version, Some(21), "v26 is a diff from v21");
    let missing = items::rollback_item(&mut conn, ItemType::Strategy, item, 99, robin);
    assert!(
        matches!(missing, Err(ItemError::HistoryNotFound { version: 99, .. })),
        "{missing:?}"
    );

    // ---- full rows from before the change stay readable --------------------
    let old = create(&mut conn, board, robin);
    for v in 2..=12 {
        edit(&mut conn, old, v - 1, &title_of(v), &text_of(v), robin);
    }
    // As before KAIROS-T-0651: each row full.
    for v in 1..=12 {
        diesel::update(
            schema::item_history::table
                .filter(schema::item_history::item_id.eq(old))
                .filter(schema::item_history::version.eq(v)),
        )
        .set((
            schema::item_history::content.eq(text_of(v)),
            schema::item_history::base_version.eq(None::<i32>),
            schema::item_history::content_diff.eq(None::<String>),
        ))
        .execute(&mut conn)
        .expect("making the row full");
    }
    assert_each_version(&mut conn, old, 12);
    edit(&mut conn, old, 12, &title_of(13), &text_of(13), robin);
    assert_eq!(
        forms(&mut conn, old).last(),
        Some(&(13, Some(12))),
        "the next version is a diff from the newest full row"
    );
    assert_each_version(&mut conn, old, 13);

    // ---- a diff row without its base is an error ---------------------------
    let broken = create(&mut conn, board, robin);
    edit(&mut conn, broken, 1, &title_of(2), &text_of(2), robin);
    assert_eq!(forms(&mut conn, broken), vec![(1, None), (2, Some(1))]);
    diesel::delete(
        schema::item_history::table
            .filter(schema::item_history::item_id.eq(broken))
            .filter(schema::item_history::version.eq(1)),
    )
    .execute(&mut conn)
    .expect("deleting the base row");
    let read = history::version_row(&mut conn, broken, 2);
    assert!(
        matches!(read, Err(HistoryError::Corrupt { version: 2, .. })),
        "{read:?}"
    );
    let rollback = items::rollback_item(&mut conn, ItemType::Strategy, broken, 2, robin);
    assert!(
        matches!(rollback, Err(ItemError::HistoryCorrupt { version: 2, .. })),
        "{rollback:?}"
    );
    // The next edit has no full row to diff from: it writes a full row.
    edit(&mut conn, broken, 2, &title_of(3), &text_of(3), robin);
    assert_eq!(forms(&mut conn, broken), vec![(2, Some(1)), (3, None)]);
}
