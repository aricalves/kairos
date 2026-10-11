//! The GitHub write-back of a repository (KAIROS-T-0386, COLLIERY-I-0614).
//!
//! For each pull request that links to an item (forge links,
//! [`crate::forge`]), Kairos writes 2 things on GitHub, when the repository
//! has a write token:
//!
//! - one comment for each linked item (its code, title, column, claim and
//!   link), made once and then edited in place ([`comment_of`],
//!   [`set_comment`]);
//! - a commit status `kairos` on the head commit of the pull request.
//!
//! The writes are jobs of the job queue (kind [`JOB_KIND`]), one job for a
//! pull request: a change of an item never waits for GitHub, and a failed
//! write is tried again. [`on_item_changed`] queues the jobs in the
//! transaction of the change of the item (column, claim, cancel); the forge
//! webhook queues one when a link is written; a new write token queues one
//! for each open pull request of the repository.
//!
//! The write token is stored ENCRYPTED, like the read token
//! ([`crate::repository_credentials`]): this module never sees it. No token:
//! no job, and nothing is written.
//!
//! The connection must be pinned to a tenant schema. This module does not
//! check rights: the caller does.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Array, BigInt, Bool, Bytea, Nullable, Text, Timestamptz, Uuid as SqlUuid};
use uuid::Uuid;

use crate::jobs::{self, NewJob};
use crate::models::enums::ActivityAction;
use crate::models::graph::NewActivityLogEntry;

/// The kind of the job of a write-back.
pub const JOB_KIND: &str = "github_writeback";

/// The priority of the job: as a webhook delivery, after an embedding
/// batch and before a periodic pass.
pub const JOB_PRIORITY: i32 = 15;

/// The tries of a write-back job.
pub const MAX_TRIES: i32 = 6;

/// The lease of a pull request while one job writes it, in seconds.
pub const BUSY_SECS: f64 = 120.0;

// ---------------------------------------------------------------------------
// The write token
// ---------------------------------------------------------------------------

/// The encrypted write token of a repository, to write.
#[derive(Clone, PartialEq, Eq)]
pub struct NewWriteToken {
    pub repository_id: Uuid,
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub key_id: String,
    pub set_by: Uuid,
}

impl std::fmt::Debug for NewWriteToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewWriteToken")
            .field("repository_id", &self.repository_id)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// The stored, encrypted write token of a repository.
#[derive(Clone, PartialEq, Eq, QueryableByName)]
pub struct StoredWriteToken {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    #[diesel(sql_type = Bytea)]
    pub ciphertext: Vec<u8>,
    #[diesel(sql_type = Bytea)]
    pub nonce: Vec<u8>,
    #[diesel(sql_type = Text)]
    pub key_id: String,
}

impl std::fmt::Debug for StoredWriteToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredWriteToken")
            .field("repository_id", &self.repository_id)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// What a read of the write token shows. It has no ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct WriteTokenStatus {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    pub set_by: Uuid,
    /// The display name of `set_by`, else the email.
    #[diesel(sql_type = Nullable<Text>)]
    pub set_by_name: Option<String>,
    #[diesel(sql_type = Timestamptz)]
    pub set_at: DateTime<Utc>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub last_write_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Nullable<Bool>)]
    pub last_write_ok: Option<bool>,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_write_error: Option<String>,
}

fn log_activity(
    conn: &mut PgConnection,
    actor_id: Uuid,
    repository_id: Uuid,
    details: String,
) -> Result<(), DieselError> {
    diesel::insert_into(crate::schema::activity_log::table)
        .values(NewActivityLogEntry {
            actor_id,
            action: ActivityAction::Repository,
            entity_id: Some(repository_id),
            entity_type: Some("repository".to_string()),
            details,
        })
        .execute(conn)?;
    Ok(())
}

/// Write the encrypted write token of a repository, and an activity row for
/// `token.set_by` (`repository_write_token_set:<slug>` or
/// `repository_write_token_replaced:<slug>`). A replacement clears the
/// result of the last write.
pub fn put_token(
    conn: &mut PgConnection,
    token: &NewWriteToken,
    slug: &str,
) -> Result<bool, DieselError> {
    conn.transaction(|conn| {
        let existed = load_token(conn, token.repository_id)?.is_some();
        sql_query(
            "INSERT INTO repository_write_tokens (repository_id, ciphertext, nonce, key_id, \
                 set_by) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (repository_id) DO UPDATE SET ciphertext = EXCLUDED.ciphertext, \
                 nonce = EXCLUDED.nonce, key_id = EXCLUDED.key_id, set_by = EXCLUDED.set_by, \
                 set_at = now(), last_write_at = NULL, last_write_ok = NULL, \
                 last_write_error = NULL",
        )
        .bind::<SqlUuid, _>(token.repository_id)
        .bind::<Bytea, _>(&token.ciphertext)
        .bind::<Bytea, _>(&token.nonce)
        .bind::<Text, _>(&token.key_id)
        .bind::<SqlUuid, _>(token.set_by)
        .execute(conn)?;
        let verb = if existed { "replaced" } else { "set" };
        log_activity(
            conn,
            token.set_by,
            token.repository_id,
            format!("repository_write_token_{verb}:{slug}"),
        )?;
        Ok(existed)
    })
}

/// The encrypted write token of a repository, if it has one.
pub fn load_token(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<Option<StoredWriteToken>, DieselError> {
    sql_query(
        "SELECT repository_id, ciphertext, nonce, key_id FROM repository_write_tokens \
         WHERE repository_id = $1",
    )
    .bind::<SqlUuid, _>(repository_id)
    .get_result(conn)
    .optional()
}

/// Each write token, locked, for the rotation of `KAIROS_SECRETS_KEY`.
pub fn tokens_for_update(conn: &mut PgConnection) -> Result<Vec<StoredWriteToken>, DieselError> {
    sql_query(
        "SELECT repository_id, ciphertext, nonce, key_id FROM repository_write_tokens \
         ORDER BY repository_id FOR UPDATE",
    )
    .load(conn)
}

/// Replace the ciphertext of a write token (the rotation of the key).
pub fn reseal_token(conn: &mut PgConnection, token: &StoredWriteToken) -> Result<(), DieselError> {
    sql_query(
        "UPDATE repository_write_tokens SET ciphertext = $2, nonce = $3, key_id = $4 \
         WHERE repository_id = $1",
    )
    .bind::<SqlUuid, _>(token.repository_id)
    .bind::<Bytea, _>(&token.ciphertext)
    .bind::<Bytea, _>(&token.nonce)
    .bind::<Text, _>(&token.key_id)
    .execute(conn)?;
    Ok(())
}

/// The status of the write token of each repository in `ids` that has one.
pub fn token_statuses(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, WriteTokenStatus>, DieselError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<WriteTokenStatus> = sql_query(
        "SELECT w.repository_id, w.set_by, \
                COALESCE(NULLIF(btrim(u.display_name), ''), u.email) AS set_by_name, \
                w.set_at, w.last_write_at, w.last_write_ok, w.last_write_error \
         FROM repository_write_tokens w LEFT JOIN public.users u ON u.id = w.set_by \
         WHERE w.repository_id = ANY($1)",
    )
    .bind::<Array<SqlUuid>, _>(ids)
    .load(conn)?;
    Ok(rows.into_iter().map(|r| (r.repository_id, r)).collect())
}

/// Remove the write token of a repository, and write an activity row
/// (`repository_write_token_removed:<slug>`). False: no token.
pub fn delete_token(
    conn: &mut PgConnection,
    repository_id: Uuid,
    actor: Uuid,
    slug: &str,
) -> Result<bool, DieselError> {
    conn.transaction(|conn| {
        let n = sql_query("DELETE FROM repository_write_tokens WHERE repository_id = $1")
            .bind::<SqlUuid, _>(repository_id)
            .execute(conn)?;
        if n == 0 {
            return Ok(false);
        }
        log_activity(
            conn,
            actor,
            repository_id,
            format!("repository_write_token_removed:{slug}"),
        )?;
        Ok(true)
    })
}

/// Record the result of a write. `error` must have no token in it.
pub fn record_write(
    conn: &mut PgConnection,
    repository_id: Uuid,
    ok: bool,
    error: Option<&str>,
) -> Result<(), DieselError> {
    sql_query(
        "UPDATE repository_write_tokens SET last_write_at = now(), last_write_ok = $2, \
             last_write_error = $3 \
         WHERE repository_id = $1",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Bool, _>(ok)
    .bind::<Nullable<Text>, _>(error)
    .execute(conn)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The jobs
// ---------------------------------------------------------------------------

#[derive(QueryableByName)]
struct PrRow {
    #[diesel(sql_type = SqlUuid)]
    repository_id: Uuid,
    #[diesel(sql_type = Text)]
    external_id: String,
}

#[derive(QueryableByName)]
struct Found {
    #[diesel(sql_type = Bool)]
    found: bool,
}

/// The key of the job of a pull request, in its payload.
fn pr_key(tenant: &str, repository_id: Uuid, pr_number: i64) -> String {
    format!("github-writeback:{tenant}:{repository_id}:{pr_number}")
}

/// Queue the write-back of the pull request `pr_number` of a repository,
/// unless a job of that pull request waits already (it reads the items when
/// it runs, so it writes this change too). Only for a GitHub repository with
/// a write token. True: a job was added.
pub fn queue_pr(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<bool, DieselError> {
    let eligible: Found = sql_query(
        "SELECT EXISTS (SELECT 1 FROM repositories r \
                          JOIN repository_write_tokens w ON w.repository_id = r.id \
                         WHERE r.id = $1 AND r.forge = 'github' AND r.deleted_at IS NULL) \
         AS found",
    )
    .bind::<SqlUuid, _>(repository_id)
    .get_result(conn)?;
    if !eligible.found {
        return Ok(false);
    }
    let tenant = crate::webhooks::current_tenant(conn)?;
    let key = pr_key(&tenant, repository_id, pr_number);
    let waiting: Found = sql_query(
        "SELECT EXISTS (SELECT 1 FROM public.jobs WHERE kind = $1 AND state = 'queued' \
                          AND payload->>'pr_key' = $2) AS found",
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
            tenant: Some(tenant),
            payload: serde_json::json!({
                "repository_id": repository_id,
                "pr_number": pr_number,
                "pr_key": key,
            }),
            priority: JOB_PRIORITY,
            dedupe_key: None,
            run_after: None,
            max_tries: MAX_TRIES,
        },
    )?;
    Ok(true)
}

/// Queue the write-back of each pull request that links to the item (any
/// type), in the transaction of its change. The number of jobs added.
pub fn on_item_changed(conn: &mut PgConnection, item_id: Uuid) -> Result<usize, DieselError> {
    let prs: Vec<PrRow> = sql_query(
        "SELECT DISTINCT r.id AS repository_id, l.external_id \
           FROM item_links l \
           JOIN forge_connections c ON c.id = l.connection_id AND c.deleted_at IS NULL \
           JOIN repositories r ON r.id = c.repository_id AND r.forge = 'github' \
           JOIN repository_write_tokens w ON w.repository_id = r.id \
          WHERE l.item_id = $1 AND l.kind = 'pull_request'",
    )
    .bind::<SqlUuid, _>(item_id)
    .load(conn)?;
    let mut added = 0;
    for pr in prs {
        if let Ok(number) = pr.external_id.parse::<i64>()
            && queue_pr(conn, pr.repository_id, number)?
        {
            added += 1;
        }
    }
    Ok(added)
}

/// Queue the write-back of each open pull request (open or draft) of a
/// repository that links to an item: a new write token writes them. The
/// number of jobs added.
pub fn queue_open_prs(conn: &mut PgConnection, repository_id: Uuid) -> Result<usize, DieselError> {
    let prs: Vec<PrRow> = sql_query(
        "SELECT DISTINCT c.repository_id, l.external_id \
           FROM item_links l \
           JOIN forge_connections c ON c.id = l.connection_id AND c.deleted_at IS NULL \
          WHERE c.repository_id = $1 AND l.kind = 'pull_request' \
            AND l.state IN ('open', 'draft')",
    )
    .bind::<SqlUuid, _>(repository_id)
    .load(conn)?;
    let mut added = 0;
    for pr in prs {
        if let Ok(number) = pr.external_id.parse::<i64>()
            && queue_pr(conn, pr.repository_id, number)?
        {
            added += 1;
        }
    }
    Ok(added)
}

// ---------------------------------------------------------------------------
// What a job reads and writes
// ---------------------------------------------------------------------------

/// The repository of a job: its name on GitHub, and its token.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct Target {
    #[diesel(sql_type = SqlUuid)]
    pub repository_id: Uuid,
    #[diesel(sql_type = Text)]
    pub slug: String,
    #[diesel(sql_type = Text)]
    pub repo_full_name: String,
    #[diesel(sql_type = Text)]
    pub forge: String,
}

/// The repository `repository_id`, live.
pub fn target(conn: &mut PgConnection, repository_id: Uuid) -> Result<Option<Target>, DieselError> {
    sql_query(
        "SELECT id AS repository_id, slug, repo_full_name, forge FROM repositories \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind::<SqlUuid, _>(repository_id)
    .get_result(conn)
    .optional()
}

/// One item that links to a pull request, with what the comment and the
/// status show.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct PrItem {
    #[diesel(sql_type = SqlUuid)]
    pub item_id: Uuid,
    #[diesel(sql_type = Text)]
    pub short_code: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub entity_type: String,
    /// The column; `None` for a document.
    #[diesel(sql_type = Nullable<Text>)]
    pub column: Option<String>,
    #[diesel(sql_type = Bool)]
    pub done: bool,
    /// The reason of the cancel of a task; `None` when it is not cancelled.
    #[diesel(sql_type = Nullable<Text>)]
    pub cancel_reason: Option<String>,
    /// The person with the claim of a task.
    #[diesel(sql_type = Nullable<Text>)]
    pub claim: Option<String>,
    #[diesel(sql_type = Nullable<Timestamptz>)]
    pub claimed_at: Option<DateTime<Utc>>,
    #[diesel(sql_type = Bool)]
    pub archived: bool,
}

/// The items that link to the pull request `pr_number` of a repository,
/// by code.
pub fn pr_items(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<Vec<PrItem>, DieselError> {
    sql_query(
        "SELECT d.id AS item_id, d.short_code, d.title, d.entity_type, \
                col.name AS \"column\", COALESCE(col.is_done, false) AS done, \
                tc.reason AS cancel_reason, \
                COALESCE(NULLIF(btrim(u.display_name), ''), u.email) AS claim, \
                cl.claimed_at, (d.deleted_at IS NOT NULL) AS archived \
           FROM (SELECT DISTINCT l.item_id FROM item_links l \
                   JOIN forge_connections c ON c.id = l.connection_id \
                    AND c.deleted_at IS NULL AND c.repository_id = $1 \
                  WHERE l.kind = 'pull_request' AND l.external_id = $2) x \
           JOIN entity_directory d ON d.id = x.item_id \
           LEFT JOIN (SELECT id, column_id FROM tasks \
                      UNION ALL SELECT id, column_id FROM initiatives \
                      UNION ALL SELECT id, column_id FROM strategies \
                      UNION ALL SELECT id, column_id FROM adrs) p ON p.id = d.id \
           LEFT JOIN board_columns col ON col.id = p.column_id \
           LEFT JOIN task_cancellations tc ON tc.task_id = d.id \
           LEFT JOIN task_claims cl ON cl.task_id = d.id \
           LEFT JOIN public.users u ON u.id = cl.user_id \
          ORDER BY d.short_code",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<Text, _>(pr_number.to_string())
    .load(conn)
}

/// The state of the write-back of a pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, QueryableByName)]
pub struct PrState {
    #[diesel(sql_type = Nullable<Text>)]
    pub head_sha: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub status_state: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub status_description: Option<String>,
}

/// Take the lease of a pull request for one job, for [`BUSY_SECS`]. The
/// state of the last write; `None`: another job holds the lease now.
pub fn claim_pr(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<Option<PrState>, DieselError> {
    sql_query(
        "INSERT INTO github_pr_writebacks AS w (repository_id, pr_number, busy_until) \
         VALUES ($1, $2, now() + make_interval(secs => $3)) \
         ON CONFLICT (repository_id, pr_number) DO UPDATE \
             SET busy_until = EXCLUDED.busy_until \
             WHERE w.busy_until IS NULL OR w.busy_until < now() \
         RETURNING head_sha, status_state, status_description",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<BigInt, _>(pr_number)
    .bind::<diesel::sql_types::Double, _>(BUSY_SECS)
    .get_result(conn)
    .optional()
}

/// End the lease of a pull request; with `status`, record the status that
/// was written (head, state, description).
pub fn release_pr(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pr_number: i64,
    status: Option<&PrState>,
) -> Result<(), DieselError> {
    match status {
        Some(state) => sql_query(
            "UPDATE github_pr_writebacks SET busy_until = NULL, head_sha = $3, \
                 status_state = $4, status_description = $5, updated_at = now() \
             WHERE repository_id = $1 AND pr_number = $2",
        )
        .bind::<SqlUuid, _>(repository_id)
        .bind::<BigInt, _>(pr_number)
        .bind::<Nullable<Text>, _>(state.head_sha.as_deref())
        .bind::<Nullable<Text>, _>(state.status_state.as_deref())
        .bind::<Nullable<Text>, _>(state.status_description.as_deref())
        .execute(conn)?,
        None => sql_query(
            "UPDATE github_pr_writebacks SET busy_until = NULL \
             WHERE repository_id = $1 AND pr_number = $2",
        )
        .bind::<SqlUuid, _>(repository_id)
        .bind::<BigInt, _>(pr_number)
        .execute(conn)?,
    };
    Ok(())
}

/// The comment of an item on a pull request: its id on GitHub, and the
/// SHA-256 of its body.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct Comment {
    #[diesel(sql_type = SqlUuid)]
    pub item_id: Uuid,
    #[diesel(sql_type = BigInt)]
    pub comment_id: i64,
    #[diesel(sql_type = Text)]
    pub body_sha256: String,
}

/// The comments of a pull request, by item.
pub fn comments_of(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pr_number: i64,
) -> Result<HashMap<Uuid, Comment>, DieselError> {
    let rows: Vec<Comment> = sql_query(
        "SELECT item_id, comment_id, body_sha256 FROM github_pr_comments \
         WHERE repository_id = $1 AND pr_number = $2",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<BigInt, _>(pr_number)
    .load(conn)?;
    Ok(rows.into_iter().map(|c| (c.item_id, c)).collect())
}

/// Record the comment of an item on a pull request.
pub fn set_comment(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pr_number: i64,
    comment: &Comment,
) -> Result<(), DieselError> {
    sql_query(
        "INSERT INTO github_pr_comments (repository_id, pr_number, item_id, comment_id, \
             body_sha256) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (repository_id, pr_number, item_id) DO UPDATE \
             SET comment_id = EXCLUDED.comment_id, body_sha256 = EXCLUDED.body_sha256, \
                 updated_at = now()",
    )
    .bind::<SqlUuid, _>(repository_id)
    .bind::<BigInt, _>(pr_number)
    .bind::<SqlUuid, _>(comment.item_id)
    .bind::<BigInt, _>(comment.comment_id)
    .bind::<Text, _>(&comment.body_sha256)
    .execute(conn)?;
    Ok(())
}
