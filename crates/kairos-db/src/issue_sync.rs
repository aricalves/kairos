//! The sync state of the issue import of a repository (KAIROS-T-0670,
//! COLLIERY-I-0623).
//!
//! A repository takes its open issues on a cadence
//! (`repositories.issue_import_interval_secs`). Each run is a sync: it lists
//! the open issues and compares them with the record of taken issues
//! (`forge_issue_intake`, keyed by the id of the issue on the forge). No
//! time cursor decides what is new.
//!
//! `forge_issue_sync` keeps one row for each repository: when the next run
//! is due ([`due`], [`claim`]), and what the last run did ([`run_started`],
//! [`run_finished`]). A run that started and did not end shows as not
//! complete.
//!
//! The connection must be pinned to a tenant schema. This module does not
//! check rights: the caller does.

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Bool, Integer, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use uuid::Uuid;

/// The sync state of one repository (`forge_issue_sync`).
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct IssueSync {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    /// When the next cadence run is due; `None`: now.
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub next_run_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub last_run_started_at: Option<DateTime<Utc>>,
    /// The end of the last run, complete or not.
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub last_run_at: Option<DateTime<Utc>>,
    /// The last run listed every page.
    #[diesel(sql_type = Bool)]
    pub last_run_complete: bool,
    /// The end of the last complete run.
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub last_complete_at: Option<DateTime<Utc>>,
    /// Open issues that the last run listed.
    #[diesel(sql_type = Integer)]
    pub last_run_listed: i32,
    /// Tasks that the last run made.
    #[diesel(sql_type = Integer)]
    pub last_run_imported: i32,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_error: Option<String>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub last_error_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "repository_id, next_run_at, last_run_started_at, last_run_at, \
                       last_run_complete, last_complete_at, last_run_listed, \
                       last_run_imported, last_error, last_error_at";

/// The sync state of a repository, or `None` when it never had a schedule
/// or a run.
pub fn get(conn: &mut PgConnection, repository_id: Uuid) -> QueryResult<Option<IssueSync>> {
    sql_query(format!(
        "SELECT {COLUMNS} FROM forge_issue_sync WHERE repository_id = $1"
    ))
    .bind::<SqlUuid, _>(repository_id)
    .get_result(conn)
    .optional()
}

/// The sync state of many repositories.
pub fn get_many(conn: &mut PgConnection, ids: &[Uuid]) -> QueryResult<Vec<IssueSync>> {
    sql_query(format!(
        "SELECT {COLUMNS} FROM forge_issue_sync WHERE repository_id = ANY($1)"
    ))
    .bind::<diesel::sql_types::Array<SqlUuid>, _>(ids)
    .load(conn)
}

/// Make the next cadence run due at `at` (the repository got a new
/// interval, or a claim failed to queue its job).
pub fn schedule(
    conn: &mut PgConnection,
    repository_id: Uuid,
    at: DateTime<Utc>,
) -> QueryResult<()> {
    sql_query(
        "INSERT INTO forge_issue_sync (repository_id, next_run_at) VALUES ($1, $2) \
         ON CONFLICT (repository_id) DO UPDATE SET next_run_at = EXCLUDED.next_run_at, \
             updated_at = now()",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Timestamptz, _>(at)
    .execute(conn)?;
    Ok(())
}

/// A repository whose cadence run is due.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct Due {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    #[diesel(sql_type = Text)]
    pub slug: String,
    #[diesel(sql_type = Integer)]
    pub interval_secs: i32,
}

/// The live repositories with the cadence on whose next run is due at
/// `now`: no sync row yet, or `next_run_at` is empty or past.
pub fn due(conn: &mut PgConnection, now: DateTime<Utc>) -> QueryResult<Vec<Due>> {
    sql_query(
        "SELECT r.id AS repository_id, r.slug, r.issue_import_interval_secs AS interval_secs \
           FROM repositories r \
           LEFT JOIN forge_issue_sync s ON s.repository_id = r.id \
          WHERE r.deleted_at IS NULL AND r.issue_import_interval_secs IS NOT NULL \
            AND (s.next_run_at IS NULL OR s.next_run_at <= $1) \
          ORDER BY r.slug",
    )
    .bind::<Timestamptz, _>(now)
    .load(conn)
}

/// Take the due run of a repository: the next run is due `interval_secs`
/// after `now`. True: this caller took the run. False: another server took
/// it first, or it is not due. Two servers never both get true for one due
/// time.
pub fn claim(
    conn: &mut PgConnection,
    repository_id: Uuid,
    interval_secs: i32,
    now: DateTime<Utc>,
) -> QueryResult<bool> {
    let next = now + chrono::Duration::seconds(i64::from(interval_secs));
    let taken = sql_query(
        "INSERT INTO forge_issue_sync (repository_id, next_run_at) VALUES ($1, $2) \
         ON CONFLICT (repository_id) DO UPDATE SET next_run_at = EXCLUDED.next_run_at, \
             updated_at = now() \
          WHERE forge_issue_sync.next_run_at IS NULL OR forge_issue_sync.next_run_at <= $3",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Timestamptz, _>(next)
    .bind::<Timestamptz, _>(now)
    .execute(conn)?;
    Ok(taken > 0)
}

/// A run starts: it is not complete until [`run_finished`].
pub fn run_started(
    conn: &mut PgConnection,
    repository_id: Uuid,
    now: DateTime<Utc>,
) -> QueryResult<()> {
    sql_query(
        "INSERT INTO forge_issue_sync (repository_id, last_run_started_at, last_run_complete) \
         VALUES ($1, $2, false) \
         ON CONFLICT (repository_id) DO UPDATE SET last_run_started_at = EXCLUDED.last_run_started_at, \
             last_run_complete = false, updated_at = now()",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Timestamptz, _>(now)
    .execute(conn)?;
    Ok(())
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd<'a> {
    /// The run listed every page.
    pub complete: bool,
    /// Open issues that the run listed so far.
    pub listed: i32,
    /// Tasks that the run made so far.
    pub imported: i32,
    /// Why the run stopped. `None` with `complete`: no error.
    pub error: Option<&'a str>,
}

/// A run ends, complete or not. A complete run clears the error; a run that
/// stopped keeps the time of the last complete run.
pub fn run_finished(
    conn: &mut PgConnection,
    repository_id: Uuid,
    now: DateTime<Utc>,
    end: &RunEnd<'_>,
) -> QueryResult<()> {
    sql_query(
        "INSERT INTO forge_issue_sync (repository_id, last_run_at, last_run_complete, \
             last_complete_at, last_run_listed, last_run_imported, last_error, last_error_at) \
         VALUES ($1, $2::timestamptz, $3::boolean, CASE WHEN $3::boolean THEN $2::timestamptz END, \
                 $4, $5, $6::text, CASE WHEN $6::text IS NOT NULL THEN $2::timestamptz END) \
         ON CONFLICT (repository_id) DO UPDATE SET last_run_at = EXCLUDED.last_run_at, \
             last_run_complete = EXCLUDED.last_run_complete, \
             last_complete_at = COALESCE(EXCLUDED.last_complete_at, forge_issue_sync.last_complete_at), \
             last_run_listed = EXCLUDED.last_run_listed, \
             last_run_imported = EXCLUDED.last_run_imported, \
             last_error = EXCLUDED.last_error, last_error_at = COALESCE(EXCLUDED.last_error_at, \
                 CASE WHEN EXCLUDED.last_error IS NULL THEN NULL ELSE forge_issue_sync.last_error_at END), \
             updated_at = now()",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Timestamptz, _>(now)
    .bind::<Bool, _>(end.complete)
    .bind::<Integer, _>(end.listed)
    .bind::<Integer, _>(end.imported)
    .bind::<Nullable<Text>, _>(end.error)
    .execute(conn)?;
    Ok(())
}

/// The count of the record: the issues that Kairos took for a repository.
pub fn record_count(conn: &mut PgConnection, repository_id: Uuid) -> Result<i64, DieselError> {
    use crate::schema::forge_issue_intake::dsl;
    dsl::forge_issue_intake
        .filter(dsl::repository_id.eq(repository_id))
        .count()
        .get_result(conn)
}

/// The count of the record of many repositories. A repository with no row
/// in the record is not in the map.
pub fn record_counts(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, i64>, DieselError> {
    use crate::schema::forge_issue_intake::dsl;
    let rows: Vec<(Uuid, i64)> = dsl::forge_issue_intake
        .filter(dsl::repository_id.eq_any(ids))
        .group_by(dsl::repository_id)
        .select((dsl::repository_id, diesel::dsl::count_star()))
        .load(conn)?;
    Ok(rows.into_iter().collect())
}
