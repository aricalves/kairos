//! KAIROS-T-0377 — the migration `auth_failures`: the counts of the login
//! throttle in the public schema.
//!
//! Proves: the table is public; the CHECK refuses an unknown kind; a failure
//! is one row for each subject, and a second failure updates it; the up.sql
//! runs again with no error; the down.sql removes the table, and the up.sql
//! makes it again.
//!
//! Runs against the LIVE compose Postgres (`angreal services up`); owns
//! the scratch database `kairos_auth_failures_migration_t0377_test`.

#[path = "common/scratch.rs"]
mod scratch;

use chrono::{DateTime, Utc};
use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::BigInt;

use kairos_db::auth_failures::{self, Rules, SubjectKey};
use kairos_db::run_public_migrations;

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";
static SCRATCH_DB: scratch::ScratchName =
    scratch::ScratchName::new("kairos_auth_failures_migration_t0377_test");
const UP_SQL: &str = include_str!("../migrations/public/2026-10-11-000004_auth_failures/up.sql");
const DOWN_SQL: &str =
    include_str!("../migrations/public/2026-10-11-000004_auth_failures/down.sql");

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
         WHERE table_schema = 'public' AND table_name = 'auth_failures'",
    )
    .get_result::<CountRow>(conn)
    .expect("table count")
    .count
}

#[test]
fn the_auth_failures_migration_runs_again_and_reverts() {
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
    assert_eq!(table_count(&mut conn), 1, "public.auth_failures exists");

    // The CHECK of the kind.
    let err = conn
        .batch_execute(
            "INSERT INTO auth_failures (kind, subject, window_started, expires_at) \
             VALUES ('tenant', 'x', now(), now())",
        )
        .expect_err("an unknown kind");
    assert!(err.to_string().contains("check"), "{err}");

    // One row for each subject; the second failure updates it.
    let now: DateTime<Utc> = "2026-10-11T12:00:00Z".parse().unwrap();
    let rules = Rules {
        max_failures: 5,
        window_secs: 300.0,
        lockout_secs: 60.0,
    };
    let subjects = [
        SubjectKey {
            kind: "identity".into(),
            subject: "a".into(),
        },
        SubjectKey {
            kind: "source".into(),
            subject: "b".into(),
        },
    ];
    assert!(!auth_failures::record_failure(&mut conn, &subjects, now, rules).unwrap());
    assert!(!auth_failures::record_failure(&mut conn, &subjects, now, rules).unwrap());
    assert_eq!(auth_failures::count(&mut conn).unwrap(), 2);
    let status = auth_failures::status(&mut conn, &subjects, now).unwrap();
    assert!(status.known && status.locked_until.is_none());

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
