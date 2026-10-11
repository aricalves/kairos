//! Flow metrics of a board (KAIROS-T-0372): cycle time, throughput per
//! week, the age of the work in progress, and the cumulative flow by
//! column per day. The source is the `activity_log`: each move of an item
//! writes a row.
//!
//! # The rows that move an item
//!
//! - `transition`, details `column:<from name>-><to name>`;
//! - `cancel`, details `column:<from name>-><to name> reason:<text>` (the
//!   cancel moves a task to the done column, KAIROS-T-0362);
//! - `board_move`, details `board:<from id>-><to id> column:<from
//!   id>-><to id>` (a task that comes to the board, or leaves it).
//!
//! A `transition` row names the columns by their name at the time of the
//! move. A column that has a different name now keeps its old name in the
//! history: the cumulative flow shows the old name as a column of its own.
//!
//! # The definitions
//!
//! - **Start**: the first entry of the item into a START column. The start
//!   columns are the columns with `claims` (the Active column of a delivery
//!   board, KAIROS-T-0359). A board with no such column uses the column
//!   with the name "Active", in any case. A board with neither has no start,
//!   and no cycle time.
//! - **Done**: the item is in a column with `is_done` now; the done time is
//!   its last entry into a done column.
//! - **Cancelled**: a task with the cancel mark (`task_cancellations`). It
//!   counts apart: not in the done throughput, and not in the cycle time.
//! - **Cycle time**: from the start to the done time, in days, for each
//!   done item (not cancelled) whose done time is in the range and that has
//!   a start.
//! - **Throughput**: the done items and the cancelled tasks of each week
//!   (weeks start on Monday, UTC) of the range, by the done time and the
//!   time of the cancel.
//! - **Work in progress**: each item that is not in a done column and has
//!   a start. Its age is the time from its start to now.
//! - **Cumulative flow**: for each day of the range (UTC), the number of
//!   live items in each column at the end of the day.
//!
//! Only the live items of the board count: an archived item, and an item
//! that is on a different board now, do not.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Datelike, Days, NaiveDate, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use uuid::Uuid;

/// The most days that one range can have.
pub const MAX_RANGE_DAYS: i64 = 366;

/// A column of the board, for the metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowColumn {
    pub id: Uuid,
    pub name: String,
    pub is_done: bool,
    pub claims: bool,
}

/// A live item of the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowItem {
    pub id: Uuid,
    pub short_code: String,
    pub title: String,
    pub column_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// One move of an item: the time and the column after it, by name. `None`
/// for a move that takes the item off the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowMove {
    pub item_id: Uuid,
    pub at: DateTime<Utc>,
    /// The column before the move, by name; `None` when the item came from
    /// a different board.
    pub from: Option<String>,
    /// The column after the move, by name; `None` when the item left the
    /// board.
    pub to: Option<String>,
}

/// What [`compute`] reads.
#[derive(Debug, Clone, Default)]
pub struct FlowInput {
    /// In position order.
    pub columns: Vec<FlowColumn>,
    pub items: Vec<FlowItem>,
    /// In time order.
    pub moves: Vec<FlowMove>,
    /// The cancelled tasks, with the time of the cancel.
    pub cancelled: HashMap<Uuid, DateTime<Utc>>,
}

/// The cycle time of one done item.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleTime {
    pub short_code: String,
    pub title: String,
    pub started_at: DateTime<Utc>,
    pub done_at: DateTime<Utc>,
    pub days: f64,
}

/// The throughput of one week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekThroughput {
    /// The Monday of the week.
    pub week_start: NaiveDate,
    pub done: i64,
    pub cancelled: i64,
}

/// One item in progress.
#[derive(Debug, Clone, PartialEq)]
pub struct AgeingItem {
    pub short_code: String,
    pub title: String,
    pub column: String,
    pub started_at: DateTime<Utc>,
    pub age_days: f64,
}

/// The counts of one day of the cumulative flow, in the order of
/// [`Flow::cfd_columns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowDay {
    pub date: NaiveDate,
    pub counts: Vec<i64>,
}

/// The flow metrics of a board for a range.
#[derive(Debug, Clone, PartialEq)]
pub struct Flow {
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// The names of the start columns; empty when the board has none.
    pub start_columns: Vec<String>,
    pub done_columns: Vec<String>,
    /// Done items in the range (not cancelled), oldest done first.
    pub cycle_times: Vec<CycleTime>,
    /// The done items in the range with no start (no cycle time).
    pub done_without_start: i64,
    pub throughput: Vec<WeekThroughput>,
    /// Oldest first.
    pub ageing: Vec<AgeingItem>,
    /// The columns of the cumulative flow: the columns of the board in
    /// position order, then the old names that the history has.
    pub cfd_columns: Vec<String>,
    pub cfd: Vec<FlowDay>,
}

impl Flow {
    /// The number of done items in the range (not cancelled).
    pub fn done_count(&self) -> i64 {
        self.throughput.iter().map(|w| w.done).sum()
    }

    /// The number of cancelled tasks in the range.
    pub fn cancelled_count(&self) -> i64 {
        self.throughput.iter().map(|w| w.cancelled).sum()
    }
}

/// A sorted percentile (nearest rank). `None` for no value.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// The Monday of the week of `date`.
pub fn week_start(date: NaiveDate) -> NaiveDate {
    date - Days::new(u64::from(date.weekday().num_days_from_monday()))
}

fn days_between(a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    (b - a).num_seconds() as f64 / 86_400.0
}

/// The end of the day `date` (UTC): the first instant of the next day.
fn end_of(date: NaiveDate) -> DateTime<Utc> {
    (date + Days::new(1))
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
}

/// The columns of an item over time: `(time, column name)`; `None` for a
/// time when the item is not on the board.
type Timeline = Vec<(DateTime<Utc>, Option<String>)>;

/// Compute the metrics of `input` for the days `from..=to`, at `now`. Pure.
pub fn compute(input: &FlowInput, from: NaiveDate, to: NaiveDate, now: DateTime<Utc>) -> Flow {
    let name_of: HashMap<Uuid, &str> = input
        .columns
        .iter()
        .map(|c| (c.id, c.name.as_str()))
        .collect();
    let mut start_names: HashSet<String> = input
        .columns
        .iter()
        .filter(|c| c.claims)
        .map(|c| c.name.clone())
        .collect();
    if start_names.is_empty() {
        start_names = input
            .columns
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case("active"))
            .map(|c| c.name.clone())
            .collect();
    }
    let done_names: HashSet<String> = input
        .columns
        .iter()
        .filter(|c| c.is_done)
        .map(|c| c.name.clone())
        .collect();
    let mut moves_of: HashMap<Uuid, Vec<&FlowMove>> = HashMap::new();
    for m in &input.moves {
        moves_of.entry(m.item_id).or_default().push(m);
    }
    for list in moves_of.values_mut() {
        list.sort_by_key(|m| m.at);
    }

    // The timeline of each item on the board: (time, column name or None).
    // Before the first move, the item is in the column that the first move
    // leaves (or, with no move, in its column now), from its creation.
    let mut timelines: Vec<(&FlowItem, Timeline)> = Vec::new();
    for item in &input.items {
        let moves = moves_of.get(&item.id).map(Vec::as_slice).unwrap_or(&[]);
        let current = name_of.get(&item.column_id).map(|s| s.to_string());
        let mut line = Vec::new();
        match moves.first() {
            Some(first) => {
                if first.from.is_some() {
                    line.push((item.created_at, first.from.clone()));
                }
            }
            None => line.push((item.created_at, current.clone())),
        }
        for m in moves {
            line.push((m.at, m.to.clone()));
        }
        timelines.push((item, line));
    }

    let range_start = from.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
    let range_end = end_of(to);
    let in_range = |at: DateTime<Utc>| at >= range_start && at < range_end;

    let mut cycle_times = Vec::new();
    let mut done_without_start = 0;
    let mut weeks: BTreeMap<NaiveDate, (i64, i64)> = BTreeMap::new();
    let mut week = week_start(from);
    while week <= to {
        weeks.insert(week, (0, 0));
        week = week + Days::new(7);
    }
    let mut ageing = Vec::new();
    for (item, line) in &timelines {
        let started_at = line
            .iter()
            .find(|(_, column)| column.as_ref().is_some_and(|c| start_names.contains(c)))
            .map(|(at, _)| *at);
        let current = name_of.get(&item.column_id).map(|s| s.to_string());
        let is_done = current.as_ref().is_some_and(|c| done_names.contains(c));
        if let Some(cancelled_at) = input.cancelled.get(&item.id) {
            if in_range(*cancelled_at)
                && let Some(entry) = weeks.get_mut(&week_start(cancelled_at.date_naive()))
            {
                entry.1 += 1;
            }
            continue;
        }
        if is_done {
            let done_at = line
                .iter()
                .rev()
                .find(|(_, column)| column.as_ref().is_some_and(|c| done_names.contains(c)))
                .map(|(at, _)| *at)
                .unwrap_or(item.created_at);
            if !in_range(done_at) {
                continue;
            }
            if let Some(entry) = weeks.get_mut(&week_start(done_at.date_naive())) {
                entry.0 += 1;
            }
            match started_at {
                Some(started_at) if started_at <= done_at => cycle_times.push(CycleTime {
                    short_code: item.short_code.clone(),
                    title: item.title.clone(),
                    started_at,
                    done_at,
                    days: days_between(started_at, done_at),
                }),
                _ => done_without_start += 1,
            }
        } else if let Some(started_at) = started_at {
            ageing.push(AgeingItem {
                short_code: item.short_code.clone(),
                title: item.title.clone(),
                column: current.unwrap_or_default(),
                started_at,
                age_days: days_between(started_at, now).max(0.0),
            });
        }
    }
    cycle_times.sort_by_key(|c| c.done_at);
    ageing.sort_by(|a, b| b.age_days.total_cmp(&a.age_days));

    // The cumulative flow.
    let mut cfd_columns: Vec<String> = input.columns.iter().map(|c| c.name.clone()).collect();
    for (_, line) in &timelines {
        for (_, column) in line {
            if let Some(column) = column
                && !cfd_columns.contains(column)
            {
                cfd_columns.push(column.clone());
            }
        }
    }
    let index: HashMap<&str, usize> = cfd_columns
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    let mut cfd = Vec::new();
    let mut day = from;
    while day <= to {
        let end = end_of(day);
        let mut counts = vec![0i64; cfd_columns.len()];
        for (_, line) in &timelines {
            let at_end = line
                .iter()
                .take_while(|(at, _)| *at < end)
                .last()
                .and_then(|(_, column)| column.as_deref());
            if let Some(column) = at_end
                && let Some(i) = index.get(column)
            {
                counts[*i] += 1;
            }
        }
        cfd.push(FlowDay { date: day, counts });
        day = day + Days::new(1);
    }

    let mut start_columns: Vec<String> = input
        .columns
        .iter()
        .filter(|c| start_names.contains(&c.name))
        .map(|c| c.name.clone())
        .collect();
    start_columns.dedup();
    Flow {
        from,
        to,
        start_columns,
        done_columns: input
            .columns
            .iter()
            .filter(|c| c.is_done)
            .map(|c| c.name.clone())
            .collect(),
        cycle_times,
        done_without_start,
        throughput: weeks
            .into_iter()
            .map(|(week_start, (done, cancelled))| WeekThroughput {
                week_start,
                done,
                cancelled,
            })
            .collect(),
        ageing,
        cfd_columns,
        cfd,
    }
}

/// `column:<from>-><to>` (and a `reason:` after it) → the two names.
fn parse_column_move(details: &str) -> Option<(String, String)> {
    let rest = details.strip_prefix("column:")?;
    let rest = rest.split(" reason:").next().unwrap_or(rest);
    let (from, to) = rest.split_once("->")?;
    Some((from.to_string(), to.to_string()))
}

/// `board:<a>-><b> column:<x>-><y>` → the four ids.
fn parse_board_move(details: &str) -> Option<(Uuid, Uuid, Uuid, Uuid)> {
    let rest = details.strip_prefix("board:")?;
    let (boards, columns) = rest.split_once(" column:")?;
    let (from_board, to_board) = boards.split_once("->")?;
    let (from_column, to_column) = columns.split_once("->")?;
    Some((
        from_board.parse().ok()?,
        to_board.parse().ok()?,
        from_column.parse().ok()?,
        to_column.parse().ok()?,
    ))
}

/// Read the input of [`compute`] for the board `board_id`: its live
/// columns, its live items (of the four types), their moves, and the
/// cancelled tasks. 4 queries and one for each type.
pub fn load_input(conn: &mut PgConnection, board_id: Uuid) -> Result<FlowInput, DieselError> {
    use crate::schema::{activity_log, adrs, board_columns, initiatives, strategies, tasks};

    let columns: Vec<FlowColumn> = board_columns::table
        .filter(board_columns::board_id.eq(board_id))
        .filter(board_columns::deleted_at.is_null())
        .order((board_columns::position.asc(), board_columns::id.asc()))
        .select((
            board_columns::id,
            board_columns::name,
            board_columns::is_done,
            board_columns::claims,
        ))
        .load::<(Uuid, String, bool, bool)>(conn)?
        .into_iter()
        .map(|(id, name, is_done, claims)| FlowColumn {
            id,
            name,
            is_done,
            claims,
        })
        .collect();

    type Row = (Uuid, String, String, Uuid, DateTime<Utc>);
    let mut rows: Vec<Row> = Vec::new();
    macro_rules! live_items {
        ($table:ident) => {
            rows.extend(
                $table::table
                    .filter($table::board_id.eq(board_id))
                    .filter($table::deleted_at.is_null())
                    .select((
                        $table::id,
                        $table::short_code,
                        $table::title,
                        $table::column_id,
                        $table::created_at,
                    ))
                    .load::<Row>(conn)?,
            )
        };
    }
    live_items!(strategies);
    live_items!(initiatives);
    live_items!(tasks);
    rows.extend(
        adrs::table
            .filter(adrs::board_id.eq(Some(board_id)))
            .filter(adrs::deleted_at.is_null())
            .filter(adrs::column_id.is_not_null())
            .select((
                adrs::id,
                adrs::short_code,
                adrs::title,
                adrs::column_id.assume_not_null(),
                adrs::created_at,
            ))
            .load::<Row>(conn)?,
    );
    let items: Vec<FlowItem> = rows
        .into_iter()
        .map(|(id, short_code, title, column_id, created_at)| FlowItem {
            id,
            short_code,
            title,
            column_id,
            created_at,
        })
        .collect();
    let ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();

    let column_name: HashMap<Uuid, String> = board_columns::table
        .filter(board_columns::board_id.eq(board_id))
        .select((board_columns::id, board_columns::name))
        .load::<(Uuid, String)>(conn)?
        .into_iter()
        .collect();
    let log: Vec<(Uuid, String, String, DateTime<Utc>)> = activity_log::table
        .filter(activity_log::entity_id.eq_any(&ids))
        .filter(activity_log::action.eq_any(["transition", "cancel", "board_move"]))
        .order(activity_log::occurred_at.asc())
        .select((
            activity_log::entity_id.assume_not_null(),
            activity_log::action,
            activity_log::details,
            activity_log::occurred_at,
        ))
        .load(conn)?;
    let mut moves = Vec::new();
    for (item_id, action, details, at) in log {
        if action == "board_move" {
            let Some((from_board, to_board, from_column, to_column)) = parse_board_move(&details)
            else {
                continue;
            };
            if to_board == board_id {
                moves.push(FlowMove {
                    item_id,
                    at,
                    from: None,
                    to: column_name.get(&to_column).cloned(),
                });
            } else if from_board == board_id {
                moves.push(FlowMove {
                    item_id,
                    at,
                    from: column_name.get(&from_column).cloned(),
                    to: None,
                });
            }
        } else if let Some((from, to)) = parse_column_move(&details) {
            moves.push(FlowMove {
                item_id,
                at,
                from: Some(from),
                to: Some(to),
            });
        }
    }

    let cancelled = {
        use crate::schema::task_cancellations as tc;
        tc::table
            .filter(tc::task_id.eq_any(&ids))
            .select((tc::task_id, tc::cancelled_at))
            .load::<(Uuid, DateTime<Utc>)>(conn)?
            .into_iter()
            .collect()
    };
    Ok(FlowInput {
        columns,
        items,
        moves,
        cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a time")
    }

    fn date(text: &str) -> NaiveDate {
        text.parse().expect("a date")
    }

    fn column(name: &str, is_done: bool, claims: bool) -> FlowColumn {
        FlowColumn {
            id: Uuid::new_v4(),
            name: name.to_string(),
            is_done,
            claims,
        }
    }

    fn mv(item: Uuid, when: &str, from: &str, to: &str) -> FlowMove {
        FlowMove {
            item_id: item,
            at: at(when),
            from: Some(from.to_string()),
            to: Some(to.to_string()),
        }
    }

    /// A board Backlog, Todo, Active (claims), Done (done), and 4 tasks:
    /// A done (Active Mon 09:00, Done Wed 09:00 = 2 days), B in Active since
    /// Tue, C cancelled on Thu, D done with no start.
    fn board() -> (FlowInput, [Uuid; 4]) {
        let columns = vec![
            column("Backlog", false, false),
            column("Todo", false, false),
            column("Active", false, true),
            column("Done", true, false),
        ];
        let ids = [
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        let item = |i: usize, code: &str, column: usize| FlowItem {
            id: ids[i],
            short_code: code.to_string(),
            title: format!("Task {code}"),
            column_id: columns[column].id,
            created_at: at("2026-09-28T08:00:00Z"),
        };
        let items = vec![
            item(0, "A", 3),
            item(1, "B", 2),
            item(2, "C", 3),
            item(3, "D", 3),
        ];
        let moves = vec![
            mv(ids[0], "2026-09-28T08:30:00Z", "Backlog", "Todo"),
            mv(ids[0], "2026-09-28T09:00:00Z", "Todo", "Active"),
            mv(ids[0], "2026-09-30T09:00:00Z", "Active", "Done"),
            mv(ids[1], "2026-09-29T10:00:00Z", "Backlog", "Active"),
            mv(ids[2], "2026-10-01T10:00:00Z", "Backlog", "Done"),
            mv(ids[3], "2026-10-06T10:00:00Z", "Todo", "Done"),
        ];
        let cancelled = HashMap::from([(ids[2], at("2026-10-01T10:00:00Z"))]);
        (
            FlowInput {
                columns,
                items,
                moves,
                cancelled,
            },
            ids,
        )
    }

    #[test]
    fn cycle_time_is_from_the_first_start_to_the_done_time() {
        let (input, _) = board();
        let flow = compute(
            &input,
            date("2026-09-28"),
            date("2026-10-11"),
            at("2026-10-11T10:00:00Z"),
        );
        assert_eq!(flow.start_columns, vec!["Active"]);
        assert_eq!(flow.done_columns, vec!["Done"]);
        assert_eq!(flow.cycle_times.len(), 1);
        assert_eq!(flow.cycle_times[0].short_code, "A");
        assert!((flow.cycle_times[0].days - 2.0).abs() < 1e-9);
        assert_eq!(flow.done_without_start, 1, "D has no start");
    }

    #[test]
    fn throughput_counts_cancelled_tasks_apart() {
        let (input, _) = board();
        let flow = compute(
            &input,
            date("2026-09-28"),
            date("2026-10-11"),
            at("2026-10-11T10:00:00Z"),
        );
        assert_eq!(
            flow.throughput,
            vec![
                WeekThroughput {
                    week_start: date("2026-09-28"),
                    done: 1,
                    cancelled: 1,
                },
                WeekThroughput {
                    week_start: date("2026-10-05"),
                    done: 1,
                    cancelled: 0,
                },
            ]
        );
        assert_eq!(flow.done_count(), 2);
        assert_eq!(flow.cancelled_count(), 1);
    }

    #[test]
    fn the_age_of_work_in_progress_is_from_its_start() {
        let (input, _) = board();
        let flow = compute(
            &input,
            date("2026-09-28"),
            date("2026-10-11"),
            at("2026-10-11T10:00:00Z"),
        );
        assert_eq!(flow.ageing.len(), 1);
        assert_eq!(flow.ageing[0].short_code, "B");
        assert_eq!(flow.ageing[0].column, "Active");
        assert!((flow.ageing[0].age_days - 12.0).abs() < 1e-9);
    }

    #[test]
    fn the_cumulative_flow_counts_each_column_at_the_end_of_each_day() {
        let (input, _) = board();
        let flow = compute(
            &input,
            date("2026-09-28"),
            date("2026-10-01"),
            at("2026-10-11T10:00:00Z"),
        );
        assert_eq!(flow.cfd_columns, vec!["Backlog", "Todo", "Active", "Done"]);
        let days: Vec<(String, Vec<i64>)> = flow
            .cfd
            .iter()
            .map(|d| (d.date.to_string(), d.counts.clone()))
            .collect();
        assert_eq!(
            days,
            vec![
                // Mon: A in Active; B, C in Backlog; D in Todo.
                ("2026-09-28".to_string(), vec![2, 1, 1, 0]),
                // Tue: B to Active.
                ("2026-09-29".to_string(), vec![1, 1, 2, 0]),
                // Wed: A done.
                ("2026-09-30".to_string(), vec![1, 1, 1, 1]),
                // Thu: C cancelled (to Done).
                ("2026-10-01".to_string(), vec![0, 1, 1, 2]),
            ]
        );
    }

    #[test]
    fn a_board_with_no_claims_column_starts_at_active_by_name() {
        let (mut input, _) = board();
        for c in &mut input.columns {
            c.claims = false;
        }
        let flow = compute(
            &input,
            date("2026-09-28"),
            date("2026-10-11"),
            at("2026-10-11T10:00:00Z"),
        );
        assert_eq!(flow.start_columns, vec!["Active"]);
        for c in &mut input.columns {
            if c.name == "Active" {
                c.name = "Doing".to_string();
            }
        }
        input.moves.clear();
        let flow = compute(
            &input,
            date("2026-09-28"),
            date("2026-10-11"),
            at("2026-10-11T10:00:00Z"),
        );
        assert!(flow.start_columns.is_empty());
        assert!(flow.cycle_times.is_empty());
    }

    #[test]
    fn the_details_of_the_moves_parse() {
        assert_eq!(
            parse_column_move("column:Todo->Active"),
            Some(("Todo".to_string(), "Active".to_string()))
        );
        assert_eq!(
            parse_column_move("column:Active->Done reason:not -> needed"),
            Some(("Active".to_string(), "Done".to_string()))
        );
        let (a, b, c, d) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        assert_eq!(
            parse_board_move(&format!("board:{a}->{b} column:{c}->{d}")),
            Some((a, b, c, d))
        );
        assert_eq!(parse_column_move("board:x"), None);
    }

    #[test]
    fn percentiles_and_weeks() {
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&sorted, 50.0), Some(2.0));
        assert_eq!(percentile(&sorted, 85.0), Some(4.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(week_start(date("2026-10-11")), date("2026-10-05"));
        assert_eq!(week_start(date("2026-10-05")), date("2026-10-05"));
    }
}
