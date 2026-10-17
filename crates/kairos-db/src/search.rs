//! The unified search/traverse pipeline (KAIROS-A-0007, KAIROS-T-0014):
//! `execute_search` runs a validated [`SearchRequest`] through the A-0007
//! execution order and returns fully typed results grouped by entity type.
//!
//! The pure request model and validation live in [`kairos_core::search`];
//! this module is the SQL side of the contract. Per KAIROS-A-0009 the
//! recursive-CTE traversal, the ranking query (the text match, the filters,
//! the sort and the page), and the metadata pre-pass are sanctioned
//! `diesel::sql_query` raw SQL; per-type hydration of the page is typed
//! diesel.
//!
//! # Pipeline (A-0007 execution order)
//!
//! 1. **Traverse** (if present): resolve the root through
//!    `entity_directory`, under an explicit `deleted_at IS NULL` unless
//!    `filter.include_deleted` asks to start from archived work
//!    (KAIROS-T-0157; an unknown root — or an archived one without the flag
//!    — is the typed [`SearchError::TraverseRootNotFound`]), then walk
//!    `item_relationships` with a recursive CTE — parameterized
//!    relationship types, direction (`outbound`/`inbound`/`both`), and
//!    depth. Cycle safety: `UNION` deduplicates `(id, depth)` rows and
//!    depth strictly increases toward the validated cap
//!    ([`kairos_core::search::MAX_TRAVERSE_DEPTH`]), so the walk always
//!    terminates; the root itself is excluded from the results. The walk
//!    runs over the raw edge graph — edges through soft-deleted items are
//!    followed (their live descendants stay reachable); visibility is
//!    enforced at type resolution and hydration.
//! 2. **Full-text `q`** (if present): a condition of the ranking query
//!    (step 5), `tsv @@ websearch_to_tsquery('english', $q)` on the tsv
//!    expression of the `idx_*_tsv` GIN indexes — chosen over
//!    `plainto_tsquery` because it accepts arbitrary end-user input without
//!    ever erroring and supports quoted phrases, `OR`, and `-negation`. The
//!    expression is the one of the `searchable_items` view (S-0004),
//!    character for character, so the index serves it. Since KAIROS-T-0157
//!    the query spells `deleted_at IS NULL` only when
//!    `filter.include_deleted` is absent (KAIROS-A-0020 rule 2); the
//!    indexes are not partial, so both modes are index-served.
//!
//!    A `q` that is a short code (any case) also matches the item with that
//!    code, and the item of that code when it is retired (COLLIERY-T-3100):
//!    the result has the item with its current code. That item ranks first.
//!    The root of a traverse follows a retired code the same way.
//! 3. **Metadata filter** (if present): one pre-pass query over
//!    `item_metadata`/`metadata_definitions` (pairs unnested server-side)
//!    returning ids that satisfy EVERY entry; values use the T-0011 LIKE
//!    translation ([`kairos_core::search::metadata_like_pattern`]).
//! 4. **Candidate set**: the intersection of the traverse and metadata
//!    sets (whichever are present). No set ⇒ pure structural filtering.
//! 5. **Rank, count, page in SQL** (KAIROS-T-0623): ONE query, a `UNION
//!    ALL` arm per type that can match, over its base table. Each arm
//!    carries the candidate ids (`= ANY`), the text match, the structural
//!    filter predicates (`board_id`, `column_id`, `team_id`,
//!    `repository_id`, `task_type`, `is_bucket`, `created_after`/`before`,
//!    `created_by`, and the subqueries of `claimed_by` and `owned_by`),
//!    `metadata_missing` as `NOT EXISTS`, and the `deleted_at IS NULL`
//!    default. It projects the id, the type, the short code and the sort
//!    key. Filters on attributes a type does not have exclude the type
//!    outright (e.g. `task_type` ⇒ tasks only, `board_id` ⇒ no documents),
//!    so those types get no arm. `repository_id` keeps three types
//!    (COLLIERY-T-0269): the tasks that link to the repository, and the
//!    documents and the ADRs that impact it. `board_id` is the board that
//!    an item is a card of. A document is a card of no board, the board
//!    that it names as its owner too, so `board_id` excludes each document.
//!    `ORDER BY` the field (`created_at`/`updated_at`/`title`/`relevance`),
//!    then `short_code` for determinism, both text keys `COLLATE "C"` (the
//!    byte order of a Rust `str`); `count(*) OVER ()` is the `total` before
//!    `LIMIT`/`OFFSET`. `relevance` is `ts_rank_cd` and is the **default
//!    when `q` is present** (KAIROS-T-0186); it requires `q`, and asking for
//!    it without one is a validation error rather than a silent fallback.
//!    Only the relevance sort computes a rank. A page past the end has no
//!    row to carry the total: the query runs once more for its first row.
//! 6. **Hydrate the page only**: at most one typed `SELECT … WHERE id =
//!    ANY(…)` per entity type on the page — **bounded at 5 queries and at
//!    `limit` rows regardless of result size**
//!    ([`SearchStats::hydration_queries`] proves it) — regrouped by type in
//!    the page's order.
//!
//! Every public function operates in the CURRENT `search_path` tenant
//! schema (same conventions as [`crate::items`]); the pipeline is
//! read-only, so no transaction is opened.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{Array, BigInt, Bool, Integer, Text, Timestamptz, Uuid as SqlUuid};
use uuid::Uuid;

use kairos_core::search::{
    self as core_search, Direction, SearchFilter, SearchRequest, SearchTaskType,
    SearchValidationError, SearchWorkClass, Sort, SortField, SortOrder, Traverse, TraverseFrom,
};
use kairos_core::short_code::ItemType;

use crate::models::enums::{TaskType, UnknownEnumValue, WorkClass};
use crate::models::items::{Adr, Document, Initiative, Strategy, Task};

/// Errors from the search pipeline.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The request failed [`kairos_core::search::validate`] (HTTP 400).
    #[error(transparent)]
    Invalid(#[from] SearchValidationError),
    /// `traverse.from` names no entity the request may see: unknown, or
    /// soft-deleted without `filter.include_deleted` (KAIROS-T-0157).
    #[error("The item {reference} of traverse.from does not exist.")]
    TraverseRootNotFound {
        /// The submitted `short_code` or `id`, for the error envelope.
        reference: String,
    },
    /// A slug of `filter.metadata_missing` names no metadata definition
    /// (KAIROS-T-0368). Refused, so that a typo does not match each item.
    #[error(
        "The slug {0:?} of filter.metadata_missing is not a metadata definition. Send the slug of a metadata definition of the organization."
    )]
    UnknownMetadataDefinition(String),
    /// Any other database error.
    #[error("The database gave an error: {0}.")]
    Database(#[from] DieselError),
}

/// Search results grouped by entity type (A-0007 response shape), each
/// group fully typed — no lossy common-denominator projection. Group order
/// within each vector follows the request's combined sort.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchResults {
    /// Matching strategies.
    pub strategies: Vec<Strategy>,
    /// Matching initiatives.
    pub initiatives: Vec<Initiative>,
    /// Matching tasks.
    pub tasks: Vec<Task>,
    /// Matching documents.
    pub documents: Vec<Document>,
    /// Matching ADRs.
    pub adrs: Vec<Adr>,
    /// Total matches BEFORE pagination.
    pub total: i64,
    /// The applied page size.
    pub limit: i64,
    /// The applied offset.
    pub offset: i64,
}

/// Query-count instrumentation for one `execute_search` run — the proof
/// behind A-0007's "bounded at 5 hydration queries regardless of result
/// size".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchStats {
    /// Per-type hydration queries executed (step 6): one per type on the
    /// page, so always `<= 5`.
    pub hydration_queries: usize,
    /// Every query executed by the pipeline, hydration included.
    pub total_queries: usize,
}

/// Run the A-0007 search pipeline (module docs) for `request` in the
/// current tenant schema.
pub fn execute_search(
    conn: &mut PgConnection,
    request: &SearchRequest,
) -> Result<SearchResults, SearchError> {
    execute_search_with_stats(conn, request).map(|(results, _)| results)
}

/// [`execute_search`], also returning the [`SearchStats`] query counters
/// (integration tests assert the ≤5-hydration-query bound through this).
pub fn execute_search_with_stats(
    conn: &mut PgConnection,
    request: &SearchRequest,
) -> Result<(SearchResults, SearchStats), SearchError> {
    core_search::validate(request)?;
    let mut stats = SearchStats::default();

    let filter = request.filter.as_ref();
    let include_deleted = filter.is_some_and(|f| f.include_deleted);
    let limit = request.effective_limit();
    let offset = request.effective_offset();
    let mut results = SearchResults {
        limit,
        offset,
        ..Default::default()
    };

    // -- steps 1 and 3: candidate id sets ------------------------------------
    let mut candidates: Option<HashSet<Uuid>> = None;
    let intersect = |candidates: &mut Option<HashSet<Uuid>>, ids: HashSet<Uuid>| {
        *candidates = Some(match candidates.take() {
            Some(existing) => existing.intersection(&ids).copied().collect(),
            None => ids,
        });
    };

    if let Some(traverse) = &request.traverse {
        let root = resolve_root(conn, &traverse.from, include_deleted, &mut stats)?;
        let ids = traverse_ids(conn, root, traverse, &mut stats)?;
        intersect(&mut candidates, ids);
    }
    // Step 2: the text match is part of the ranking query; only the code
    // match is a query of its own (at most one row).
    let text = match &request.q {
        Some(q) => Some(TextMatch {
            q: q.clone(),
            codes: code_match_ids(conn, q, include_deleted, &mut stats)?,
        }),
        None => None,
    };
    if let Some(metadata) = filter
        .and_then(|f| f.metadata.as_ref())
        .filter(|m| !m.is_empty())
    {
        let ids = metadata_match_ids(conn, metadata, &mut stats)?;
        intersect(&mut candidates, ids);
    }

    // KAIROS-T-0368: the items that HAVE a value for one of the slugs of
    // `metadata_missing` leave the result, in the ranking query.
    let missing = match filter.and_then(|f| f.metadata_missing.as_ref()) {
        Some(slugs) => Some(known_metadata_slugs(conn, slugs, &mut stats)?),
        None => None,
    };

    let types = applicable_types(filter);
    if types.is_empty() || candidates.as_ref().is_some_and(HashSet::is_empty) {
        return Ok((results, stats));
    }

    // -- steps 4 and 5: rank, count and page in SQL --------------------------
    let ranking = Ranking {
        types: &types,
        filter,
        include_deleted,
        candidates: candidates.map(|ids| ids.into_iter().collect()),
        text,
        missing,
        sort: request.effective_sort(),
    };
    let mut page = ranking.page(conn, limit, offset, &mut stats)?;
    results.total = match page.first() {
        Some(row) => row.total,
        // A page past the end has no row to carry the total: ask for the
        // first row instead.
        None if offset > 0 => ranking
            .page(conn, 1, 0, &mut stats)?
            .first()
            .map_or(0, |row| row.total),
        None => 0,
    };

    // -- step 6: hydrate the page only ----------------------------------------
    let mut by_type: HashMap<ItemType, Vec<Uuid>> = HashMap::new();
    let mut order: Vec<(ItemType, Uuid)> = Vec::with_capacity(page.len());
    for row in page.drain(..) {
        let item_type = parse_entity_type(&row.entity_type)?;
        by_type.entry(item_type).or_default().push(row.id);
        order.push((item_type, row.id));
    }
    let mut strategies = HashMap::new();
    let mut initiatives = HashMap::new();
    let mut tasks = HashMap::new();
    let mut documents = HashMap::new();
    let mut adrs = HashMap::new();
    for (item_type, ids) in by_type {
        stats.hydration_queries += 1;
        stats.total_queries += 1;
        match item_type {
            ItemType::Strategy => {
                use crate::schema::strategies::dsl;
                strategies = keyed(
                    dsl::strategies
                        .filter(dsl::id.eq_any(ids))
                        .select(Strategy::as_select())
                        .load(conn)?,
                    |row: &Strategy| row.id,
                );
            }
            ItemType::Initiative => {
                use crate::schema::initiatives::dsl;
                initiatives = keyed(
                    dsl::initiatives
                        .filter(dsl::id.eq_any(ids))
                        .select(Initiative::as_select())
                        .load(conn)?,
                    |row: &Initiative| row.id,
                );
            }
            ItemType::Task => {
                use crate::schema::tasks::dsl;
                tasks = keyed(
                    dsl::tasks
                        .filter(dsl::id.eq_any(ids))
                        .select(Task::as_select())
                        .load(conn)?,
                    |row: &Task| row.id,
                );
            }
            ItemType::Document => {
                use crate::schema::documents::dsl;
                documents = keyed(
                    dsl::documents
                        .filter(dsl::id.eq_any(ids))
                        .select(Document::as_select())
                        .load(conn)?,
                    |row: &Document| row.id,
                );
            }
            ItemType::Adr => {
                use crate::schema::adrs::dsl;
                adrs = keyed(
                    dsl::adrs
                        .filter(dsl::id.eq_any(ids))
                        .select(Adr::as_select())
                        .load(conn)?,
                    |row: &Adr| row.id,
                );
            }
        }
    }
    // The page's order, regrouped by type. A row that went away between the
    // two queries is left out.
    for (item_type, id) in order {
        match item_type {
            ItemType::Strategy => results.strategies.extend(strategies.remove(&id)),
            ItemType::Initiative => results.initiatives.extend(initiatives.remove(&id)),
            ItemType::Task => results.tasks.extend(tasks.remove(&id)),
            ItemType::Document => results.documents.extend(documents.remove(&id)),
            ItemType::Adr => results.adrs.extend(adrs.remove(&id)),
        }
    }
    Ok((results, stats))
}

/// The rows by id.
fn keyed<T>(rows: Vec<T>, id: impl Fn(&T) -> Uuid) -> HashMap<Uuid, T> {
    rows.into_iter().map(|row| (id(&row), row)).collect()
}

// ---------------------------------------------------------------------------
// Steps 1-3: candidate id sets (sanctioned raw SQL, A-0009)
// ---------------------------------------------------------------------------

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
}

/// Resolve `traverse.from` to an entity id via `entity_directory`.
/// Live-only by default, stated here rather than inherited from the view
/// (KAIROS-T-0156); with `include_deleted` the root may be archived
/// (KAIROS-T-0157), because "what hung off this once?" is exactly the audit
/// question and a 404 is not an answer to it (KAIROS-A-0020).
fn resolve_root(
    conn: &mut PgConnection,
    from: &TraverseFrom,
    include_deleted: bool,
    stats: &mut SearchStats,
) -> Result<Uuid, SearchError> {
    let live_only = if include_deleted {
        ""
    } else {
        " AND deleted_at IS NULL"
    };
    stats.total_queries += 1;
    let row: Option<IdRow> = match (&from.short_code, from.id) {
        // COLLIERY-T-3100: a retired code names the item that had it.
        (Some(short_code), None) => sql_query(format!(
            "SELECT id FROM entity_directory \
              WHERE (short_code = $1 \
                     OR id = (SELECT item_id FROM retired_codes WHERE code = $1)){live_only}"
        ))
        .bind::<Text, _>(short_code)
        .get_result(conn)
        .optional()?,
        (None, Some(id)) => sql_query(format!(
            "SELECT id FROM entity_directory WHERE id = $1{live_only}"
        ))
        .bind::<SqlUuid, _>(id)
        .get_result(conn)
        .optional()?,
        // validate() enforces exactly-one before we get here.
        _ => unreachable!("validated traverse.from names exactly one reference"),
    };
    row.map(|r| r.id).ok_or_else(|| {
        let reference = from
            .short_code
            .clone()
            .unwrap_or_else(|| from.id.map(|id| id.to_string()).unwrap_or_default());
        SearchError::TraverseRootNotFound { reference }
    })
}

/// Recursive-CTE walk from `root` (module docs, step 1). The `UNION`
/// deduplicates `(id, depth)` rows and `w.depth < $3` bounds the recursion
/// at the validated depth, so cycles cannot loop it; `id <> $1` keeps the
/// root out even if a cycle re-reaches it.
fn traverse_ids(
    conn: &mut PgConnection,
    root: Uuid,
    traverse: &Traverse,
    stats: &mut SearchStats,
) -> Result<HashSet<Uuid>, SearchError> {
    let step = match traverse.direction {
        Direction::Outbound => {
            "SELECT r.target_id, w.depth + 1
             FROM walk w
             JOIN item_relationships r ON r.source_id = w.id
             WHERE r.relationship = ANY($2) AND w.depth < $3"
        }
        Direction::Inbound => {
            "SELECT r.source_id, w.depth + 1
             FROM walk w
             JOIN item_relationships r ON r.target_id = w.id
             WHERE r.relationship = ANY($2) AND w.depth < $3"
        }
        Direction::Both => {
            // KAIROS-T-0628: one UNION ALL arm per direction, so each arm probes its
            // relationship index (source or target); the CHECK source_id != target_id
            // keeps the arms disjoint, so no edge is counted twice.
            "SELECT n.other_id, w.depth + 1
             FROM walk w
             CROSS JOIN LATERAL (
                 SELECT r.target_id AS other_id FROM item_relationships r
                     WHERE r.source_id = w.id AND r.relationship = ANY($2)
                 UNION ALL
                 SELECT r.source_id FROM item_relationships r
                     WHERE r.target_id = w.id AND r.relationship = ANY($2)
             ) n
             WHERE w.depth < $3"
        }
    };
    let sql = format!(
        "WITH RECURSIVE walk(id, depth) AS (
             SELECT $1::uuid, 0
             UNION
             {step}
         )
         SELECT DISTINCT id FROM walk WHERE depth > 0 AND id <> $1"
    );
    let relationships: Vec<String> = traverse
        .relationships
        .iter()
        .map(|r| r.as_str().to_string())
        .collect();
    let depth = traverse.depth.expect("validated traverse carries a depth");

    stats.total_queries += 1;
    let rows: Vec<IdRow> = sql_query(sql)
        .bind::<SqlUuid, _>(root)
        .bind::<Array<Text>, _>(relationships)
        .bind::<Integer, _>(depth as i32)
        .load(conn)?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// The relevance of the item whose code (current or retired) is the query.
/// `ts_rank_cd` gives much less than this, so that item ranks first.
const CODE_MATCH_SCORE: f32 = 1.0e6;

/// The item whose short code is `q`, or whose retired code is `q`
/// (COLLIERY-T-3100, module docs step 2). Empty, with no query, when `q`
/// is not a short code. Live-only unless `include_deleted`.
fn code_match_ids(
    conn: &mut PgConnection,
    q: &str,
    include_deleted: bool,
    stats: &mut SearchStats,
) -> Result<Vec<Uuid>, SearchError> {
    let code = q.trim().to_ascii_uppercase();
    if kairos_core::short_code::parse_short_code(&code).is_none() {
        return Ok(Vec::new());
    }
    let live_only = if include_deleted {
        ""
    } else {
        " AND d.deleted_at IS NULL"
    };
    stats.total_queries += 1;
    let rows: Vec<IdRow> = sql_query(format!(
        "SELECT d.id FROM entity_directory d \
          WHERE (d.short_code = $1 \
                 OR d.id = (SELECT item_id FROM retired_codes WHERE code = $1)){live_only}"
    ))
    .bind::<Text, _>(code)
    .load(conn)?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// Ids satisfying EVERY metadata entry (module docs, step 3): one query,
/// pairs unnested server-side, `HAVING` requiring all keys matched. Values
/// go through the T-0011 LIKE translation; PostgreSQL's default LIKE escape
/// (`\`) applies.
fn metadata_match_ids(
    conn: &mut PgConnection,
    metadata: &std::collections::BTreeMap<String, String>,
    stats: &mut SearchStats,
) -> Result<HashSet<Uuid>, SearchError> {
    let slugs: Vec<String> = metadata.keys().cloned().collect();
    let patterns: Vec<String> = metadata
        .values()
        .map(|value| core_search::metadata_like_pattern(value))
        .collect();

    stats.total_queries += 1;
    let rows: Vec<IdRow> = sql_query(
        "SELECT im.item_id AS id
         FROM item_metadata im
         JOIN metadata_definitions md ON md.id = im.metadata_definition_id
         JOIN unnest($1::text[], $2::text[]) AS m(slug, pattern)
           ON md.slug = m.slug AND im.value LIKE m.pattern
         GROUP BY im.item_id
         HAVING COUNT(DISTINCT md.slug) = $3",
    )
    .bind::<Array<Text>, _>(slugs)
    .bind::<Array<Text>, _>(patterns)
    .bind::<BigInt, _>(metadata.len() as i64)
    .load(conn)?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// The slugs of `filter.metadata_missing`, trimmed (KAIROS-T-0368). Each
/// slug must name a metadata definition: an unknown slug is
/// [`SearchError::UnknownMetadataDefinition`]. The ranking query leaves
/// out each item that has a value (not empty) for one of them.
fn known_metadata_slugs(
    conn: &mut PgConnection,
    slugs: &[String],
    stats: &mut SearchStats,
) -> Result<Vec<String>, SearchError> {
    use crate::schema::metadata_definitions::dsl as md;
    let wanted: Vec<String> = slugs.iter().map(|s| s.trim().to_string()).collect();
    stats.total_queries += 1;
    let known: HashSet<String> = md::metadata_definitions
        .filter(md::slug.eq_any(&wanted))
        .select(md::slug)
        .load::<String>(conn)?
        .into_iter()
        .collect();
    if let Some(unknown) = wanted.iter().find(|s| !known.contains(*s)) {
        return Err(SearchError::UnknownMetadataDefinition(unknown.clone()));
    }
    Ok(wanted)
}

// ---------------------------------------------------------------------------
// Type of a ranked row
// ---------------------------------------------------------------------------

/// Parse an `entity_type` literal from type resolution. Only the five
/// S-0004 values exist; anything else is schema drift and fails loudly
/// (same convention as `crate::graph`).
fn parse_entity_type(value: &str) -> Result<ItemType, SearchError> {
    ItemType::ALL
        .iter()
        .copied()
        .find(|t| t.entity_type() == value)
        .ok_or_else(|| {
            SearchError::Database(DieselError::DeserializationError(Box::new(
                UnknownEnumValue {
                    enum_name: "ItemType",
                    value: value.to_string(),
                },
            )))
        })
}

/// Which entity types can match the structural filter at all (module docs,
/// step 5): the `entity_type` restriction intersected with the types that
/// carry every filtered attribute. Types ruled out here get no arm in the
/// ranking query.
fn applicable_types(filter: Option<&SearchFilter>) -> Vec<ItemType> {
    let mut types: Vec<ItemType> = match filter.and_then(|f| f.entity_type.as_ref()) {
        Some(requested) => {
            let set: HashSet<ItemType> = requested.iter().map(|t| t.item_type()).collect();
            ItemType::ALL
                .iter()
                .copied()
                .filter(|t| set.contains(t))
                .collect()
        }
        None => ItemType::ALL.to_vec(),
    };
    if let Some(filter) = filter {
        if filter.board_id.is_some() || filter.column_id.is_some() {
            // Documents do not live on boards.
            types.retain(|t| *t != ItemType::Document);
        }
        if filter.repository_id.is_some() {
            // COLLIERY-T-0269: a task links to a repository, and a
            // document or an ADR impacts one. A strategy and an
            // initiative have no link to a repository.
            types.retain(|t| matches!(t, ItemType::Task | ItemType::Document | ItemType::Adr));
        }
        if filter.team_id.is_some() || filter.task_type.is_some() || filter.work_class.is_some() {
            // team_id, task_type, and work_class are task-level
            // attributes.
            types.retain(|t| *t == ItemType::Task);
        }
        if filter.is_bucket.is_some() {
            // is_bucket is an initiative-level attribute.
            types.retain(|t| *t == ItemType::Initiative);
        }
        if filter.claimed_by.is_some() {
            // KAIROS-T-0488: only a task has a claim.
            types.retain(|t| *t == ItemType::Task);
        }
        if filter.owned_by.is_some() {
            // KAIROS-T-0488: only a strategy and an initiative have an
            // owner.
            types.retain(|t| matches!(t, ItemType::Strategy | ItemType::Initiative));
        }
    }
    types
}

// ---------------------------------------------------------------------------
// Step 5: the ranking query (sanctioned raw SQL, A-0009)
// ---------------------------------------------------------------------------

/// The stored counterpart of a [`SearchTaskType`].
fn model_task_type(task_type: SearchTaskType) -> TaskType {
    match task_type {
        SearchTaskType::Task => TaskType::Task,
        SearchTaskType::Bug => TaskType::Bug,
        SearchTaskType::TechDebt => TaskType::TechDebt,
        SearchTaskType::Support => TaskType::Support,
    }
}

/// The stored counterpart of a [`SearchWorkClass`] (KAIROS-T-0077).
fn model_work_class(work_class: SearchWorkClass) -> WorkClass {
    match work_class {
        SearchWorkClass::Planned => WorkClass::Planned,
        SearchWorkClass::Support => WorkClass::Support,
    }
}

/// The weighted tsvector of an item: the expression of the `idx_*_tsv`
/// GIN indexes and of `searchable_items` (migration 2026-09-23-000004),
/// character for character, so that the planner serves `@@` from the
/// index.
const TSV: &str = "(setweight(to_tsvector('english', coalesce(title, '')), 'A') \
                   || setweight(to_tsvector('english', coalesce(content, '')), 'B'))";

/// The text part of a search: `q` and the ids of the item whose code is
/// `q` ([`code_match_ids`]).
struct TextMatch {
    q: String,
    codes: Vec<Uuid>,
}

/// One bind of the ranking query, in the order of its placeholders.
#[derive(Clone)]
enum Bind {
    Uuid(Uuid),
    Uuids(Vec<Uuid>),
    Text(String),
    Texts(Vec<String>),
    Time(DateTime<Utc>),
    Bool(bool),
    Int(i64),
}

/// The binds of a query being written; [`Binds::push`] gives the
/// placeholder of the bind.
#[derive(Default)]
struct Binds(Vec<Bind>);

impl Binds {
    fn push(&mut self, bind: Bind) -> String {
        self.0.push(bind);
        format!("${}", self.0.len())
    }
}

/// The columns and filters that a type has. A filter that a type does not
/// have has already ruled the type out ([`applicable_types`]).
struct Arm {
    item_type: ItemType,
    table: &'static str,
    on_board: bool,
    task: bool,
    initiative: bool,
    owned: bool,
    impacts: bool,
}

impl Arm {
    fn of(item_type: ItemType) -> Self {
        let arm = |table, on_board, owned, impacts| Arm {
            item_type,
            table,
            on_board,
            task: item_type == ItemType::Task,
            initiative: item_type == ItemType::Initiative,
            owned,
            impacts,
        };
        match item_type {
            ItemType::Strategy => arm("strategies", true, true, false),
            ItemType::Initiative => arm("initiatives", true, true, false),
            ItemType::Task => arm("tasks", true, false, false),
            ItemType::Document => arm("documents", false, false, true),
            ItemType::Adr => arm("adrs", true, false, true),
        }
    }
}

/// One row of the page: the id, the type, and the count of every row.
#[derive(QueryableByName)]
struct PageRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Text)]
    entity_type: String,
    #[diesel(sql_type = BigInt)]
    total: i64,
}

/// The ranking query of a search: one `UNION ALL` arm for each type that
/// can match, each arm with every filter of its type, projecting the id,
/// the type, the short code and the sort key; then `ORDER BY`, `LIMIT`,
/// `OFFSET` and `count(*) OVER ()` (module docs, step 5).
struct Ranking<'a> {
    types: &'a [ItemType],
    filter: Option<&'a SearchFilter>,
    include_deleted: bool,
    /// The traverse and metadata ids, intersected; `None` when neither
    /// was asked.
    candidates: Option<Vec<Uuid>>,
    text: Option<TextMatch>,
    /// The slugs of `metadata_missing`, each a known definition.
    missing: Option<Vec<String>>,
    sort: Sort,
}

impl Ranking<'_> {
    /// The SQL and its binds, without `LIMIT` and `OFFSET`.
    fn sql(&self) -> (String, Binds) {
        let mut binds = Binds::default();
        // Each value is bound once; every arm uses the placeholder.
        let candidates = self
            .candidates
            .as_ref()
            .map(|ids| binds.push(Bind::Uuids(ids.clone())));
        let text = self.text.as_ref().map(|text| {
            let q = binds.push(Bind::Text(text.q.clone()));
            let codes =
                (!text.codes.is_empty()).then(|| binds.push(Bind::Uuids(text.codes.clone())));
            (q, codes)
        });
        let missing = self
            .missing
            .as_ref()
            .map(|slugs| binds.push(Bind::Texts(slugs.clone())));
        let filter = self.filter;
        let mut push_opt = |bind: Option<Bind>| bind.map(|b| binds.push(b));
        let board = push_opt(filter.and_then(|f| f.board_id).map(Bind::Uuid));
        let column = push_opt(filter.and_then(|f| f.column_id).map(Bind::Uuid));
        let team = push_opt(filter.and_then(|f| f.team_id).map(Bind::Uuid));
        let repository = push_opt(filter.and_then(|f| f.repository_id).map(Bind::Uuid));
        let impact_kind = repository
            .as_ref()
            .and_then(|_| push_opt(Some(Bind::Text(crate::impacts::TARGET_REPOSITORY.into()))));
        let task_types = push_opt(filter.and_then(|f| f.task_type.as_ref()).map(|types| {
            Bind::Texts(
                types
                    .iter()
                    .map(|t| model_task_type(*t).as_str().to_string())
                    .collect(),
            )
        }));
        let work_classes = push_opt(filter.and_then(|f| f.work_class.as_ref()).map(|classes| {
            Bind::Texts(
                classes
                    .iter()
                    .map(|c| model_work_class(*c).as_str().to_string())
                    .collect(),
            )
        }));
        let is_bucket = push_opt(filter.and_then(|f| f.is_bucket).map(Bind::Bool));
        let created_after = push_opt(filter.and_then(|f| f.created_after).map(Bind::Time));
        let created_before = push_opt(filter.and_then(|f| f.created_before).map(Bind::Time));
        let updated_after = push_opt(filter.and_then(|f| f.updated_after).map(Bind::Time));
        let updated_before = push_opt(filter.and_then(|f| f.updated_before).map(Bind::Time));
        let created_by = push_opt(filter.and_then(|f| f.created_by).map(Bind::Uuid));
        let claimed_by = push_opt(filter.and_then(|f| f.claimed_by).map(Bind::Uuid));
        let owned_by = push_opt(filter.and_then(|f| f.owned_by).map(Bind::Uuid));

        let arms: Vec<String> = self
            .types
            .iter()
            .map(|item_type| {
                let arm = Arm::of(*item_type);
                let mut wheres: Vec<String> = Vec::new();
                if !self.include_deleted {
                    wheres.push("deleted_at IS NULL".into());
                }
                if let Some(ids) = &candidates {
                    wheres.push(format!("id = ANY({ids})"));
                }
                if let Some((q, codes)) = &text {
                    let matches = format!("{TSV} @@ websearch_to_tsquery('english', {q})");
                    wheres.push(match codes {
                        Some(codes) => format!("({matches} OR id = ANY({codes}))"),
                        None => matches,
                    });
                }
                if let Some(slugs) = &missing {
                    wheres.push(format!(
                        "NOT EXISTS (SELECT 1 FROM item_metadata im \
                         JOIN metadata_definitions md ON md.id = im.metadata_definition_id \
                         WHERE im.item_id = x.id AND md.slug = ANY({slugs}) \
                         AND btrim(im.value) <> '')"
                    ));
                }
                if arm.on_board {
                    if let Some(p) = &board {
                        wheres.push(format!("board_id = {p}"));
                    }
                    if let Some(p) = &column {
                        wheres.push(format!("column_id = {p}"));
                    }
                }
                if arm.task {
                    if let Some(p) = &team {
                        wheres.push(format!("team_id = {p}"));
                    }
                    if let Some(p) = &repository {
                        wheres.push(format!("repository_id = {p}"));
                    }
                    if let Some(p) = &task_types {
                        wheres.push(format!("task_type = ANY({p})"));
                    }
                    if let Some(p) = &work_classes {
                        wheres.push(format!("work_class = ANY({p})"));
                    }
                    if let Some(p) = &claimed_by {
                        wheres.push(format!(
                            "id IN (SELECT task_id FROM task_claims WHERE user_id = {p})"
                        ));
                    }
                }
                if arm.impacts
                    && let (Some(p), Some(kind)) = (&repository, &impact_kind)
                {
                    // COLLIERY-T-0269: the documents and the ADRs that
                    // impact the repository.
                    wheres.push(format!(
                        "id IN (SELECT item_id FROM item_impacts \
                         WHERE target_kind = {kind} AND target_id = {p})"
                    ));
                }
                if arm.initiative
                    && let Some(p) = &is_bucket
                {
                    wheres.push(format!("is_bucket = {p}"));
                }
                if arm.owned
                    && let Some(p) = &owned_by
                {
                    wheres.push(format!(
                        "id IN (SELECT item_id FROM item_owners WHERE user_id = {p})"
                    ));
                }
                for (bound, sql) in [
                    (&created_after, "created_at >"),
                    (&created_before, "created_at <"),
                    (&updated_after, "updated_at >"),
                    (&updated_before, "updated_at <"),
                    (&created_by, "created_by ="),
                ] {
                    if let Some(p) = bound {
                        wheres.push(format!("{sql} {p}"));
                    }
                }
                let key = match self.sort.field {
                    SortField::CreatedAt => "created_at".to_string(),
                    SortField::UpdatedAt => "updated_at".to_string(),
                    SortField::Title => "title".to_string(),
                    // Validation refuses relevance without `q`.
                    SortField::Relevance => match &text {
                        Some((q, codes)) => {
                            let rank =
                                format!("ts_rank_cd({TSV}, websearch_to_tsquery('english', {q}))");
                            match codes {
                                Some(codes) => format!(
                                    "CASE WHEN id = ANY({codes}) \
                                     THEN CAST({CODE_MATCH_SCORE} AS real) ELSE {rank} END"
                                ),
                                None => rank,
                            }
                        }
                        None => "CAST(0 AS real)".to_string(),
                    },
                };
                let wheres = if wheres.is_empty() {
                    String::new()
                } else {
                    format!(" WHERE {}", wheres.join(" AND "))
                };
                format!(
                    "SELECT id, '{}'::text AS entity_type, short_code, {key} AS sort_key \
                     FROM {} x{wheres}",
                    arm.item_type.entity_type(),
                    arm.table,
                )
            })
            .collect();

        // The order of a Rust `str` is the byte order: `COLLATE "C"`.
        let key = match self.sort.field {
            SortField::Title => "sort_key COLLATE \"C\"",
            _ => "sort_key",
        };
        let direction = match self.sort.order {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        };
        let sql = format!(
            "WITH hits AS ({}) \
             SELECT id, entity_type, count(*) OVER () AS total FROM hits \
             ORDER BY {key} {direction}, short_code COLLATE \"C\" ASC, id ASC",
            arms.join(" UNION ALL ")
        );
        (sql, binds)
    }

    /// The rows of one page, in order.
    fn page(
        &self,
        conn: &mut PgConnection,
        limit: i64,
        offset: i64,
        stats: &mut SearchStats,
    ) -> Result<Vec<PageRow>, SearchError> {
        let (sql, mut binds) = self.sql();
        let limit = binds.push(Bind::Int(limit));
        let offset = binds.push(Bind::Int(offset));
        let mut query = sql_query(format!("{sql} LIMIT {limit} OFFSET {offset}")).into_boxed();
        for bind in binds.0 {
            query = match bind {
                Bind::Uuid(v) => query.bind::<SqlUuid, _>(v),
                Bind::Uuids(v) => query.bind::<Array<SqlUuid>, _>(v),
                Bind::Text(v) => query.bind::<Text, _>(v),
                Bind::Texts(v) => query.bind::<Array<Text>, _>(v),
                Bind::Time(v) => query.bind::<Timestamptz, _>(v),
                Bind::Bool(v) => query.bind::<Bool, _>(v),
                Bind::Int(v) => query.bind::<BigInt, _>(v),
            };
        }
        stats.total_queries += 1;
        Ok(query.load(conn)?)
    }
}
