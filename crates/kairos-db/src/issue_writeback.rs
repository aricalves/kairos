//! The write-back of an issue to the forge (KAIROS-T-0575).
//!
//! Issue intake (KAIROS-T-0482) makes a support task from an opened GitHub
//! or GitLab issue, and keeps one `forge_issue_intake` row for it. When the
//! repository has a write token, the issue follows its task:
//!
//! - the task is done: a comment ("Done in ACME-T-0012; PR #37.") and the
//!   issue closed;
//! - the task is cancelled: a comment with the reason, and the issue closed
//!   as not planned;
//! - the task is reopened: the issue opened again.
//!
//! The writes are jobs of the job queue (kind [`JOB_KIND`]), one job for an
//! issue. [`on_item_changed`] queues a job in the transaction of the change
//! of the task, when the state that the task wants differs from the state
//! that Kairos last wrote (`written_state`). A new write token queues one
//! job for each issue that differs ([`queue_changed_issues`]).
//!
//! The connection must be pinned to a tenant schema. This module does not
//! check rights: the caller does.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Bool, Nullable, Text, Uuid as SqlUuid};
use uuid::Uuid;

use crate::jobs::{self, NewJob};

/// The kind of the job of an issue write-back.
pub const JOB_KIND: &str = "issue_writeback";

/// The priority of the job: as the write-back of a pull request.
pub const JOB_PRIORITY: i32 = crate::writeback::JOB_PRIORITY;

/// The tries of a job: as the write-back of a pull request.
pub const MAX_TRIES: i32 = crate::writeback::MAX_TRIES;

/// The lease of an issue while one job writes it, in seconds.
pub const BUSY_SECS: f64 = 120.0;

/// The state of an issue that Kairos writes.
pub const OPEN: &str = "open";
/// The issue is closed because its task is done.
pub const COMPLETED: &str = "completed";
/// The issue is closed because its task is cancelled.
pub const NOT_PLANNED: &str = "not_planned";

/// The state that a task wants for its issue, as SQL. It reads the aliases
/// `col` (the column of the task) and `tc` (its cancel mark).
const WANTED: &str = "CASE WHEN col.is_done IS NOT TRUE THEN 'open' \
                           WHEN tc.task_id IS NOT NULL THEN 'not_planned' \
                           ELSE 'completed' END";

#[derive(QueryableByName)]
struct IssueKey {
    #[diesel(sql_type = SqlUuid)]
    repository_id: Uuid,
    #[diesel(sql_type = Text)]
    issue_id: String,
}

#[derive(QueryableByName)]
struct Found {
    #[diesel(sql_type = Bool)]
    found: bool,
}

/// The key of the job of an issue, in its payload.
fn issue_key(tenant: &str, repository_id: Uuid, issue_id: &str) -> String {
    format!("issue-writeback:{tenant}:{repository_id}:{issue_id}")
}

/// The issues to queue, with the filter `filter` on `i` (the intake row):
/// a GitHub or GitLab repository with a write token, and a task that wants
/// a state that Kairos did not write.
fn changed(conn: &mut PgConnection, filter: &str, id: Uuid) -> Result<Vec<IssueKey>, DieselError> {
    sql_query(format!(
        "SELECT i.repository_id, i.issue_id \
           FROM forge_issue_intake i \
           JOIN repositories r ON r.id = i.repository_id AND r.deleted_at IS NULL \
                AND r.forge IN ('github', 'gitlab') \
           JOIN repository_write_tokens w ON w.repository_id = r.id \
           JOIN tasks t ON t.id = i.item_id \
           LEFT JOIN board_columns col ON col.id = t.column_id \
           LEFT JOIN task_cancellations tc ON tc.task_id = t.id \
          WHERE {filter} = $1 AND i.written_state <> {WANTED} \
          ORDER BY i.repository_id, i.issue_id"
    ))
    .bind::<SqlUuid, _>(id)
    .load(conn)
}

/// Queue the write-back of one issue, unless a job of that issue waits
/// already (it reads the task when it runs). True: a job was added.
fn queue_issue(
    conn: &mut PgConnection,
    tenant: &str,
    repository_id: Uuid,
    issue_id: &str,
) -> Result<bool, DieselError> {
    let key = issue_key(tenant, repository_id, issue_id);
    let waiting: Found = sql_query(
        "SELECT EXISTS (SELECT 1 FROM public.jobs WHERE kind = $1 AND state = 'queued' \
                          AND payload->>'issue_key' = $2) AS found",
    )
    .bind::<Text, _>(JOB_KIND)
    .bind::<Text, _>(&key)
    .get_result(conn)?;
    if waiting.found {
        return Ok(false);
    }
    jobs::enqueue(
        conn,
        &NewJob {
            kind: JOB_KIND.to_string(),
            tenant: Some(tenant.to_string()),
            payload: serde_json::json!({
                "repository_id": repository_id,
                "issue_id": issue_id,
                "issue_key": key,
            }),
            priority: JOB_PRIORITY,
            dedupe_key: None,
            run_after: None,
            max_tries: MAX_TRIES,
        },
    )?;
    Ok(true)
}

fn queue_all(conn: &mut PgConnection, keys: Vec<IssueKey>) -> Result<usize, DieselError> {
    if keys.is_empty() {
        return Ok(0);
    }
    let tenant = crate::webhooks::current_tenant(conn)?;
    let mut added = 0;
    for key in keys {
        if queue_issue(conn, &tenant, key.repository_id, &key.issue_id)? {
            added += 1;
        }
    }
    Ok(added)
}

/// Queue the write-back of the issue of a task, in the transaction of its
/// change (column, cancel, reopen). Nothing for a task with no intake row,
/// with no write token, or with no new state. The number of jobs added.
pub fn on_item_changed(conn: &mut PgConnection, item_id: Uuid) -> Result<usize, DieselError> {
    let keys = changed(conn, "i.item_id", item_id)?;
    queue_all(conn, keys)
}

/// Queue the write-back of each issue of a repository whose task wants a
/// new state: a new write token writes them. The number of jobs added.
pub fn queue_changed_issues(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<usize, DieselError> {
    let keys = changed(conn, "i.repository_id", repository_id)?;
    queue_all(conn, keys)
}

// ---------------------------------------------------------------------------
// What a job reads and writes
// ---------------------------------------------------------------------------

/// The issue of a job and its task, read when the job takes the lease.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct IssueTarget {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    #[diesel(sql_type = Text)]
    pub issue_id: String,
    /// The number that people see (`#12`; GitLab `iid`).
    #[diesel(sql_type = BigInt)]
    pub number: i64,
    #[diesel(sql_type = SqlUuid)]
    pub item_id: Uuid,
    #[diesel(sql_type = Text)]
    pub short_code: String,
    /// The state that Kairos last wrote on the issue.
    #[diesel(sql_type = Text)]
    pub written_state: String,
    /// The state whose comment is on the issue.
    #[diesel(sql_type = Nullable<Text>)]
    pub comment_state: Option<String>,
    /// The state that the task wants now.
    #[diesel(sql_type = Text)]
    pub wanted: String,
    /// The reason of the cancel; `None` when the task is not cancelled.
    #[diesel(sql_type = Nullable<Text>)]
    pub cancel_reason: Option<String>,
}

/// What [`claim_issue`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// No intake row, or no task: nothing to write.
    Missing,
    /// Another job holds the lease now.
    Busy,
    /// The lease is taken for [`BUSY_SECS`].
    Taken(IssueTarget),
}

/// Take the lease of an issue for one job, and read its task.
pub fn claim_issue(
    conn: &mut PgConnection,
    repository_id: Uuid,
    issue_id: &str,
) -> Result<Claim, DieselError> {
    let exists: Found = sql_query(
        "SELECT EXISTS (SELECT 1 FROM forge_issue_intake \
                         WHERE repository_id = $1 AND issue_id = $2) AS found",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Text, _>(issue_id)
    .get_result(conn)?;
    if !exists.found {
        return Ok(Claim::Missing);
    }
    let taken = sql_query(
        "UPDATE forge_issue_intake SET busy_until = now() + make_interval(secs => $3) \
          WHERE repository_id = $1 AND issue_id = $2 \
            AND (busy_until IS NULL OR busy_until < now())",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Text, _>(issue_id)
    .bind::<diesel::sql_types::Double, _>(BUSY_SECS)
    .execute(conn)?;
    if taken == 0 {
        return Ok(Claim::Busy);
    }
    let target: Option<IssueTarget> = sql_query(format!(
        "SELECT i.repository_id, i.issue_id, i.number, i.item_id, t.short_code, \
                i.written_state, i.comment_state, {WANTED} AS wanted, \
                tc.reason AS cancel_reason \
           FROM forge_issue_intake i \
           JOIN tasks t ON t.id = i.item_id \
           LEFT JOIN board_columns col ON col.id = t.column_id \
           LEFT JOIN task_cancellations tc ON tc.task_id = t.id \
          WHERE i.repository_id = $1 AND i.issue_id = $2"
    ))
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Text, _>(issue_id)
    .get_result(conn)
    .optional()?;
    match target {
        Some(target) => Ok(Claim::Taken(target)),
        None => {
            release_issue(conn, repository_id, issue_id, None)?;
            Ok(Claim::Missing)
        }
    }
}

/// Record that the comment of `state` is on the issue.
pub fn set_comment_state(
    conn: &mut PgConnection,
    repository_id: Uuid,
    issue_id: &str,
    state: &str,
) -> Result<(), DieselError> {
    sql_query(
        "UPDATE forge_issue_intake SET comment_state = $3 \
          WHERE repository_id = $1 AND issue_id = $2",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Text, _>(issue_id)
    .bind::<Text, _>(state)
    .execute(conn)?;
    Ok(())
}

/// End the lease of an issue; with `written`, record the state that was
/// written. An issue that is open again has no comment of its state.
pub fn release_issue(
    conn: &mut PgConnection,
    repository_id: Uuid,
    issue_id: &str,
    written: Option<&str>,
) -> Result<(), DieselError> {
    match written {
        Some(state) => sql_query(
            "UPDATE forge_issue_intake SET busy_until = NULL, written_state = $3, \
                 comment_state = CASE WHEN $3 = 'open' THEN NULL ELSE comment_state END \
             WHERE repository_id = $1 AND issue_id = $2",
        )
        .bind::<SqlUuid, _>(repository_id)
        .bind::<Text, _>(issue_id)
        .bind::<Text, _>(state)
        .execute(conn)?,
        None => sql_query(
            "UPDATE forge_issue_intake SET busy_until = NULL \
             WHERE repository_id = $1 AND issue_id = $2",
        )
        .bind::<SqlUuid, _>(repository_id)
        .bind::<Text, _>(issue_id)
        .execute(conn)?,
    };
    Ok(())
}

/// A pull request (a GitLab merge request) that links to a task.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct TaskPr {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    #[diesel(sql_type = Text)]
    pub repo_full_name: String,
    #[diesel(sql_type = Text)]
    pub forge: String,
    #[diesel(sql_type = Text)]
    pub number: String,
}

/// The pull requests of a task that are not closed unmerged, by
/// repository and number.
pub fn task_prs(conn: &mut PgConnection, item_id: Uuid) -> Result<Vec<TaskPr>, DieselError> {
    let mut prs: Vec<TaskPr> = sql_query(
        "SELECT DISTINCT r.id AS repository_id, r.repo_full_name, r.forge::text AS forge, \
                l.external_id AS number \
           FROM item_links l \
           JOIN forge_connections c ON c.id = l.connection_id \
           JOIN repositories r ON r.id = c.repository_id \
          WHERE l.item_id = $1 AND l.kind = 'pull_request' AND l.state <> 'closed'",
    )
    .bind::<SqlUuid, _>(item_id)
    .load(conn)?;
    prs.sort_by(|a, b| {
        (&a.repo_full_name, a.number.parse::<i64>().unwrap_or(0))
            .cmp(&(&b.repo_full_name, b.number.parse::<i64>().unwrap_or(0)))
    });
    Ok(prs)
}
