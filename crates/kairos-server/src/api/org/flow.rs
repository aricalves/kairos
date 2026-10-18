//! `GET /api/boards/{id}/flow` (KAIROS-T-0372): the flow metrics of a
//! board for a range of days. The metrics and their definitions are in
//! [`kairos_db::flow`]; this module reads the range, and gives the DTO to
//! REST, MCP (`board_flow`) and the CLI (`kairos boards flow`).
//!
//! Each member of the organization can read a board, so each member can
//! read its flow.
//!
//! # The cache (KAIROS-T-0627)
//!
//! The result is kept in the memory of the server, by board, range and
//! [`flow::FlowBoard::stamp`], for [`CACHE_TTL`] at most. The stamp changes
//! with each move, each new or archived item, each title and each column of
//! the board, so a read after a transition computes again on each server,
//! and no write drops an entry. A read reads the columns, the items and the
//! cancel marks each time (5 queries); a hit does not read the activity log.
//! The age of the work in progress is set at each read.

use axum::extract::{Extension, Path, State};
use axum::routing::get;
use axum::{Json, Router};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Days, NaiveDate, SecondsFormat, Utc};
use diesel::pg::PgConnection;
use kairos_client::types_org as dto;
use kairos_db::flow::{self, Flow, FlowInput, MAX_RANGE_DAYS};
use uuid::Uuid;

use super::load_board_by_ref;
use crate::app::AppState;
use crate::error::ApiError;
use crate::input::ApiQuery;
use crate::middleware::tenant::TenantContext;

/// The days of the default range (today and the 89 days before it).
pub const DEFAULT_RANGE_DAYS: u64 = 90;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/api/boards/{id}/flow", get(board_flow))
}

/// A date of the query, or a refusal that names the parameter.
fn parse_day(raw: &str, parameter: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|_| {
        ApiError::validation(format!(
            "The value {raw:?} of {parameter} is not a date. Send a date as YYYY-MM-DD, for \
             example 2026-10-01."
        ))
        .with_details(serde_json::json!({ "argument": parameter }))
    })
}

/// The range of the query (module docs of [`dto::BoardFlowQuery`]), at
/// `today`. Pure, host-tested.
pub fn resolve_range(
    from: Option<&str>,
    to: Option<&str>,
    today: NaiveDate,
) -> Result<(NaiveDate, NaiveDate), ApiError> {
    let to = match to {
        Some(raw) => parse_day(raw, "to")?,
        None => today,
    };
    let from = match from {
        Some(raw) => parse_day(raw, "from")?,
        None => to - Days::new(DEFAULT_RANGE_DAYS - 1),
    };
    if from > to {
        return Err(ApiError::validation(format!(
            "The range starts on {from} and ends on {to}. Send a from that is on or before to."
        ))
        .with_details(serde_json::json!({ "argument": "from" })));
    }
    let days = (to - from).num_days() + 1;
    if days > MAX_RANGE_DAYS {
        return Err(ApiError::validation(format!(
            "The range has {days} days. A range has {MAX_RANGE_DAYS} days at most. Send a \
             shorter range."
        ))
        .with_details(serde_json::json!({ "argument": "from" })));
    }
    Ok((from, to))
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Two decimals: the metrics are days, and a second is not news.
fn round(days: f64) -> f64 {
    (days * 100.0).round() / 100.0
}

/// The DTO of the metrics.
pub fn to_dto(board_id: String, board_slug: String, flow: &Flow) -> dto::BoardFlow {
    let mut sorted: Vec<f64> = flow.cycle_times.iter().map(|c| c.days).collect();
    sorted.sort_by(f64::total_cmp);
    let mean = (!sorted.is_empty()).then(|| sorted.iter().sum::<f64>() / sorted.len() as f64);
    dto::BoardFlow {
        board_id,
        board_slug,
        from: flow.from.to_string(),
        to: flow.to.to_string(),
        start_columns: flow.start_columns.clone(),
        done_columns: flow.done_columns.clone(),
        done_count: flow.done_count(),
        cancelled_count: flow.cancelled_count(),
        cycle_time: dto::FlowCycleTime {
            count: sorted.len() as i64,
            mean_days: mean.map(round),
            median_days: flow::percentile(&sorted, 50.0).map(round),
            p85_days: flow::percentile(&sorted, 85.0).map(round),
            items: flow
                .cycle_times
                .iter()
                .map(|c| dto::FlowCycleTimeItem {
                    short_code: c.short_code.clone(),
                    title: c.title.clone(),
                    started_at: rfc3339(c.started_at),
                    done_at: rfc3339(c.done_at),
                    days: round(c.days),
                })
                .collect(),
        },
        done_without_start: flow.done_without_start,
        throughput: flow
            .throughput
            .iter()
            .map(|w| dto::FlowWeek {
                week_start: w.week_start.to_string(),
                done: w.done,
                cancelled: w.cancelled,
            })
            .collect(),
        ageing: flow
            .ageing
            .iter()
            .map(|a| dto::FlowAgeingItem {
                short_code: a.short_code.clone(),
                title: a.title.clone(),
                column: a.column.clone(),
                started_at: rfc3339(a.started_at),
                age_days: round(a.age_days),
            })
            .collect(),
        cumulative_flow: dto::CumulativeFlow {
            columns: flow.cfd_columns.clone(),
            days: flow
                .cfd
                .iter()
                .map(|d| dto::CumulativeFlowDay {
                    date: d.date.to_string(),
                    counts: d.counts.clone(),
                })
                .collect(),
        },
    }
}

/// The longest time that a result stays in the cache (module docs).
pub const CACHE_TTL: Duration = Duration::from_secs(300);

/// The most results in the cache. A full cache drops its old entries,
/// then everything.
pub const CACHE_CAPACITY: usize = 256;

/// The key of a result: the board, the range, and the stamp of the board.
type CacheKey = (Uuid, NaiveDate, NaiveDate, u64);

static CACHE: LazyLock<Mutex<HashMap<CacheKey, (Instant, Flow)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached(key: &CacheKey) -> Option<Flow> {
    let mut cache = CACHE.lock().expect("the flow cache lock");
    match cache.get(key) {
        Some((at, flow)) if at.elapsed() < CACHE_TTL => Some(flow.clone()),
        Some(_) => {
            cache.remove(key);
            None
        }
        None => None,
    }
}

fn keep(key: CacheKey, flow: Flow) {
    let mut cache = CACHE.lock().expect("the flow cache lock");
    if cache.len() >= CACHE_CAPACITY && !cache.contains_key(&key) {
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_CAPACITY {
            cache.clear();
        }
    }
    cache.insert(key, (Instant::now(), flow));
}

/// The metrics of the board `board_id` for the days `from..=to` at `now`,
/// from the cache when the board has not changed (module docs).
pub fn flow_of_board(
    conn: &mut PgConnection,
    board_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Flow, ApiError> {
    let board =
        flow::load_board(conn, board_id, flow::start_of(from)).map_err(ApiError::internal)?;
    let key = (board_id, from, to, board.stamp);
    if let Some(mut flow) = cached(&key) {
        flow.age_at(now);
        return Ok(flow);
    }
    let moves = flow::load_moves(conn, board_id, &board, flow::start_of(from))
        .map_err(ApiError::internal)?;
    let flow = flow::compute(&FlowInput::new(board, moves), from, to, now);
    keep(key, flow.clone());
    Ok(flow)
}

/// The flow of the board `reference` (slug or UUID) for the range; the
/// shared body of REST and MCP.
pub fn board_flow_of(
    conn: &mut PgConnection,
    reference: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<dto::BoardFlow, ApiError> {
    let now = Utc::now();
    let (from, to) = resolve_range(from, to, now.date_naive())?;
    let board = load_board_by_ref(conn, reference)?;
    let flow = flow_of_board(conn, board.id, from, to, now)?;
    Ok(to_dto(board.id.to_string(), board.slug, &flow))
}

/// The flow metrics of a board for a range of days (KAIROS-T-0372). They are
/// the cycle time, the throughput per week, the work in progress and the
/// cumulative flow. A cancelled task counts apart
/// from the done items. The path takes the slug or the id of the board.
#[utoipa::path(
    get,
    path = "/api/boards/{id}/flow",
    tag = "boards",
    params(("id" = String, Path, description = "The slug or the id (UUID) of the board"), dto::BoardFlowQuery),
    responses(
        (status = 200, description = "The flow metrics of the range", body = dto::BoardFlow),
        (status = 404, description = "Unknown board", body = kairos_client::types::ErrorEnvelope),
        (status = 422, description = "VALIDATION: a date that is not YYYY-MM-DD, from after to, or more than 366 days", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn board_flow(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantContext>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<dto::BoardFlowQuery>,
) -> Result<Json<dto::BoardFlow>, ApiError> {
    let flow = state
        .blocking
        .run(&tenant.slug, move |conn| {
            board_flow_of(conn, &id, query.from.as_deref(), query.to.as_deref())
        })
        .await?;
    Ok(Json(flow))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        text.parse().expect("a date")
    }

    #[test]
    fn the_default_range_is_90_days_to_today() {
        let (from, to) = resolve_range(None, None, day("2026-10-11")).expect("range");
        assert_eq!((from, to), (day("2026-07-14"), day("2026-10-11")));
        assert_eq!((to - from).num_days() + 1, 90);
        let (from, to) = resolve_range(None, Some("2026-10-01"), day("2026-10-11")).expect("range");
        assert_eq!((from, to), (day("2026-07-04"), day("2026-10-01")));
    }

    #[test]
    fn a_bad_range_is_refused() {
        let today = day("2026-10-11");
        assert!(resolve_range(Some("2026-10-12"), Some("2026-10-11"), today).is_err());
        assert!(resolve_range(Some("2025-01-01"), Some("2026-10-11"), today).is_err());
        assert!(resolve_range(Some("10/01/2026"), None, today).is_err());
        assert!(resolve_range(Some("2026-10-11"), Some("2026-10-11"), today).is_ok());
    }

    #[test]
    fn the_numbers_have_two_decimals() {
        assert_eq!(round(2.0 / 3.0), 0.67);
        assert_eq!(round(2.0), 2.0);
    }
}
