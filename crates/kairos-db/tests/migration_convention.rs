//! KAIROS-T-0639 — the migration convention and the boot check.
//!
//! Proves:
//! 1. One query finds the stale tenant schemas. The boot migrates those
//!    and skips a current one.
//! 2. A schema without an older version is stale, also when it has the
//!    newest version (a migration that merges after a later one).
//! 3. A no-transaction migration with `CONCURRENTLY` runs at boot outside a
//!    transaction, and in provisioning inside its transaction (without the
//!    keyword). The index is valid in both.
//! 4. Provisioning refuses a no-transaction migration without its SQL, and
//!    leaves nothing.
//!
//! The no-transaction migration is a probe: the tenant tree plus one
//! migration of this test, with a version after each real one.
//!
//! Runs against the compose Postgres (`angreal services up`). Each test owns
//! a scratch database.

#[path = "common/scratch.rs"]
mod scratch;

use std::collections::BTreeSet;
use std::fmt;

use diesel::connection::BoxableConnection;
use diesel::migration::{
    Migration, MigrationMetadata, MigrationName, MigrationSource, MigrationVersion,
};
use diesel::pg::{Pg, PgConnection};
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Bool, Text};

use kairos_db::migrations::TENANT_MIGRATIONS;
use kairos_db::tenant::TenantError;
use kairos_db::{
    migrate_each_tenant_from, provision_tenant, provision_tenant_from, run_public_migrations,
    stale_tenants, with_migration_lock,
};

const PROBE_VERSION: &str = "29991231000000";
const PROBE_NAME: &str = "2999-12-31-000000_t0639_probe";
const PROBE_SQL: &str = "CREATE INDEX CONCURRENTLY t0639_probe ON notifications (created_at)";

/// The probe migration: one `CONCURRENTLY` statement, outside a
/// transaction.
struct Probe;

struct ProbeName;

impl fmt::Display for ProbeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PROBE_NAME)
    }
}

impl MigrationName for ProbeName {
    fn version(&self) -> MigrationVersion<'_> {
        MigrationVersion::from(PROBE_VERSION)
    }
}

struct NoTransaction;

impl MigrationMetadata for NoTransaction {
    fn run_in_transaction(&self) -> bool {
        false
    }
}

impl Migration<Pg> for Probe {
    fn run(&self, conn: &mut dyn BoxableConnection<Pg>) -> diesel::migration::Result<()> {
        conn.batch_execute(PROBE_SQL)?;
        Ok(())
    }

    fn revert(&self, conn: &mut dyn BoxableConnection<Pg>) -> diesel::migration::Result<()> {
        conn.batch_execute("DROP INDEX CONCURRENTLY IF EXISTS t0639_probe")?;
        Ok(())
    }

    fn metadata(&self) -> &dyn MigrationMetadata {
        &NoTransaction
    }

    fn name(&self) -> &dyn MigrationName {
        &ProbeName
    }
}

/// The tenant tree plus the probe.
struct WithProbe;

impl MigrationSource<Pg> for WithProbe {
    fn migrations(&self) -> diesel::migration::Result<Vec<Box<dyn Migration<Pg>>>> {
        let mut migrations = MigrationSource::<Pg>::migrations(&TENANT_MIGRATIONS)?;
        migrations.push(Box::new(Probe));
        Ok(migrations)
    }
}

const PROBE_LIST: &[(&str, &str)] = &[(PROBE_VERSION, PROBE_SQL)];

fn scratch_connection(base: &str) -> (scratch::ScratchGuard, PgConnection) {
    let admin_url = scratch::admin_database_url();
    let mut admin = PgConnection::establish(&admin_url).expect("admin connection");
    let name = scratch::scratch_db_name(base);
    scratch::drop_quietly(&mut admin, &name);
    sql_query(format!("CREATE DATABASE {name}"))
        .execute(&mut admin)
        .expect("create the scratch database");
    let guard = scratch::ScratchGuard::new(&name);
    let (root, _) = admin_url
        .rsplit_once('/')
        .expect("a database url has a path");
    let mut conn = PgConnection::establish(&format!("{root}/{name}")).expect("scratch connection");
    run_public_migrations(&mut conn).expect("public migrations");
    (guard, conn)
}

#[derive(QueryableByName)]
struct BoolRow {
    #[diesel(sql_type = Bool)]
    value: bool,
}

/// Does `schema` have the probe index, valid?
fn probe_index_is_valid(conn: &mut PgConnection, schema: &str) -> bool {
    sql_query(
        "SELECT EXISTS (SELECT 1 FROM pg_index i \
           JOIN pg_class c ON c.oid = i.indexrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = $1 AND c.relname = 't0639_probe' AND i.indisvalid) AS value",
    )
    .bind::<Text, _>(schema)
    .get_result::<BoolRow>(conn)
    .expect("probe index")
    .value
}

fn has_version(conn: &mut PgConnection, schema: &str, version: &str) -> bool {
    sql_query(format!(
        "SELECT EXISTS (SELECT 1 FROM \"{schema}\".__diesel_schema_migrations \
          WHERE version = $1) AS value"
    ))
    .bind::<Text, _>(version)
    .get_result::<BoolRow>(conn)
    .expect("bookkeeping row")
    .value
}

fn schema_exists(conn: &mut PgConnection, schema: &str) -> bool {
    sql_query("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1) AS value")
        .bind::<Text, _>(schema)
        .get_result::<BoolRow>(conn)
        .expect("schema")
        .value
}

fn set(slugs: &[&str]) -> BTreeSet<String> {
    slugs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_boot_skips_a_current_tenant_and_migrates_a_stale_one() {
    let (_guard, mut conn) = scratch_connection("kairos_t0639_boot_check");

    // "current" has the probe: provisioning ran it in its transaction.
    let report = provision_tenant_from(&mut conn, "current", "Current", &WithProbe, PROBE_LIST)
        .expect("provision with the probe");
    assert_eq!(
        report.migrations_applied.last().map(String::as_str),
        Some(PROBE_VERSION)
    );
    assert!(probe_index_is_valid(&mut conn, "org_current"));
    assert!(has_version(&mut conn, "org_current", PROBE_VERSION));

    // "stale" has the real tree only.
    provision_tenant(&mut conn, "stale", "Stale").expect("provision");

    assert_eq!(
        stale_tenants(&mut conn, &WithProbe).expect("stale check"),
        set(&["stale"])
    );
    assert_eq!(
        stale_tenants(&mut conn, &TENANT_MIGRATIONS).expect("stale check"),
        set(&[]),
        "a schema with a version that the tree does not know is current"
    );

    // The boot: under the lock, on a connection with no transaction.
    let reports = with_migration_lock(&mut conn, |conn| migrate_each_tenant_from(conn, &WithProbe))
        .expect("the boot migration");
    let results: Vec<(String, Result<Vec<String>, String>)> =
        reports.into_iter().map(|r| (r.slug, r.result)).collect();
    assert_eq!(
        results,
        vec![
            ("current".to_string(), Ok(vec![])),
            ("stale".to_string(), Ok(vec![PROBE_VERSION.to_string()])),
        ]
    );
    assert!(
        probe_index_is_valid(&mut conn, "org_stale"),
        "CONCURRENTLY ran outside a transaction"
    );
    assert_eq!(
        stale_tenants(&mut conn, &WithProbe).expect("stale check"),
        set(&[])
    );
}

#[test]
fn a_missing_older_version_is_stale_and_provisioning_needs_the_sql() {
    let (_guard, mut conn) = scratch_connection("kairos_t0639_out_of_order");
    provision_tenant(&mut conn, "ooo", "Out of order").expect("provision");
    assert_eq!(
        stale_tenants(&mut conn, &TENANT_MIGRATIONS).expect("stale check"),
        set(&[])
    );

    // Remove the row of an older migration. The newest stays.
    let versions: Vec<String> = MigrationSource::<Pg>::migrations(&TENANT_MIGRATIONS)
        .expect("tree")
        .iter()
        .map(|m| m.name().version().to_string())
        .collect();
    let newest = versions.iter().max().expect("a version").clone();
    let older = versions.iter().min().expect("a version").clone();
    sql_query("DELETE FROM org_ooo.__diesel_schema_migrations WHERE version = $1")
        .bind::<Text, _>(&older)
        .execute(&mut conn)
        .expect("forget an older version");
    assert!(has_version(&mut conn, "org_ooo", &newest));
    assert_eq!(
        stale_tenants(&mut conn, &TENANT_MIGRATIONS).expect("stale check"),
        set(&["ooo"]),
        "the newest version is there, but an older one is not"
    );

    // Provisioning with a no-transaction migration and no SQL for it.
    let refusal = provision_tenant_from(&mut conn, "nosql", "No SQL", &WithProbe, &[])
        .expect_err("no SQL for the probe");
    match &refusal {
        TenantError::Migration { slug, message } => {
            assert_eq!(slug, "nosql");
            assert!(message.contains(PROBE_NAME), "{message}");
        }
        other => panic!("{other}"),
    }
    assert!(!schema_exists(&mut conn, "org_nosql"), "nothing is left");
    let left: i64 =
        sql_query("SELECT count(*) AS count FROM public.organizations WHERE slug = 'nosql'")
            .get_result::<CountRow>(&mut conn)
            .expect("count")
            .count;
    assert_eq!(left, 0, "no organization row is left");

    // Without the provisioning path, CONCURRENTLY fails in a transaction.
    let err = conn
        .transaction::<(), DieselError, _>(|conn| {
            sql_query("SET LOCAL search_path TO org_ooo").execute(conn)?;
            sql_query(PROBE_SQL).execute(conn)?;
            Ok(())
        })
        .expect_err("CONCURRENTLY in a transaction");
    assert!(err.to_string().contains("transaction block"), "{err}");
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}
