//! KAIROS-T-0670 — the migration `issue_sync` and the sync state of the
//! issue import.
//!
//! Proves: the migration runs again with no error and changes no row (the
//! cadence is off for each repository; a link keeps its connection); the
//! CHECK on the interval refuses 59 seconds; a link may have a repository
//! and no connection, but not neither; `due` names only a repository with
//! the cadence on whose time is past; `claim` gives a due run to one caller
//! only; a run is not complete until `run_finished`; a run that stopped keeps
//! the time of the last complete run, and a complete run clears the error;
//! down.sql removes the new schema.
//!
//! Runs against the LIVE compose Postgres (`angreal services up`); owns the
//! scratch database `kairos_issue_sync_t0670_test`.

#[path = "common/scratch.rs"]
mod scratch;

use chrono::{Duration, Utc};
use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use uuid::Uuid;

use kairos_db::issue_sync::{self, RunEnd};
use kairos_db::{provision_tenant, run_public_migrations};

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";
static SCRATCH_DB: scratch::ScratchName = scratch::ScratchName::new("kairos_issue_sync_t0670_test");
const UP_SQL: &str = include_str!("../migrations/tenant/2026-10-19-000100_issue_sync/up.sql");
const DOWN_SQL: &str = include_str!("../migrations/tenant/2026-10-19-000100_issue_sync/down.sql");

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("database url has a path");
    format!("{base}/{}", scratch::scratch_db_name(db_name))
}

fn insert_repository(conn: &mut PgConnection, id: Uuid, slug: &str, team: Uuid) {
    sql_query(
        "INSERT INTO org_acme.repositories \
         (id, slug, forge, repo_full_name, repo_url, default_branch, team_id, \
          description, created_by, updated_by) \
         VALUES ($1, $2, 'github', 'acme/' || $2, 'https://github.com/acme/' || $2, \
                 'main', $3, '', $3, $3)",
    )
    .bind::<diesel::sql_types::Uuid, _>(id)
    .bind::<diesel::sql_types::Text, _>(slug)
    .bind::<diesel::sql_types::Uuid, _>(team)
    .execute(conn)
    .expect("repository");
}

fn set_interval(conn: &mut PgConnection, id: Uuid, secs: i32) {
    sql_query("UPDATE org_acme.repositories SET issue_import_interval_secs = $2 WHERE id = $1")
        .bind::<diesel::sql_types::Uuid, _>(id)
        .bind::<diesel::sql_types::Integer, _>(secs)
        .execute(conn)
        .expect("interval");
}

fn insert_link(
    conn: &mut PgConnection,
    connection: Option<Uuid>,
    repository: Option<Uuid>,
) -> QueryResult<usize> {
    sql_query(
        "INSERT INTO org_acme.item_links (item_id, connection_id, repository_id, kind, \
             external_id, title, url, state, forge_updated_at) \
         VALUES ($1, $2, $3, 'issue', '7', 't', 'u', 'open', now())",
    )
    .bind::<diesel::sql_types::Uuid, _>(Uuid::new_v4())
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(connection)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(repository)
    .execute(conn)
}

#[test]
fn the_sync_state_of_the_issue_import() {
    let admin_url = admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url)
        .expect("connecting to compose postgres (is the stack up? `angreal services up`)");
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch db");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch db");
    let _scratch_guard = scratch::ScratchGuard::new(&SCRATCH_DB);
    let mut conn = PgConnection::establish(&with_database(&admin_url, &SCRATCH_DB))
        .expect("connecting to scratch db");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .unwrap();

    // Given a connection and a link from before the migration
    conn.batch_execute(DOWN_SQL).expect("down.sql");
    let team = Uuid::new_v4();
    sql_query(
        "INSERT INTO org_acme.teams (id, name, slug, team_type) \
         VALUES ($1, 'Platform', 'platform', 'platform')",
    )
    .bind::<diesel::sql_types::Uuid, _>(team)
    .execute(&mut conn)
    .expect("team");
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    insert_repository(&mut conn, a, "alpha", team);
    insert_repository(&mut conn, b, "bravo", team);
    insert_repository(&mut conn, c, "charlie", team);
    let connection = Uuid::new_v4();
    sql_query(
        "INSERT INTO org_acme.forge_connections (id, forge, created_by, repository_id) \
         VALUES ($1, 'github', $2, $3)",
    )
    .bind::<diesel::sql_types::Uuid, _>(connection)
    .bind::<diesel::sql_types::Uuid, _>(team)
    .bind::<diesel::sql_types::Uuid, _>(a)
    .execute(&mut conn)
    .expect("connection");

    // When the migration runs, twice
    conn.batch_execute(UP_SQL).expect("up.sql");
    conn.batch_execute(UP_SQL).expect("up.sql again");

    // Then each repository has the cadence off
    for id in [a, b, c] {
        let row = kairos_db::repositories::load(&mut conn, id).expect("repository");
        assert_eq!(row.issue_import_interval_secs, None);
    }
    // And a link has a connection or a repository, not neither
    insert_link(&mut conn, Some(connection), None).expect("a link with a connection");
    insert_link(&mut conn, None, Some(a)).expect("a link with a repository");
    let neither = insert_link(&mut conn, None, None).expect_err("a link with neither");
    assert!(
        neither.to_string().contains("item_links_owner_check"),
        "{neither}"
    );
    // And the interval has a floor and a ceiling
    for bad in [59, 2_592_001] {
        let refused = sql_query(
            "UPDATE org_acme.repositories SET issue_import_interval_secs = $2 WHERE id = $1",
        )
        .bind::<diesel::sql_types::Uuid, _>(a)
        .bind::<diesel::sql_types::Integer, _>(bad)
        .execute(&mut conn)
        .expect_err("the CHECK refuses");
        assert!(
            refused
                .to_string()
                .contains("repositories_issue_import_interval_check"),
            "{refused}"
        );
    }

    // ---- due and claim -------------------------------------------------
    let now = Utc::now();
    assert!(
        issue_sync::due(&mut conn, now).unwrap().is_empty(),
        "all off"
    );
    set_interval(&mut conn, a, 60);
    set_interval(&mut conn, b, 3600);
    let due = issue_sync::due(&mut conn, now).unwrap();
    assert_eq!(
        due.iter().map(|d| d.slug.as_str()).collect::<Vec<_>>(),
        ["alpha", "bravo"],
        "no sync row: due now"
    );
    // Two callers, one run
    assert!(issue_sync::claim(&mut conn, a, 60, now).unwrap());
    assert!(!issue_sync::claim(&mut conn, a, 60, now).unwrap(), "taken");
    let due = issue_sync::due(&mut conn, now).unwrap();
    assert_eq!(due.len(), 1, "alpha is not due until its next run");
    assert_eq!(due[0].slug, "bravo");
    let later = now + Duration::seconds(61);
    let due = issue_sync::due(&mut conn, later).unwrap();
    assert_eq!(due.len(), 2, "alpha is due after its interval");
    assert!(issue_sync::claim(&mut conn, a, 60, later).unwrap());
    let sync = issue_sync::get(&mut conn, a).unwrap().expect("sync row");
    assert_eq!(
        sync.next_run_at.unwrap().timestamp(),
        (later + Duration::seconds(60)).timestamp()
    );
    // A new schedule makes it due now
    issue_sync::schedule(&mut conn, a, now).unwrap();
    assert!(
        issue_sync::due(&mut conn, later)
            .unwrap()
            .iter()
            .any(|d| d.slug == "alpha")
    );
    // A repository whose cadence is off again is not due
    sql_query("UPDATE org_acme.repositories SET issue_import_interval_secs = NULL WHERE id = $1")
        .bind::<diesel::sql_types::Uuid, _>(a)
        .execute(&mut conn)
        .unwrap();
    assert!(
        !issue_sync::due(&mut conn, later)
            .unwrap()
            .iter()
            .any(|d| d.slug == "alpha")
    );

    // ---- the state of a run ----------------------------------------------
    assert!(issue_sync::get(&mut conn, c).unwrap().is_none());
    let t1 = now;
    issue_sync::run_started(&mut conn, c, t1).unwrap();
    let sync = issue_sync::get(&mut conn, c).unwrap().unwrap();
    assert!(
        !sync.last_run_complete,
        "a run that has not ended is not complete"
    );
    assert_eq!(sync.last_complete_at, None);
    // It stops half way
    let t2 = t1 + Duration::seconds(5);
    issue_sync::run_finished(
        &mut conn,
        c,
        t2,
        &RunEnd {
            complete: false,
            listed: 100,
            imported: 97,
            error: Some("rate limit"),
        },
    )
    .unwrap();
    let sync = issue_sync::get(&mut conn, c).unwrap().unwrap();
    assert!(!sync.last_run_complete);
    assert_eq!((sync.last_run_listed, sync.last_run_imported), (100, 97));
    assert_eq!(sync.last_error.as_deref(), Some("rate limit"));
    assert_eq!(sync.last_complete_at, None);
    // The next run completes
    let t3 = t2 + Duration::seconds(60);
    issue_sync::run_started(&mut conn, c, t3).unwrap();
    issue_sync::run_finished(
        &mut conn,
        c,
        t3 + Duration::seconds(1),
        &RunEnd {
            complete: true,
            listed: 103,
            imported: 3,
            error: None,
        },
    )
    .unwrap();
    let sync = issue_sync::get(&mut conn, c).unwrap().unwrap();
    assert!(sync.last_run_complete);
    assert_eq!(sync.last_error, None);
    assert_eq!(sync.last_error_at, None);
    assert_eq!(
        sync.last_complete_at.unwrap().timestamp(),
        (t3 + Duration::seconds(1)).timestamp()
    );
    // A later run that fails keeps the time of the last complete run
    let t4 = t3 + Duration::seconds(120);
    issue_sync::run_finished(
        &mut conn,
        c,
        t4,
        &RunEnd {
            complete: false,
            listed: 0,
            imported: 0,
            error: Some("refused"),
        },
    )
    .unwrap();
    let sync = issue_sync::get(&mut conn, c).unwrap().unwrap();
    assert_eq!(
        sync.last_complete_at.unwrap().timestamp(),
        (t3 + Duration::seconds(1)).timestamp()
    );
    assert_eq!(sync.last_error.as_deref(), Some("refused"));
    assert_eq!(issue_sync::record_count(&mut conn, c).unwrap(), 0);

    // ---- down.sql ----------------------------------------------------------
    conn.batch_execute(DOWN_SQL).expect("down.sql");
    let gone = sql_query("SELECT 1 FROM org_acme.forge_issue_sync")
        .execute(&mut conn)
        .expect_err("the table is gone");
    assert!(gone.to_string().contains("forge_issue_sync"), "{gone}");
    let links: i64 = {
        #[derive(QueryableByName)]
        struct N {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        sql_query("SELECT count(*) AS n FROM org_acme.item_links")
            .get_result::<N>(&mut conn)
            .unwrap()
            .n
    };
    assert_eq!(links, 1, "the link with a connection stays, the other goes");
    conn.batch_execute(UP_SQL).expect("up.sql after down.sql");
}
