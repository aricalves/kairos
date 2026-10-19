//! One page of the cards of a board (COLLIERY-T-0261).
//!
//! The cards of a board are in four tables (`strategies`, `initiatives`,
//! `tasks`, `adrs`). A page of the board needs ONE order for the cards of
//! the four tables, so this module reads the ids of a page with one
//! `UNION ALL` query (KAIROS-A-0009: raw SQL is the sanctioned escape
//! hatch). The caller then loads the rows of each table by id.
//!
//! # The order
//!
//! 1. the position of the column, then the id of the column (a removed
//!    column can have the position of a live column),
//! 2. the type of the card: strategy, initiative, task, ADR,
//! 3. the short code of the card, then the id of the card.
//!
//! The order is total, so two pages of one board have no card in common
//! and each card is on one page. It is the order that the route had
//! before it had pages: the columns by position, and the cards of each
//! type by short code.
//!
//! # The time in the column and the notes (KAIROS-T-0649)
//!
//! The page query also gives the time that each card of the page entered
//! its column, and the number of its notes. The time is that of the last
//! move of the card in `activity_log` (a `transition`, a `cancel` or a
//! `board_move`), or the time of its creation. With
//! [`BoardItemFilter::done_limit`], a done column keeps only its last
//! cards by that time.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use diesel::sql_query;
use diesel::sql_types::{
    Array, BigInt, Bool, Integer, Nullable, SmallInt, Text, Timestamptz, Uuid as SqlUuid,
};
use uuid::Uuid;

/// The type of a card, in the order of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardKind {
    Strategy,
    Initiative,
    Task,
    Adr,
}

impl CardKind {
    fn from_rank(rank: i16) -> Option<Self> {
        match rank {
            0 => Some(Self::Strategy),
            1 => Some(Self::Initiative),
            2 => Some(Self::Task),
            3 => Some(Self::Adr),
            _ => None,
        }
    }
}

/// Which cards of the board are in the list. Each filter applies before
/// the page, and [`BoardItemPage::total`] is the count after the filters.
#[derive(Debug, Clone)]
pub struct BoardItemFilter<'a> {
    pub board_id: Uuid,
    /// The columns that the response has. A card in a different column is
    /// not in the list.
    pub column_ids: &'a [Uuid],
    /// Include the archived cards (KAIROS-T-0159).
    pub include_archived: bool,
    /// Narrow the TASKS to those of this repository (KAIROS-T-0104). The
    /// other types are unaffected.
    pub repository_id: Option<Uuid>,
    /// Narrow the STRATEGIES and the INITIATIVES by their teams
    /// (KAIROS-T-0321, [`crate::item_teams`]). The tasks and the ADRs are
    /// unaffected.
    pub team: Option<TeamFilter>,
    /// Keep only the last N cards of each done column, by the time that
    /// each card entered the column (KAIROS-T-0649). `None`: each card.
    /// The other columns are unaffected.
    pub done_limit: Option<i64>,
}

/// The team filter of a board (KAIROS-T-0321).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamFilter {
    /// The items that have this team, from tasks or set by hand.
    Team(Uuid),
    /// The items that have no team.
    NoTeam,
}

/// One page of the cards of a board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardItemPage {
    /// The number of cards after the filters, on all pages.
    pub total: i64,
    /// The cards of the page, in the order of the page.
    pub cards: Vec<(CardKind, Uuid)>,
    /// The time in the column and the notes of each card of the page, by
    /// id (KAIROS-T-0649).
    pub marks: std::collections::HashMap<Uuid, CardMarks>,
}

/// What the page query gives for each card of the page (KAIROS-T-0649).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardMarks {
    /// The time that the card entered its column: its last move, or its
    /// creation.
    pub entered_column_at: chrono::DateTime<chrono::Utc>,
    /// The number of the notes of the item (`item_notes`).
    pub notes_count: i64,
}

#[derive(QueryableByName)]
struct CardRow {
    #[diesel(sql_type = SmallInt)]
    kind: i16,
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Timestamptz)]
    entered_at: chrono::DateTime<chrono::Utc>,
    #[diesel(sql_type = BigInt)]
    notes_count: i64,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

/// The cards of the four tables that pass the filters. `$1` is the board,
/// `$2` is "include the archived cards", `$3` is the repository of the
/// tasks (or NULL), and `$4` is the list of the columns. `$5` is a list of
/// strategies and initiatives (or NULL for no team filter), and `$6` says
/// if the filter keeps the items of the list (`true`) or the other items
/// (`false`).
const CARDS_SQL: &str = "\
    SELECT card.kind, card.id, card.short_code, card.created_at, c.position, \
           c.is_done, c.id AS column_id \
    FROM ( \
        SELECT 0::smallint AS kind, id, short_code, column_id, created_at FROM strategies \
         WHERE board_id = $1 AND ($2 OR deleted_at IS NULL) \
           AND ($5::uuid[] IS NULL OR (id = ANY($5)) = $6) \
        UNION ALL \
        SELECT 1::smallint, id, short_code, column_id, created_at FROM initiatives \
         WHERE board_id = $1 AND ($2 OR deleted_at IS NULL) \
           AND ($5::uuid[] IS NULL OR (id = ANY($5)) = $6) \
        UNION ALL \
        SELECT 2::smallint, id, short_code, column_id, created_at FROM tasks \
         WHERE board_id = $1 AND ($2 OR deleted_at IS NULL) \
           AND ($3::uuid IS NULL OR repository_id = $3) \
        UNION ALL \
        SELECT 3::smallint, id, short_code, column_id, created_at FROM adrs \
         WHERE board_id = $1 AND ($2 OR deleted_at IS NULL) \
    ) card \
    JOIN board_columns c ON c.id = card.column_id \
    WHERE card.column_id = ANY($4)";

/// The time that the row `alias` (with `id` and `created_at`) entered its
/// column, as SQL: the time of its last move in `activity_log`, or its
/// creation (KAIROS-T-0649). The index `(entity_id, occurred_at)` serves
/// it.
fn entered_sql(alias: &str) -> String {
    format!(
        "COALESCE((SELECT max(a.occurred_at) FROM activity_log a \
          WHERE a.entity_id = {alias}.id \
            AND a.action IN ('transition', 'cancel', 'board_move')), \
         {alias}.created_at)"
    )
}

/// The cards of [`CARDS_SQL`] after the done limit (`$8`) and the code
/// filter (`$7`). A card of a done column has its rank by the time of
/// entry when `$8` is set; the time is read for those cards only. The code
/// filter comes after the limit: a card that the limit cuts is not in the
/// page of its code.
fn filtered_sql() -> String {
    let entered = entered_sql("cards");
    format!(
        "SELECT * FROM ( \
           SELECT cards.*, \
                  CASE WHEN cards.is_done AND $8::bigint IS NOT NULL THEN \
                    ROW_NUMBER() OVER ( \
                      PARTITION BY cards.column_id \
                      ORDER BY CASE WHEN cards.is_done AND $8::bigint IS NOT NULL \
                                    THEN {entered} END DESC NULLS LAST, cards.id DESC) \
                  END AS done_rank \
           FROM ({CARDS_SQL}) cards \
         ) ranked \
         WHERE (ranked.done_rank IS NULL OR ranked.done_rank <= $8) \
           AND ($7::text IS NULL OR ranked.short_code = $7)"
    )
}

/// One page of the cards of a board: 2 queries (the count and the ids of
/// the page), for a board of each size, and 1 more with a team filter.
pub fn board_item_page(
    conn: &mut PgConnection,
    filter: &BoardItemFilter<'_>,
    limit: i64,
    offset: i64,
) -> Result<BoardItemPage, DieselError> {
    board_item_page_of_code(conn, filter, None, limit, offset)
}

/// [`board_item_page`], narrowed to the card with the short code `code`
/// when it is `Some` (KAIROS-T-0615). The page then has that one card, or
/// no card when no card of the board has the code and passes the filters.
/// A live board patches one card with it after an event of that item.
pub fn board_item_page_of_code(
    conn: &mut PgConnection,
    filter: &BoardItemFilter<'_>,
    code: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<BoardItemPage, DieselError> {
    // The team filter: the items of the board that have the team (or any
    // team), then keep them or the others. One query for the board.
    let (team_items, keep): (Option<Vec<Uuid>>, bool) = match filter.team {
        None => (None, true),
        Some(team) => {
            let teams = crate::item_teams::board_teams(conn, filter.board_id)?;
            let with = |wanted: &dyn Fn(&crate::item_teams::ItemTeam) -> bool| -> Vec<Uuid> {
                teams
                    .iter()
                    .filter(|(_, item_teams)| item_teams.iter().any(wanted))
                    .map(|(id, _)| *id)
                    .collect()
            };
            match team {
                TeamFilter::Team(team_id) => (Some(with(&|t| t.team_id == team_id)), true),
                TeamFilter::NoTeam => (Some(with(&|_| true)), false),
            }
        }
    };
    // `$7` is the short code of one card, or NULL for each card. `$8` is
    // the done limit, or NULL.
    let filtered = filtered_sql();
    let total = sql_query(format!("SELECT COUNT(*) AS count FROM ({filtered}) f"))
        .bind::<SqlUuid, _>(filter.board_id)
        .bind::<Bool, _>(filter.include_archived)
        .bind::<Nullable<SqlUuid>, _>(filter.repository_id)
        .bind::<Array<SqlUuid>, _>(filter.column_ids)
        .bind::<Nullable<Array<SqlUuid>>, _>(team_items.as_deref())
        .bind::<Bool, _>(keep)
        .bind::<Nullable<Text>, _>(code)
        .bind::<Nullable<BigInt>, _>(filter.done_limit)
        .get_result::<CountRow>(conn)?
        .count;
    // The time in the column and the notes count of the cards of the page
    // only: the outer query reads them after the limit.
    let rows: Vec<CardRow> = sql_query(format!(
        "SELECT p.kind, p.id, {entered} AS entered_at, \
                (SELECT COUNT(*) FROM item_notes n WHERE n.item_id = p.id) AS notes_count \
         FROM ( \
           SELECT kind, id, created_at, position, column_id, short_code FROM ({filtered}) f \
           ORDER BY position, column_id, kind, short_code, id \
           LIMIT $9 OFFSET $10 \
         ) p \
         ORDER BY p.position, p.column_id, p.kind, p.short_code, p.id",
        entered = entered_sql("p"),
    ))
    .bind::<SqlUuid, _>(filter.board_id)
    .bind::<Bool, _>(filter.include_archived)
    .bind::<Nullable<SqlUuid>, _>(filter.repository_id)
    .bind::<Array<SqlUuid>, _>(filter.column_ids)
    .bind::<Nullable<Array<SqlUuid>>, _>(team_items.as_deref())
    .bind::<Bool, _>(keep)
    .bind::<Nullable<Text>, _>(code)
    .bind::<Nullable<BigInt>, _>(filter.done_limit)
    .bind::<BigInt, _>(limit)
    .bind::<BigInt, _>(offset)
    .load(conn)?;
    let marks = rows
        .iter()
        .map(|row| {
            (
                row.id,
                CardMarks {
                    entered_column_at: row.entered_at,
                    notes_count: row.notes_count,
                },
            )
        })
        .collect();
    Ok(BoardItemPage {
        total,
        cards: rows
            .into_iter()
            .filter_map(|row| CardKind::from_rank(row.kind).map(|kind| (kind, row.id)))
            .collect(),
        marks,
    })
}

#[derive(QueryableByName)]
struct DoneTaskRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Text)]
    short_code: String,
}

/// The live tasks of the done columns of the board `board_id`, as
/// `(id, short_code)` in short-code order: the tasks that "Archive
/// completed" archives (KAIROS-T-0363). With `older_than_days`, only the
/// tasks that entered their column at least that many days ago
/// (KAIROS-T-0649). 1 query.
pub fn done_tasks(
    conn: &mut PgConnection,
    board_id: Uuid,
    older_than_days: Option<i32>,
) -> Result<Vec<(Uuid, String)>, DieselError> {
    let rows: Vec<DoneTaskRow> = sql_query(format!(
        "SELECT card.id, card.short_code FROM tasks card \
         JOIN board_columns c ON c.id = card.column_id \
         WHERE card.board_id = $1 AND card.deleted_at IS NULL AND c.is_done \
           AND ($2::integer IS NULL \
                OR {entered} <= now() - make_interval(days => $2::integer)) \
         ORDER BY card.short_code",
        entered = entered_sql("card"),
    ))
    .bind::<SqlUuid, _>(board_id)
    .bind::<Nullable<Integer>, _>(older_than_days)
    .load(conn)?;
    Ok(rows.into_iter().map(|r| (r.id, r.short_code)).collect())
}

/// One live column of a board, with the number of live cards in it.
#[derive(Debug, Clone, PartialEq, Eq, QueryableByName)]
pub struct ColumnCardCount {
    /// The board of the column.
    #[diesel(sql_type = SqlUuid)]
    pub board_id: Uuid,
    /// The column.
    #[diesel(sql_type = SqlUuid)]
    pub column_id: Uuid,
    /// The name of the column.
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub name: String,
    /// The number of live cards in the column, of the four types.
    #[diesel(sql_type = BigInt)]
    pub count: i64,
    /// The order of the column on its board (KAIROS-T-0554).
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub position: i32,
    /// The column counts as done (KAIROS-T-0554).
    #[diesel(sql_type = diesel::sql_types::Bool)]
    pub is_done: bool,
    /// A move into the column gives a claim (KAIROS-T-0554).
    #[diesel(sql_type = diesel::sql_types::Bool)]
    pub claims: bool,
    /// The WIP limit of the column, when it has one (KAIROS-T-0554).
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Integer>)]
    pub wip_limit: Option<i32>,
}

/// The live columns of the boards `$1`, each with the count of its live
/// cards. The database counts the cards (`GROUP BY`), and no row of a card
/// comes to the server.
const COLUMN_COUNTS_SQL: &str = "\
    SELECT c.board_id, c.id AS column_id, c.name, \
           COALESCE(n.count, 0)::bigint AS count, \
           c.position, c.is_done, c.claims, c.wip_limit \
    FROM board_columns c \
    LEFT JOIN ( \
        SELECT card.column_id, COUNT(*) AS count \
        FROM ( \
            SELECT column_id FROM strategies \
             WHERE board_id = ANY($1) AND deleted_at IS NULL \
            UNION ALL \
            SELECT column_id FROM initiatives \
             WHERE board_id = ANY($1) AND deleted_at IS NULL \
            UNION ALL \
            SELECT column_id FROM tasks \
             WHERE board_id = ANY($1) AND deleted_at IS NULL \
            UNION ALL \
            SELECT column_id FROM adrs \
             WHERE board_id = ANY($1) AND deleted_at IS NULL \
        ) card \
        GROUP BY card.column_id \
    ) n ON n.column_id = c.id \
    WHERE c.board_id = ANY($1) AND c.deleted_at IS NULL \
    ORDER BY c.board_id, c.position, c.id";

/// The live columns of `board_ids` in position order, each with the count
/// of its live cards (COLLIERY-T-0265): 1 query, for each number of boards
/// and for a board of each size. An archived card does not count (ADR-20
/// rule 5), and a removed column is not in the result.
///
/// The MCP tool `my_boards` prints the counts. Before this function it
/// read each row of each board to count them.
pub fn live_column_counts(
    conn: &mut PgConnection,
    board_ids: &[Uuid],
) -> Result<Vec<ColumnCardCount>, DieselError> {
    if board_ids.is_empty() {
        return Ok(Vec::new());
    }
    sql_query(COLUMN_COUNTS_SQL)
        .bind::<Array<SqlUuid>, _>(board_ids)
        .load(conn)
}

/// The load of one column against its WIP limit (KAIROS-T-0371).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnLoad {
    pub name: String,
    /// The live cards in the column, of the four types.
    pub count: i64,
    pub wip_limit: Option<i32>,
}

impl ColumnLoad {
    /// The column has more live cards than its limit.
    pub fn over_limit(&self) -> bool {
        self.wip_limit
            .is_some_and(|limit| self.count > i64::from(limit))
    }
}

/// Is a count over a limit? Pure.
pub fn over_wip_limit(count: i64, wip_limit: Option<i32>) -> bool {
    wip_limit.is_some_and(|limit| count > i64::from(limit))
}

/// The load of the column `column_id`: its name, its live cards and its
/// limit. `None` for an unknown column. 2 queries.
pub fn column_load(
    conn: &mut PgConnection,
    column_id: Uuid,
) -> Result<Option<ColumnLoad>, DieselError> {
    use crate::schema::board_columns;
    let Some((board_id, wip_limit)) = board_columns::table
        .filter(board_columns::id.eq(column_id))
        .select((board_columns::board_id, board_columns::wip_limit))
        .first::<(Uuid, Option<i32>)>(conn)
        .optional()?
    else {
        return Ok(None);
    };
    Ok(live_column_counts(conn, &[board_id])?
        .into_iter()
        .find(|c| c.column_id == column_id)
        .map(|c| ColumnLoad {
            name: c.name,
            count: c.count,
            wip_limit,
        }))
}

/// The metadata values of each item of `ids` (KAIROS-T-0368): one query,
/// keyed by item id, each list as `(definition slug, value)` in slug order.
/// An item with no value has no entry. `board_items` with `details` and
/// `GET /api/boards/{id}/items?with_metadata=true` show them per card, so
/// an agent does not call `get_item` for each card to read its priority.
pub fn metadata_of_items(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<(String, String)>>, DieselError> {
    use crate::schema::{item_metadata as im, metadata_definitions as md};
    let mut out: std::collections::HashMap<Uuid, Vec<(String, String)>> =
        std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(Uuid, String, String)> = im::table
        .inner_join(md::table)
        .filter(im::item_id.eq_any(ids))
        .order((im::item_id.asc(), md::slug.asc()))
        .select((im::item_id, md::slug, im::value))
        .load(conn)?;
    for (id, slug, value) in rows {
        out.entry(id).or_default().push((slug, value));
    }
    Ok(out)
}

/// The transitions of the boards `board_ids`, as `(board, from, to)`
/// column ids, in one query (KAIROS-T-0554). Removed columns are not
/// filtered here: the caller matches them against its live columns.
pub fn transitions_of_boards(
    conn: &mut PgConnection,
    board_ids: &[Uuid],
) -> Result<Vec<(Uuid, Uuid, Uuid)>, DieselError> {
    use crate::schema::board_transitions;
    use diesel::prelude::*;
    if board_ids.is_empty() {
        return Ok(Vec::new());
    }
    board_transitions::table
        .filter(board_transitions::board_id.eq_any(board_ids))
        .select((
            board_transitions::board_id,
            board_transitions::from_column_id,
            board_transitions::to_column_id,
        ))
        .load(conn)
}

// ---------------------------------------------------------------------------
// The rows of the cards (KAIROS-T-0602)
// ---------------------------------------------------------------------------

/// The columns of a strategy card: the row without `content`,
/// `created_by` and `updated_by` (KAIROS-T-0602). The board route reads
/// these when the query does not ask for `fields=full`.
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable)]
#[diesel(table_name = crate::schema::strategies)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct StrategyCardRow {
    pub id: Uuid,
    pub short_code: String,
    pub title: String,
    pub board_id: Uuid,
    pub column_id: Uuid,
    pub hypothesis: Option<String>,
    pub version: i32,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The columns of an initiative card (KAIROS-T-0602).
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable)]
#[diesel(table_name = crate::schema::initiatives)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct InitiativeCardRow {
    pub id: Uuid,
    pub short_code: String,
    pub title: String,
    pub board_id: Uuid,
    pub column_id: Uuid,
    pub complexity: Option<crate::models::Complexity>,
    pub is_bucket: bool,
    pub bucket_type: Option<crate::models::BucketType>,
    pub version: i32,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The columns of a task card (KAIROS-T-0602).
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable)]
#[diesel(table_name = crate::schema::tasks)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct TaskCardRow {
    pub id: Uuid,
    pub short_code: String,
    pub title: String,
    pub board_id: Uuid,
    pub column_id: Uuid,
    pub task_type: crate::models::TaskType,
    pub work_class: crate::models::WorkClass,
    pub team_id: Option<Uuid>,
    pub repository_id: Option<Uuid>,
    pub version: i32,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The columns of an ADR card (KAIROS-T-0602).
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable)]
#[diesel(table_name = crate::schema::adrs)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct AdrCardRow {
    pub id: Uuid,
    pub short_code: String,
    pub title: String,
    pub board_id: Option<Uuid>,
    pub column_id: Option<Uuid>,
    pub decision_maker: Option<String>,
    pub decision_date: Option<chrono::NaiveDate>,
    pub version: i32,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
