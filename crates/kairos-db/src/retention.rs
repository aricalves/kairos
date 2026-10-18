//! Retention sweeper (KAIROS-T-0015, contract per the KAIROS-A-0004
//! "History Retention: Sweeper, Compaction, and Offload" amendment, config
//! per KAIROS-A-0013).
//!
//! KAIROS-A-0009 layering, same shape as [`crate::items`]: this module
//! loads `item_history`/`activity_log` rows, delegates the pure decision
//! (WHICH rows leave the database) to
//! [`kairos_core::retention::plan_history_compaction`], and performs the
//! I/O: **offload-then-prune**. Rows are serialized as NDJSON (one JSON
//! object per row, full column fidelity) to the archive target and the
//! file is fsynced BEFORE any `DELETE` runs — data is never destroyed
//! without a completed offload copy unless the operator explicitly set
//! `KAIROS_RETENTION_MODE=discard`.
//!
//! # Mode semantics (KAIROS-A-0004)
//!
//! - `archive` + configured filesystem target: offload → fsync → prune.
//! - `archive` + NO target: read NOTHING and prune nothing; a warning is
//!   logged (`tracing::warn!`), counted on the [`SweepReport`], and the
//!   audit row records `skipped: no target` (KAIROS-T-0631).
//! - `archive` + S3 target: typed [`RetentionError::ArchiveTargetNotImplemented`]
//!   — v1 ships the filesystem target only (KAIROS-T-0015; no S3 SDK
//!   dependency). The target parses so operators get a clear error, not a
//!   misconfiguration.
//! - `discard`: prune without archiving.
//! - `off`: complete no-op (no prune, no archive, no `retention_sweep`
//!   activity row).
//!
//! # Paging (KAIROS-T-0631)
//!
//! The sweep never loads a table whole. It reads `item_history` a page of
//! items at a time, in `item_id` order, and only the metadata columns
//! `(id, item_id, version, edited_at)`; it reads the content only of the
//! rows that it archives. It reads `activity_log` a page of rows at a time,
//! in `(occurred_at, id)` order. Each delete names at most one batch of
//! ids. [`SweepPaging`] sets the sizes.
//!
//! # The rows that move an item stay (KAIROS-T-0571)
//!
//! The `activity_log` sweep does not read or remove the rows of the
//! actions in [`kairos_core::retention::KEPT_ACTIVITY_ACTIONS`]
//! (`transition`, `cancel`, `board_move`), at any age. The flow metrics
//! of a board read them: without the first move into Active, a task that
//! is in progress for more than the window has no age.
//!
//! # Archive layout
//!
//! One file per sweep per tenant per table:
//! `{target}/{tenant_slug}/{table}/{timestamp}.ndjson`, where `timestamp`
//! is the injected sweep clock (`now`) — the sweeper never reads the wall
//! clock, so tests inject fixed clocks end to end. No file is written when
//! a table has nothing to prune.
//!
//! # Audit + metrics seam
//!
//! Every sweep (except `off`) appends one `activity_log` row with action
//! `retention_sweep` (the KAIROS-A-0004 extension of the action set; the
//! DDL leaves `action` unconstrained TEXT on purpose) carrying per-table
//! archived/pruned/warning counts, actor = the nil UUID (system actor —
//! `actor_id` references `public.users` by documentation only, no FK).
//! The same counts are exposed on [`SweepReport`]; the server's `/metrics`
//! endpoint (KAIROS-A-0013) will export them as Prometheus counters
//! (`rows_archived`/`rows_pruned`/`warnings` per table, labelled by tenant)
//! when it is wired — no metrics crate is taken here on purpose. The
//! server runs [`run_retention_pass`] as a periodic job
//! (KAIROS-T-0366, KAIROS-T-0376).

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Bool, Text};
use serde::Serialize;
use uuid::Uuid;

use kairos_core::retention::{
    ArchiveTarget, HistoryRowMeta, RetentionConfig, RetentionMode, plan_history_compaction,
};

use crate::models::enums::ActivityAction;
use crate::models::graph::ItemHistory;
use crate::tenant::{self, TenantError};

/// Errors from the retention sweeper.
#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    /// The slug does not match the KAIROS-S-0004 organization slug pattern.
    #[error(
        "The tenant slug {0:?} is not correct. A tenant slug must match ^[a-z][a-z0-9_-]{{1,62}}$."
    )]
    InvalidSlug(String),
    /// No `org_{slug}` schema exists for this tenant.
    #[error("The tenant {0:?} does not exist.")]
    TenantNotFound(String),
    /// The configured archive target kind is recognized but not shipped in
    /// v1 (KAIROS-T-0015: filesystem only; S3-compatible offload is a
    /// follow-up — NOT IMPLEMENTED).
    #[error(
        "This version does not have the archive target {0:?}. It has only the filesystem \
         target (KAIROS-T-0015)."
    )]
    ArchiveTargetNotImplemented(String),
    /// Writing (or fsyncing) an archive file failed; nothing was deleted.
    #[error("The server cannot write the archive at {path:?}: {source}.")]
    ArchiveIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Serializing a row for the NDJSON archive failed.
    #[error("The server cannot write a row of the archive as JSON: {0}.")]
    Serialize(#[from] serde_json::Error),
    /// Listing tenants for the fleet sweep failed.
    #[error(transparent)]
    Tenant(#[from] TenantError),
    /// Any other database error.
    #[error("The database gave an error: {0}.")]
    Database(#[from] DieselError),
}

/// Per-table sweep counters (the KAIROS-A-0013 metrics seam — see module
/// docs; Prometheus export happens in the server, M2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableCounts {
    /// Rows written to the NDJSON archive (0 in `discard` mode).
    pub rows_archived: u64,
    /// Rows deleted from the table.
    pub rows_pruned: u64,
    /// Warnings raised (currently: 1 when `archive` mode had no archive
    /// target configured, so the sweep was skipped and pruning paused).
    pub warnings: u64,
}

/// Why a sweep read and pruned nothing (KAIROS-T-0631).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepSkip {
    /// `archive` mode with no archive target.
    NoTarget,
}

impl SweepSkip {
    /// The record of the skip: `skipped: no target`.
    pub fn as_str(self) -> &'static str {
        match self {
            SweepSkip::NoTarget => kairos_core::retention::SKIPPED_NO_TARGET,
        }
    }
}

/// What one [`sweep_tenant`] run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// The organization slug.
    pub slug: String,
    /// The mode the sweep ran in.
    pub mode: RetentionMode,
    /// `item_history` counters (compaction tiers per KAIROS-A-0004).
    pub item_history: TableCounts,
    /// `activity_log` counters (plain window, no compaction tiers).
    pub activity_log: TableCounts,
    /// Archive files written by this sweep (empty unless `archive` mode
    /// pruned something).
    pub archive_files: Vec<PathBuf>,
    /// Set when the sweep read and pruned nothing, with the reason.
    pub skipped: Option<SweepSkip>,
    /// The pages of items of `item_history` that the sweep read.
    pub history_pages: u64,
    /// The pages of rows of `activity_log` that the sweep read.
    pub activity_pages: u64,
}

impl SweepReport {
    fn new(slug: &str, mode: RetentionMode) -> Self {
        SweepReport {
            slug: slug.to_string(),
            mode,
            item_history: TableCounts::default(),
            activity_log: TableCounts::default(),
            archive_files: Vec::new(),
            skipped: None,
            history_pages: 0,
            activity_pages: 0,
        }
    }

    /// The `retention_sweep` activity row's `details` payload: per-table
    /// counts, stable format. A skipped sweep names the reason after the
    /// mode: `mode:archive skipped: no target item_history:{…}`.
    fn details(&self) -> String {
        let skipped = self
            .skipped
            .map(|skip| format!(" {}", skip.as_str()))
            .unwrap_or_default();
        format!(
            "mode:{}{skipped} item_history:{{archived:{},pruned:{},warnings:{}}} \
             activity_log:{{archived:{},pruned:{},warnings:{}}}",
            self.mode,
            self.item_history.rows_archived,
            self.item_history.rows_pruned,
            self.item_history.warnings,
            self.activity_log.rows_archived,
            self.activity_log.rows_pruned,
            self.activity_log.warnings,
        )
    }
}

// ---------------------------------------------------------------------------
// NDJSON archive rows (full column fidelity)
// ---------------------------------------------------------------------------

/// Full-fidelity `item_history` archive row (every DDL column).
#[derive(Serialize)]
struct HistoryArchiveRow<'a> {
    id: Uuid,
    item_id: Uuid,
    version: i32,
    title: &'a str,
    content: &'a str,
    edited_by: Uuid,
    edited_at: DateTime<Utc>,
    agent_key_id: Option<Uuid>,
}

impl<'a> From<&'a ItemHistory> for HistoryArchiveRow<'a> {
    fn from(row: &'a ItemHistory) -> Self {
        HistoryArchiveRow {
            id: row.id,
            item_id: row.item_id,
            version: row.version,
            title: &row.title,
            content: &row.content,
            edited_by: row.edited_by,
            edited_at: row.edited_at,
            agent_key_id: row.agent_key_id,
        }
    }
}

/// A loaded `activity_log` row. Loaded with `action` as raw TEXT (NOT the
/// [`crate::models::enums::ActivityAction`] enum) because archived logs may
/// contain actions from older releases that the enum no longer knows; the
/// sweep must age those rows out, not fail on them.
type ActivityRow = (
    Uuid,
    Uuid,
    String,
    Option<Uuid>,
    Option<String>,
    String,
    DateTime<Utc>,
    Option<Uuid>,
);

/// Full-fidelity `activity_log` archive row (every DDL column).
#[derive(Serialize)]
struct ActivityArchiveRow<'a> {
    id: Uuid,
    actor_id: Uuid,
    action: &'a str,
    entity_id: Option<Uuid>,
    entity_type: Option<&'a str>,
    details: &'a str,
    occurred_at: DateTime<Utc>,
    agent_key_id: Option<Uuid>,
}

impl<'a> From<&'a ActivityRow> for ActivityArchiveRow<'a> {
    fn from(row: &'a ActivityRow) -> Self {
        ActivityArchiveRow {
            id: row.0,
            actor_id: row.1,
            action: &row.2,
            entity_id: row.3,
            entity_type: row.4.as_deref(),
            details: &row.5,
            occurred_at: row.6,
            agent_key_id: row.7,
        }
    }
}

/// `{target}/{tenant}/{table}/{timestamp}.ndjson` (timestamp = the injected
/// sweep clock, filename-safe UTC format with microseconds).
fn archive_path(base: &Path, slug: &str, table: &str, now: DateTime<Utc>) -> PathBuf {
    base.join(slug)
        .join(table)
        .join(format!("{}.ndjson", now.format("%Y%m%dT%H%M%S%.6fZ")))
}

/// One NDJSON archive file that a sweep writes page by page
/// (KAIROS-T-0631). The file is NEW: [`NdjsonFile::create`] refuses to
/// overwrite an existing file (two sweeps sharing a tenant/table/timestamp
/// would clobber an archive). [`NdjsonFile::finish`] fsyncs it: the offload
/// must be durable before any row is deleted.
struct NdjsonFile {
    path: PathBuf,
    file: std::io::BufWriter<fs::File>,
    lines: u64,
}

impl NdjsonFile {
    fn io_err(path: &Path) -> impl Fn(std::io::Error) -> RetentionError + '_ {
        move |source| RetentionError::ArchiveIo {
            path: path.to_path_buf(),
            source,
        }
    }

    fn create(path: PathBuf) -> Result<Self, RetentionError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(Self::io_err(&path))?;
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(Self::io_err(&path))?;
        Ok(NdjsonFile {
            path,
            file: std::io::BufWriter::new(file),
            lines: 0,
        })
    }

    fn write_line(&mut self, line: &str) -> Result<(), RetentionError> {
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.write_all(b"\n"))
            .map_err(Self::io_err(&self.path))?;
        self.lines += 1;
        Ok(())
    }

    /// Flush and fsync; the path and the number of lines.
    fn finish(self) -> Result<(PathBuf, u64), RetentionError> {
        let NdjsonFile { path, file, lines } = self;
        let file = file.into_inner().map_err(|e| RetentionError::ArchiveIo {
            path: path.clone(),
            source: e.into_error(),
        })?;
        file.sync_all().map_err(Self::io_err(&path))?;
        Ok((path, lines))
    }
}

/// The archive file of one table, opened at its first line: no file is
/// written when a table has nothing to prune.
struct LazyArchive {
    path: PathBuf,
    file: Option<NdjsonFile>,
}

impl LazyArchive {
    fn new(path: PathBuf) -> Self {
        LazyArchive { path, file: None }
    }

    fn write_line(&mut self, line: &str) -> Result<(), RetentionError> {
        if self.file.is_none() {
            self.file = Some(NdjsonFile::create(self.path.clone())?);
        }
        match self.file.as_mut() {
            Some(file) => file.write_line(line),
            None => Ok(()),
        }
    }

    /// The path and the lines of the file, when one was written.
    fn finish(self) -> Result<Option<(PathBuf, u64)>, RetentionError> {
        self.file.map(NdjsonFile::finish).transpose()
    }
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// The page and batch sizes of a sweep (KAIROS-T-0631). The sweep never
/// holds a whole table in memory: it reads `item_history` a page of items
/// at a time (metadata columns only), `activity_log` a page of rows at a
/// time, and names at most `batch` ids in one content read or one
/// `DELETE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepPaging {
    /// The items whose history rows one page reads.
    pub items_per_page: i64,
    /// The `activity_log` rows that one page reads.
    pub activity_per_page: i64,
    /// The ids that one content read or one `DELETE` names.
    pub batch: usize,
}

impl Default for SweepPaging {
    fn default() -> Self {
        SweepPaging {
            items_per_page: 500,
            activity_per_page: 5_000,
            batch: 1_000,
        }
    }
}

impl SweepPaging {
    /// Each size at least 1.
    fn checked(self) -> Self {
        SweepPaging {
            items_per_page: self.items_per_page.max(1),
            activity_per_page: self.activity_per_page.max(1),
            batch: self.batch.max(1),
        }
    }
}

#[derive(QueryableByName)]
struct BoolRow {
    #[diesel(sql_type = Bool)]
    present: bool,
}

fn schema_exists(conn: &mut PgConnection, schema: &str) -> Result<bool, DieselError> {
    let row: BoolRow =
        sql_query("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1) AS present")
            .bind::<Text, _>(schema)
            .get_result(conn)?;
    Ok(row.present)
}

/// Run one retention sweep for one tenant (KAIROS-A-0004 tiers, mode
/// semantics per module docs), with an injected clock — `now` is the only
/// time source; the sweeper never calls the wall clock. The default
/// [`SweepPaging`]; see [`sweep_tenant_with`].
pub fn sweep_tenant(
    conn: &mut PgConnection,
    slug: &str,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<SweepReport, RetentionError> {
    sweep_tenant_with(conn, slug, config, now, SweepPaging::default())
}

/// [`sweep_tenant`] with the page and batch sizes `paging`.
///
/// Pins `search_path` to the tenant schema for the duration (restored to
/// DEFAULT afterwards, success or failure, like
/// [`crate::tenant::migrate_all_tenants`]). Archive files are written and
/// fsynced BEFORE the deletes; the deletes and the `retention_sweep`
/// activity row commit in one transaction.
pub fn sweep_tenant_with(
    conn: &mut PgConnection,
    slug: &str,
    config: &RetentionConfig,
    now: DateTime<Utc>,
    paging: SweepPaging,
) -> Result<SweepReport, RetentionError> {
    if config.mode == RetentionMode::Off {
        // Disabled: full no-op — not even the audit row.
        return Ok(SweepReport::new(slug, RetentionMode::Off));
    }
    if !tenant::is_valid_tenant_slug(slug) {
        return Err(RetentionError::InvalidSlug(slug.to_string()));
    }
    let schema = tenant::tenant_schema_name(slug);
    if !schema_exists(conn, &schema)? {
        return Err(RetentionError::TenantNotFound(slug.to_string()));
    }

    sql_query(format!("SET search_path TO \"{schema}\"")).execute(conn)?;
    let result = sweep_current_schema(conn, slug, config, now, paging.checked());
    // Always restore the connection's default search_path, even on error,
    // before surfacing the outcome (same discipline as migrate_all_tenants).
    let reset = sql_query("SET search_path TO DEFAULT").execute(conn);
    let report = result?;
    reset?;
    Ok(report)
}

/// The `item_history` columns that the planner needs: `(id, item_id,
/// version, edited_at)`. Never the title or the content.
type HistoryMetaRow = (Uuid, Uuid, i32, DateTime<Utc>);

/// Plan `item_history` page by page (KAIROS-T-0631): a page is the
/// metadata of every row of up to `items_per_page` items, in `item_id`
/// order, so the planner sees whole items. The content is read only for
/// the rows that `archive` writes, `batch` rows at a time. The ids to
/// delete, in `(item_id, version)` order.
fn plan_history_pages(
    conn: &mut PgConnection,
    slug: &str,
    config: &RetentionConfig,
    now: DateTime<Utc>,
    paging: SweepPaging,
    mut archive: Option<&mut LazyArchive>,
    report: &mut SweepReport,
) -> Result<Vec<Uuid>, RetentionError> {
    use crate::schema::item_history;

    let mut delete_ids = Vec::new();
    let mut after: Option<Uuid> = None;
    loop {
        let mut page = item_history::table
            .select(item_history::item_id)
            .distinct()
            .order(item_history::item_id.asc())
            .limit(paging.items_per_page)
            .into_boxed();
        if let Some(last) = after {
            page = page.filter(item_history::item_id.gt(last));
        }
        let items: Vec<Uuid> = page.load(conn)?;
        let Some(last) = items.last().copied() else {
            break;
        };
        report.history_pages += 1;

        let rows: Vec<HistoryMetaRow> = item_history::table
            .filter(item_history::item_id.eq_any(&items))
            .select((
                item_history::id,
                item_history::item_id,
                item_history::version,
                item_history::edited_at,
            ))
            .load(conn)?;
        let ids: HashMap<(Uuid, i32), Uuid> = rows
            .iter()
            .map(|(id, item, version, _)| ((*item, *version), *id))
            .collect();
        let meta: Vec<HistoryRowMeta> = rows
            .iter()
            .map(|(_, item_id, version, edited_at)| HistoryRowMeta {
                item_id: *item_id,
                version: *version,
                edited_at: *edited_at,
            })
            .collect();
        // The plan is sorted by (item_id, version), so the ids are too.
        let page_ids: Vec<Uuid> = plan_history_compaction(&meta, now, config)
            .into_iter()
            .filter_map(|k| ids.get(&(k.item_id, k.version)).copied())
            .collect();

        if let Some(archive) = archive.as_deref_mut() {
            for chunk in page_ids.chunks(paging.batch) {
                let full: Vec<ItemHistory> = item_history::table
                    .filter(item_history::id.eq_any(chunk))
                    .order((item_history::item_id.asc(), item_history::version.asc()))
                    .select(ItemHistory::as_select())
                    .load(conn)?;
                for row in &full {
                    archive.write_line(&serde_json::to_string(&HistoryArchiveRow::from(row))?)?;
                }
            }
        }
        delete_ids.extend(page_ids);

        if (items.len() as i64) < paging.items_per_page {
            break;
        }
        after = Some(last);
    }
    tracing::debug!(
        tenant = slug,
        pages = report.history_pages,
        candidates = delete_ids.len(),
        "retention: item_history planned"
    );
    Ok(delete_ids)
}

/// Read the `activity_log` rows older than the window page by page, in
/// `(occurred_at, id)` order, and archive them when `archive` is set. The
/// ids to delete. The rows that move an item stay (module docs).
fn plan_activity_pages(
    conn: &mut PgConnection,
    cutoff: DateTime<Utc>,
    paging: SweepPaging,
    mut archive: Option<&mut LazyArchive>,
    report: &mut SweepReport,
) -> Result<Vec<Uuid>, RetentionError> {
    use crate::schema::activity_log;

    let mut delete_ids = Vec::new();
    let mut after: Option<(DateTime<Utc>, Uuid)> = None;
    loop {
        let mut page = activity_log::table
            .filter(activity_log::occurred_at.lt(cutoff))
            .filter(activity_log::action.ne_all(kairos_core::retention::KEPT_ACTIVITY_ACTIONS))
            .order((activity_log::occurred_at.asc(), activity_log::id.asc()))
            .limit(paging.activity_per_page)
            .select((
                activity_log::id,
                activity_log::actor_id,
                activity_log::action,
                activity_log::entity_id,
                activity_log::entity_type,
                activity_log::details,
                activity_log::occurred_at,
                activity_log::agent_key_id,
            ))
            .into_boxed();
        if let Some((at, id)) = after {
            page = page.filter(
                activity_log::occurred_at
                    .gt(at)
                    .or(activity_log::occurred_at
                        .eq(at)
                        .and(activity_log::id.gt(id))),
            );
        }
        let rows: Vec<ActivityRow> = page.load(conn)?;
        let Some(last) = rows.last().map(|r| (r.6, r.0)) else {
            break;
        };
        report.activity_pages += 1;
        if let Some(archive) = archive.as_deref_mut() {
            for row in &rows {
                archive.write_line(&serde_json::to_string(&ActivityArchiveRow::from(row))?)?;
            }
        }
        delete_ids.extend(rows.iter().map(|r| r.0));
        if (rows.len() as i64) < paging.activity_per_page {
            break;
        }
        after = Some(last);
    }
    Ok(delete_ids)
}

/// Write the `retention_sweep` audit row of `report` (A-0004: with row
/// counts). Nil UUID = system actor; occurred_at = the injected sweep
/// clock.
fn insert_audit_row(
    conn: &mut PgConnection,
    report: &SweepReport,
    now: DateTime<Utc>,
) -> Result<(), DieselError> {
    use crate::schema::activity_log;
    diesel::insert_into(activity_log::table)
        .values((
            activity_log::actor_id.eq(Uuid::nil()),
            activity_log::action.eq(ActivityAction::RetentionSweep),
            activity_log::details.eq(report.details()),
            activity_log::occurred_at.eq(now),
        ))
        .execute(conn)?;
    Ok(())
}

/// The sweep body; assumes `search_path` is pinned to the tenant schema.
fn sweep_current_schema(
    conn: &mut PgConnection,
    slug: &str,
    config: &RetentionConfig,
    now: DateTime<Utc>,
    paging: SweepPaging,
) -> Result<SweepReport, RetentionError> {
    use crate::schema::{activity_log, item_history};

    let mut report = SweepReport::new(slug, config.mode);

    // The mode decides first, so a sweep that cannot prune reads nothing.
    let target: Option<&Path> = match (config.mode, &config.archive_target) {
        (RetentionMode::Off, _) => unreachable!("gated in sweep_tenant"),
        (RetentionMode::Archive, None) => {
            // A-0004: compaction pauses; data is never destroyed without an
            // offload copy unless the operator explicitly chose `discard`.
            // KAIROS-T-0631: the sweep reads no row, and says so.
            tracing::warn!(
                tenant = slug,
                "retention: archive mode with no {} configured; item_history and \
                 activity_log are not swept, nothing pruned ({})",
                kairos_core::retention::ENV_ARCHIVE_TARGET,
                kairos_core::retention::SKIPPED_NO_TARGET,
            );
            report.skipped = Some(SweepSkip::NoTarget);
            report.item_history.warnings = 1;
            report.activity_log.warnings = 1;
            insert_audit_row(conn, &report, now)?;
            return Ok(report);
        }
        (RetentionMode::Archive, Some(ArchiveTarget::S3(url))) => {
            return Err(RetentionError::ArchiveTargetNotImplemented(url.clone()));
        }
        (RetentionMode::Archive, Some(ArchiveTarget::Filesystem(base))) => Some(base.as_path()),
        (RetentionMode::Discard, _) => None,
    };

    let mut history_archive =
        target.map(|base| LazyArchive::new(archive_path(base, slug, "item_history", now)));
    let history_delete_ids = plan_history_pages(
        conn,
        slug,
        config,
        now,
        paging,
        history_archive.as_mut(),
        &mut report,
    )?;

    // activity_log: plain retention window, no compaction tiers (A-0004 §4).
    let activity_cutoff = now - Duration::days(i64::from(config.activity_retention_days));
    let mut activity_archive =
        target.map(|base| LazyArchive::new(archive_path(base, slug, "activity_log", now)));
    let activity_delete_ids = plan_activity_pages(
        conn,
        activity_cutoff,
        paging,
        activity_archive.as_mut(),
        &mut report,
    )?;

    // The offload is durable before any delete.
    for (archive, counts) in [
        (history_archive, &mut report.item_history),
        (activity_archive, &mut report.activity_log),
    ] {
        if let Some((path, lines)) = archive.map(LazyArchive::finish).transpose()?.flatten() {
            counts.rows_archived = lines;
            report.archive_files.push(path);
        }
    }

    // Prune + audit in ONE transaction (the archive files above are already
    // durable; a failed transaction leaves at worst an orphan archive copy,
    // never a deleted-but-unarchived row). The deletes go in batches.
    conn.transaction::<_, DieselError, _>(|conn| {
        for chunk in history_delete_ids.chunks(paging.batch) {
            let n = diesel::delete(item_history::table.filter(item_history::id.eq_any(chunk)))
                .execute(conn)?;
            report.item_history.rows_pruned += n as u64;
        }
        for chunk in activity_delete_ids.chunks(paging.batch) {
            let n = diesel::delete(activity_log::table.filter(activity_log::id.eq_any(chunk)))
                .execute(conn)?;
            report.activity_log.rows_pruned += n as u64;
        }
        insert_audit_row(conn, &report, now)
    })?;

    Ok(report)
}

/// Fleet sweep: run [`sweep_tenant`] for every provisioned organization,
/// in slug order (same iteration pattern as
/// [`crate::tenant::migrate_all_tenants`]). Stops at the first failing
/// tenant. `mode = off` returns an empty report list without touching the
/// database beyond the tenant listing.
pub fn sweep_all_tenants(
    conn: &mut PgConnection,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<Vec<SweepReport>, RetentionError> {
    if config.mode == RetentionMode::Off {
        return Ok(Vec::new());
    }
    let tenants = tenant::list_tenants(conn)?;
    let mut reports = Vec::with_capacity(tenants.len());
    for tenant_info in tenants {
        if !tenant_info.schema_exists {
            return Err(RetentionError::TenantNotFound(tenant_info.slug));
        }
        reports.push(sweep_tenant(conn, &tenant_info.slug, config, now)?);
    }
    Ok(reports)
}

/// Delete, in each tenant, the notifications that were read more than
/// `notifications_read_days` days before `now` (KAIROS-T-0381). In each
/// mode, also `off`: a read notification is not archived. 0 days deletes
/// nothing. Returns the count.
pub fn delete_read_notifications(
    conn: &mut PgConnection,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<u64, RetentionError> {
    if config.notifications_read_days == 0 {
        return Ok(0);
    }
    let cutoff = now - Duration::days(i64::from(config.notifications_read_days));
    let mut deleted = 0u64;
    for tenant_info in tenant::list_tenants(conn)? {
        if !tenant_info.schema_exists {
            continue;
        }
        let schema = tenant::tenant_schema_name(&tenant_info.slug);
        sql_query(format!("SET search_path TO \"{schema}\"")).execute(conn)?;
        let result = crate::notifications::delete_read_before(conn, cutoff);
        let reset = sql_query("SET search_path TO DEFAULT").execute(conn);
        deleted += result? as u64;
        reset?;
    }
    Ok(deleted)
}

/// Delete, in each tenant, the summaries of the code-index pools that no
/// stored index has used and that a build made more than
/// `code_index_pool_days` days before `now` (KAIROS-T-0378). In each mode,
/// also `off`: such a summary is a cache that a build can make again. 0
/// days deletes nothing. Returns the count.
pub fn delete_unused_summaries(
    conn: &mut PgConnection,
    config: &RetentionConfig,
    now: DateTime<Utc>,
) -> Result<u64, RetentionError> {
    if config.code_index_pool_days == 0 {
        return Ok(0);
    }
    let cutoff = now - Duration::days(i64::from(config.code_index_pool_days));
    let mut deleted = 0u64;
    for tenant_info in tenant::list_tenants(conn)? {
        if !tenant_info.schema_exists {
            continue;
        }
        let schema = tenant::tenant_schema_name(&tenant_info.slug);
        sql_query(format!("SET search_path TO \"{schema}\"")).execute(conn)?;
        let result = crate::code_indexes::delete_unused(conn, cutoff);
        let reset = sql_query("SET search_path TO DEFAULT").execute(conn);
        deleted += result? as u64;
        reset?;
    }
    Ok(deleted)
}

/// What one [`run_retention_pass`] did (KAIROS-T-0366).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPass {
    /// The mode of the pass.
    pub mode: RetentionMode,
    /// One report per tenant (empty when the mode is `off`).
    pub tenants: Vec<SweepReport>,
    /// The expired password sessions (`public.local_sessions`) the pass
    /// deleted.
    pub expired_sessions_deleted: u64,
    /// The rows of the login throttle (`public.auth_failures`) that can
    /// change no decision, which the pass deleted (KAIROS-T-0377).
    pub expired_auth_failures_deleted: u64,
    /// The read notifications that the pass deleted in all the tenants
    /// (KAIROS-T-0381).
    pub read_notifications_deleted: u64,
    /// The summaries of the code-index pools that no stored index used,
    /// which the pass deleted in all the tenants (KAIROS-T-0378).
    pub unused_summaries_deleted: u64,
}

impl RetentionPass {
    /// The record of the pass for its job (KAIROS-T-0631):
    /// `skipped: no target` when a sweep was skipped, `off` in the mode
    /// `off`, else the rows that the sweeps archived and pruned.
    pub fn result(&self) -> String {
        if let Some(skip) = self.tenants.iter().find_map(|t| t.skipped) {
            return skip.as_str().to_string();
        }
        if self.mode == RetentionMode::Off {
            return "off".to_string();
        }
        let sum = |f: fn(&SweepReport) -> u64| self.tenants.iter().map(f).sum::<u64>();
        format!(
            "swept {} tenants: item_history archived {} pruned {}, activity_log archived {} \
             pruned {}",
            self.tenants.len(),
            sum(|t| t.item_history.rows_archived),
            sum(|t| t.item_history.rows_pruned),
            sum(|t| t.activity_log.rows_archived),
            sum(|t| t.activity_log.rows_pruned),
        )
    }
}

/// One retention pass of the server (KAIROS-T-0366): sweep each tenant (as
/// [`sweep_all_tenants`], which obeys the mode) and delete the password
/// sessions that expired before `now`. The session delete runs in every
/// mode, also `off`: an expired session cannot authenticate, so the row has
/// no use. So does the delete of the rows of the login throttle whose
/// window and lockout ended (KAIROS-T-0377), of the read notifications
/// (KAIROS-T-0381) and of the unused summaries of the code-index pools
/// (KAIROS-T-0378). `progress` is called before each tenant and after
/// each delete.
///
/// The pass is a periodic job of the job queue of the server
/// (KAIROS-T-0376): the queue gives each run to one server, so the pass
/// takes no lock of its own.
pub fn run_retention_pass(
    conn: &mut PgConnection,
    config: &RetentionConfig,
    now: DateTime<Utc>,
    progress: &mut dyn FnMut(),
) -> Result<RetentionPass, RetentionError> {
    let mut tenants = Vec::new();
    if config.mode != RetentionMode::Off {
        for tenant_info in tenant::list_tenants(conn)? {
            progress();
            if !tenant_info.schema_exists {
                return Err(RetentionError::TenantNotFound(tenant_info.slug));
            }
            tenants.push(sweep_tenant(conn, &tenant_info.slug, config, now)?);
        }
    }
    let expired = crate::local_auth::delete_expired_sessions(conn, now)?;
    let throttle = crate::auth_failures::delete_expired(conn, now)?;
    let notifications = delete_read_notifications(conn, config, now)?;
    progress();
    let summaries = delete_unused_summaries(conn, config, now)?;
    progress();
    Ok(RetentionPass {
        mode: config.mode,
        tenants,
        expired_sessions_deleted: expired as u64,
        expired_auth_failures_deleted: throttle as u64,
        read_notifications_deleted: notifications,
        unused_summaries_deleted: summaries,
    })
}
