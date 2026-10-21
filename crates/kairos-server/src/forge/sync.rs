//! The cadence of the issue import (KAIROS-T-0670, COLLIERY-I-0623).
//!
//! A repository with `issue_import_interval_secs` takes its open issues on
//! that interval, with no webhook. This is a periodic pass
//! ([`crate::periodic::ISSUE_SYNC`]): one run is open at a time, whatever
//! the number of servers. Each run looks at the repositories of each
//! organization, and queues the job `issue_import`
//! ([`crate::forge::import`]) for each one whose next run is due.
//!
//! - [`kairos_db::issue_sync::claim`] gives a due run to one caller only,
//!   and sets the next run one interval later. A run that was missed (the
//!   server was down) is due at once, and makes one job, not one for each
//!   missed interval.
//! - The job has the dedupe key of the import of the repository
//!   ([`crate::forge::import::job_key`]): a repository never has two
//!   imports that wait or run, also when a person started one by hand.
//!   Then the job is not added, and the next run is one interval later.
//! - The pass is one row of the job queue that the worker queues again
//!   after each look ([`crate::jobs::JobHandle::rearm_in`]), so the table
//!   of jobs does not grow with the looks.
//! - The job is a sync: it compares the whole list of open issues with the
//!   record of taken issues. This pass decides only WHEN to run.

use std::time::Duration;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;

use crate::forge::import::{self, Trigger};
use crate::jobs::{JobError, JobFuture, JobHandle};
use crate::periodic::PassFn;

/// How often the pass looks for a repository that is due. The shortest
/// interval of a repository is 60 seconds.
pub const TICK: Duration = Duration::from_secs(30);

/// What one look at a tenant did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queued {
    /// The slugs of the repositories that got a new job.
    pub queued: Vec<String>,
    /// The slugs of the repositories that were due, with an import that
    /// waits or runs already.
    pub busy: Vec<String>,
}

/// Queue the import of each repository of the tenant that is due at `now`.
/// The connection is pinned to the schema of the tenant. `max_tries` is
/// the tries of a job.
pub fn queue_due(
    conn: &mut PgConnection,
    tenant: &str,
    now: DateTime<Utc>,
    max_tries: i32,
) -> QueryResult<Queued> {
    let mut out = Queued::default();
    for due in kairos_db::issue_sync::due(conn, now)? {
        if !kairos_db::issue_sync::claim(conn, due.repository_id, due.interval_secs, now)? {
            continue;
        }
        let repository = match kairos_db::repositories::load(conn, due.repository_id) {
            Ok(repository) => repository,
            Err(e) => {
                tracing::warn!(tenant, repository = %due.slug, error = %e, "issue sync: the repository was not read");
                continue;
            }
        };
        let job = import::new_job(tenant, &repository, max_tries, Trigger::Cadence);
        match kairos_db::jobs::enqueue(conn, &job) {
            Ok(Some(_)) => out.queued.push(due.slug),
            Ok(None) => out.busy.push(due.slug),
            Err(e) => {
                // The run is due again at the next look.
                kairos_db::issue_sync::schedule(conn, due.repository_id, now)?;
                tracing::warn!(tenant, repository = %due.slug, error = %e, "issue sync: the job was not queued");
            }
        }
    }
    Ok(out)
}

/// The pass: on a connection of its own, look at each organization.
pub fn pass(database_url: String, max_tries: i32) -> PassFn {
    std::sync::Arc::new(move |handle: JobHandle| -> JobFuture {
        let url = database_url.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || run(&url, max_tries, &handle))
                .await
                .map_err(|e| JobError::retry(format!("The issue-sync pass stopped: {e}")))?
        })
    })
}

fn run(database_url: &str, max_tries: i32, handle: &JobHandle) -> Result<(), JobError> {
    let mut conn = kairos_db::establish_migration_connection(database_url).map_err(|e| {
        JobError::retry(format!(
            "The issue-sync pass cannot connect to the database: {e}"
        ))
    })?;
    let tenants = kairos_db::tenant::list_tenants(&mut conn).map_err(|e| {
        JobError::retry(format!(
            "The issue-sync pass cannot list the organizations: {e}"
        ))
    })?;
    let now = Utc::now();
    let mut queued = 0usize;
    for tenant in tenants {
        handle.progress();
        if !tenant.schema_exists {
            continue;
        }
        let schema = kairos_db::tenant::tenant_schema_name(&tenant.slug);
        diesel::sql_query(format!("SET search_path TO \"{schema}\""))
            .execute(&mut conn)
            .map_err(|e| {
                JobError::retry(format!("The issue-sync pass cannot pin {schema}: {e}"))
            })?;
        let result = queue_due(&mut conn, &tenant.slug, now, max_tries);
        let reset = diesel::sql_query("SET search_path TO DEFAULT").execute(&mut conn);
        match result {
            Ok(done) => queued += done.queued.len(),
            // One organization that fails (a schema that is not migrated
            // yet) does not stop the others.
            Err(e) => {
                tracing::warn!(tenant = %tenant.slug, error = %e, "issue sync: the organization was skipped")
            }
        }
        reset.map_err(|e| {
            JobError::retry(format!("The issue-sync pass cannot reset the schema: {e}"))
        })?;
    }
    if queued > 0 {
        tracing::info!(queued, "issue sync: pass done");
    }
    // The pass looks every 30 seconds. It runs as one row of the queue for
    // ever, not a new row for each look (KAIROS-T-0636).
    handle.rearm_in(TICK);
    Ok(())
}
