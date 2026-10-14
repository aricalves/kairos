//! KAIROS-T-0430 — the migration `repository_branch_source`: each
//! repository has `default_branch_source` (`forge` or `hand`) and
//! `forge_branch`.
//!
//! Proves: a row that exists gets `forge` and no forge branch; a new row
//! that does not name the column gets `hand`; the CHECK refuses another
//! value; the up.sql runs again with no error; `record_forge_branch`
//! changes `default_branch` only on a row that follows the forge.
//!
//! Runs against the LIVE compose Postgres (`angreal services up`); owns
//! the scratch database `kairos_repository_branch_source_t0430_test`.

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use uuid::Uuid;

use kairos_db::{provision_tenant, run_public_migrations};

const DEFAULT_DATABASE_URL: &str = "postgres://kairos:kairos@localhost:41432/kairos";
const SCRATCH_DB: &str = "kairos_repository_branch_source_t0430_test";
const UP_SQL: &str =
    include_str!("../migrations/tenant/2026-10-14-000100_repository_branch_source/up.sql");
const DOWN_SQL: &str =
    include_str!("../migrations/tenant/2026-10-14-000100_repository_branch_source/down.sql");

fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn with_database(url: &str, db_name: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("database url has a path");
    format!("{base}/{db_name}")
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

#[test]
fn each_repository_has_the_source_of_its_default_branch() {
    let admin_url = admin_database_url();
    let mut admin_conn = PgConnection::establish(&admin_url)
        .expect("connecting to compose postgres (is the stack up? `angreal services up`)");
    sql_query(format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"))
        .execute(&mut admin_conn)
        .expect("dropping scratch db");
    sql_query(format!("CREATE DATABASE {SCRATCH_DB}"))
        .execute(&mut admin_conn)
        .expect("creating scratch db");
    let mut conn = PgConnection::establish(&with_database(&admin_url, SCRATCH_DB))
        .expect("connecting to scratch db");
    run_public_migrations(&mut conn).expect("public migrations");
    provision_tenant(&mut conn, "acme", "Acme Inc").expect("provisioning acme");
    sql_query("SET search_path TO org_acme, public")
        .execute(&mut conn)
        .unwrap();

    // Given a repository made before the migration
    conn.batch_execute(DOWN_SQL).expect("down.sql");
    let team = Uuid::new_v4();
    sql_query(
        "INSERT INTO org_acme.teams (id, name, slug, team_type) \
         VALUES ($1, 'Platform', 'platform', 'platform')",
    )
    .bind::<diesel::sql_types::Uuid, _>(team)
    .execute(&mut conn)
    .expect("team");
    let old = Uuid::new_v4();
    insert_repository(&mut conn, old, "site", team);

    // When the migration runs (2 times: it is re-runnable)
    conn.batch_execute(UP_SQL).expect("up.sql");
    conn.batch_execute(UP_SQL).expect("up.sql again");

    // Then the repository follows the forge, with no forge branch yet
    let row = kairos_db::repositories::load(&mut conn, old).expect("site");
    assert_eq!(row.default_branch_source, "forge");
    assert_eq!(row.forge_branch, None);
    assert_eq!(row.default_branch, "main");

    // And a new row that does not name the column has its branch by hand
    let new = Uuid::new_v4();
    insert_repository(&mut conn, new, "ledger", team);
    let row = kairos_db::repositories::load(&mut conn, new).expect("ledger");
    assert_eq!(row.default_branch_source, "hand");

    // And another value is refused
    let refused = sql_query(
        "UPDATE org_acme.repositories SET default_branch_source = 'remote' WHERE id = $1",
    )
    .bind::<diesel::sql_types::Uuid, _>(new)
    .execute(&mut conn)
    .expect_err("the CHECK refuses remote");
    assert!(
        refused
            .to_string()
            .contains("repositories_default_branch_source_check"),
        "{refused}"
    );

    // When the builder records the default branch of the forge
    let followed =
        kairos_db::repositories::record_forge_branch(&mut conn, old, "development").expect("site");
    let pinned = kairos_db::repositories::record_forge_branch(&mut conn, new, "development")
        .expect("ledger");

    // Then the row that follows the forge indexes it, and the branch set by
    // hand stays
    assert_eq!(followed.default_branch, "development");
    assert_eq!(followed.forge_branch.as_deref(), Some("development"));
    assert_eq!(pinned.default_branch, "main");
    assert_eq!(pinned.forge_branch.as_deref(), Some("development"));

    // And a branch that git does not accept is refused
    kairos_db::repositories::record_forge_branch(&mut conn, old, "-bad")
        .expect_err("a branch with a hyphen at the start");
}
