//! The breaker of the code index builder of each repository (KAIROS-T-0399,
//! COLLIERY-I-0620, GitHub #34).
//!
//! A build job whose lease ran out (its server stopped, ran out of memory,
//! or stalled) is not run again: [`trip`] adds a row for its repository.
//! While the row is there, the builder adds no build job for the
//! repository, and a build job that waits does not build. A build that a
//! person asks for removes the row ([`reset`]). No row: the breaker is not
//! tripped.
//!
//! The connection must be pinned to a tenant schema. This module does not
//! check rights: the caller does.

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use crate::schema::code_index_breakers;

/// The tripped breaker of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable, Insertable)]
#[diesel(table_name = code_index_breakers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Breaker {
    pub repository_id: Uuid,
    /// When the breaker tripped.
    pub tripped_at: DateTime<Utc>,
    /// Why: the error of the job that tripped it.
    pub reason: String,
    /// The job that tripped it. The job queue removes old jobs.
    pub job_id: Option<Uuid>,
}

/// Trip the breaker of `repository` with `reason`. A breaker that is
/// tripped already gets the new time, reason and job.
pub fn trip(
    conn: &mut PgConnection,
    repository: Uuid,
    reason: &str,
    job: Option<Uuid>,
) -> QueryResult<Breaker> {
    let row = Breaker {
        repository_id: repository,
        tripped_at: Utc::now(),
        reason: reason.to_string(),
        job_id: job,
    };
    diesel::insert_into(code_index_breakers::table)
        .values(&row)
        .on_conflict(code_index_breakers::repository_id)
        .do_update()
        .set((
            code_index_breakers::tripped_at.eq(row.tripped_at),
            code_index_breakers::reason.eq(&row.reason),
            code_index_breakers::job_id.eq(row.job_id),
        ))
        .returning(Breaker::as_returning())
        .get_result(conn)
}

/// The breaker of `repository`, when it is tripped.
pub fn get(conn: &mut PgConnection, repository: Uuid) -> QueryResult<Option<Breaker>> {
    code_index_breakers::table
        .find(repository)
        .select(Breaker::as_select())
        .first(conn)
        .optional()
}

/// The tripped breakers of `repositories`.
pub fn of(conn: &mut PgConnection, repositories: &[Uuid]) -> QueryResult<Vec<Breaker>> {
    code_index_breakers::table
        .filter(code_index_breakers::repository_id.eq_any(repositories))
        .select(Breaker::as_select())
        .load(conn)
}

/// Reset the breaker of `repository`. True: it was tripped.
pub fn reset(conn: &mut PgConnection, repository: Uuid) -> QueryResult<bool> {
    let n = diesel::delete(code_index_breakers::table.find(repository)).execute(conn)?;
    Ok(n == 1)
}
