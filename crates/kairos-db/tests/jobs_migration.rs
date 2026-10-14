//! KAIROS-T-0373 — the migration `jobs`: the job queue in the public schema.
//!
//! Proves: the table is public, with the tenant as a column; the CHECK
//! refuses an unknown state; the dedupe key holds only for an open job; the
//! up.sql runs again with no error; the down.sql removes the table, and the
//! up.sql makes it again.
//!
//! Runs against the LIVE compose Postgres (`angreal services up`); owns
//! the scratch database `kairos_jobs_migration_t0373_test`.

#[path = "common/scratch.rs"]
mod scratch;

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::BigInt;

use kairos_db::jobs::{self, NewJob};
use kairos_db::run_public_migrations;

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";
static SCRATCH_DB: scratch::ScratchName =
    scratch::ScratchName::new("kairos_jobs_migration_t0373_test");
const UP_SQL: &str = include_str!("../migrations/public/2026-10-11-000003_jobs/up.sql");
const DOWN_SQL: &str = include_str!("../migrations/public/2026-10-11-000003_jobs/down.sql");

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("database url has a path");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

fn table_count(conn: &mut PgConnection) -> i64 {
    sql_query(
        "SELECT count(*) AS count FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'jobs'",
    )
    .get_result::<CountRow>(conn)
    .expect("table count")
    .count
}

#[test]
fn the_jobs_migration_runs_again_and_reverts() {
    let admin_url = admin_database_url();
    let mut admin = PgConnection::establish(&admin_url).expect("admin connection");
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin)
        .unwrap();
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin)
        .unwrap();
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let mut conn = PgConnection::establish(&with_database(&admin_url, &SCRATCH_DB)).unwrap();
    run_public_migrations(&mut conn).expect("public migrations");
    assert_eq!(table_count(&mut conn), 1, "public.jobs exists");

    // The CHECK of the state.
    let err = conn
        .batch_execute("INSERT INTO jobs (kind, state) VALUES ('test', 'running')")
        .expect_err("an unknown state");
    assert!(err.to_string().contains("check"), "{err}");

    // The dedupe key holds for an open job only.
    let keyed = NewJob {
        dedupe_key: Some("k".into()),
        tenant: Some("acme".into()),
        ..NewJob::new("test")
    };
    let first = jobs::enqueue(&mut conn, &keyed).unwrap().expect("added");
    assert_eq!(first.state, "queued");
    assert_eq!(first.tries, 0);
    assert_eq!(first.max_tries, 3);
    assert!(jobs::enqueue(&mut conn, &keyed).unwrap().is_none());
    conn.batch_execute("UPDATE jobs SET state = 'failed', finished_at = now()")
        .unwrap();
    assert!(jobs::enqueue(&mut conn, &keyed).unwrap().is_some());

    // Re-runnable, then reverted, then made again.
    conn.batch_execute(UP_SQL).expect("up.sql again");
    assert_eq!(table_count(&mut conn), 1);
    conn.batch_execute(DOWN_SQL).expect("down.sql");
    assert_eq!(table_count(&mut conn), 0, "down.sql removes the table");
    conn.batch_execute(DOWN_SQL).expect("down.sql again");
    conn.batch_execute(UP_SQL).expect("up.sql after down.sql");
    assert_eq!(table_count(&mut conn), 1);

    drop(conn);
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin)
        .unwrap();
}
