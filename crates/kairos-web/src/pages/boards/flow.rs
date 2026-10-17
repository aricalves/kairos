//! The flow metrics of a board on the board page (KAIROS-T-0372): the
//! panel that the button "Flow" opens. It reads `GET /api/boards/{id}/flow`
//! for the last 30, 90, 180 or 365 days, and shows:
//!
//! - tiles: the done items, the cancelled tasks (apart), the median and the
//!   85th percentile of the cycle time, the items in progress;
//! - the throughput per week: bars, and a table;
//! - the work in progress, oldest first;
//! - the cumulative flow: one line for each column, over the days.
//!
//! The charts are the Aurora `Sparkline` and `StatTile`, in token colours.

use aurora_dark::components::{Alert, Group, Loading, Panel, Pill, Select, Stack, Table, Text};
use aurora_dark::data::{Sparkline, StatTile};
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;

use crate::auth::{Auth, use_auth};

/// mirror of: `kairos_client::types_org::BoardFlow` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct BoardFlow {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub start_columns: Vec<String>,
    pub done_count: i64,
    pub cancelled_count: i64,
    pub cycle_time: CycleTime,
    #[serde(default)]
    pub throughput: Vec<Week>,
    #[serde(default)]
    pub ageing: Vec<AgeingItem>,
    pub cumulative_flow: CumulativeFlow,
}

/// mirror of: `kairos_client::types_org::FlowCycleTime` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CycleTime {
    pub count: i64,
    pub median_days: Option<f64>,
    pub p85_days: Option<f64>,
}

/// mirror of: `kairos_client::types_org::FlowWeek`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Week {
    pub week_start: String,
    pub done: i64,
    pub cancelled: i64,
}

/// mirror of: `kairos_client::types_org::FlowAgeingItem` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AgeingItem {
    pub short_code: String,
    pub title: String,
    pub column: String,
    pub age_days: f64,
}

/// mirror of: `kairos_client::types_org::CumulativeFlow`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CumulativeFlow {
    pub columns: Vec<String>,
    pub days: Vec<FlowDay>,
}

/// mirror of: `kairos_client::types_org::CumulativeFlowDay`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FlowDay {
    pub date: String,
    pub counts: Vec<i64>,
}

/// The ranges that the panel offers, in days.
const RANGES: [&str; 4] = ["30", "90", "180", "365"];

/// The milliseconds of the first day of a range of `days` days that ends on
/// the day of `now_ms`. Pure, host-tested.
pub(crate) fn range_start_ms(now_ms: f64, days: u32) -> f64 {
    now_ms - f64::from(days.saturating_sub(1)) * 86_400_000.0
}

/// `YYYY-MM-DD` (UTC) of a time in milliseconds.
fn day_of(ms: f64) -> String {
    let iso: String = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms))
        .to_iso_string()
        .into();
    iso.get(..10).unwrap_or(&iso).to_string()
}

/// A number of days with at most one decimal, or "-". Pure, host-tested.
pub(crate) fn days_text(value: Option<f64>) -> String {
    match value {
        Some(v) => {
            let rounded = (v * 10.0).round() / 10.0;
            if rounded.fract() == 0.0 {
                format!("{rounded:.0}")
            } else {
                format!("{rounded:.1}")
            }
        }
        None => "-".to_string(),
    }
}

/// The series of one column of the cumulative flow. Pure, host-tested.
pub(crate) fn column_series(flow: &CumulativeFlow, index: usize) -> Vec<f64> {
    flow.days
        .iter()
        .map(|d| d.counts.get(index).copied().unwrap_or(0) as f64)
        .collect()
}

async fn fetch_flow(auth: Auth, board: String, days: u32) -> Result<BoardFlow, ApiError> {
    let now = js_sys::Date::now();
    let from = day_of(range_start_ms(now, days));
    let to = day_of(now);
    crate::api::get_json(
        auth,
        &format!(
            "/api/boards/{}/flow?from={from}&to={to}",
            crate::api::encode_component(&board)
        ),
    )
    .await
}

/// The flow panel of the board `board` (slug).
#[component]
pub fn FlowPanel(#[prop(into)] board: String) -> impl IntoView {
    let auth = use_auth();
    let board = StoredValue::new(board);
    let range = RwSignal::new("90".to_string());
    let flow = LocalResource::new(move || {
        auth.track_session();
        let days: u32 = range.get().parse().unwrap_or(90);
        fetch_flow(auth, board.get_value(), days)
    });
    let options: Vec<(String, String)> = RANGES
        .iter()
        .map(|d| (d.to_string(), format!("Last {d} days")))
        .collect();
    view! {
        <div class="kairos-flow" data-testid="flow-panel">
            <Panel title="Flow" caption="cycle time, throughput, work in progress, cumulative flow">
                <Stack gap="md">
                    <Group align="end" gap="sm">
                        <Select label="Range" option_pairs=options value=range/>
                    </Group>
                    {move || match flow.get() {
                        None => view! { <Loading label="Loading the flow…"/> }.into_any(),
                        Some(Err(error)) => view! {
                            <Alert title="The flow is not available" color=token::BAD>
                                <Text size="sm">{crate::pages::item::api::error_text(&error)}</Text>
                            </Alert>
                        }.into_any(),
                        Some(Ok(flow)) => view! { <FlowBody flow/> }.into_any(),
                    }}
                </Stack>
            </Panel>
        </div>
    }
}

#[component]
fn FlowBody(flow: BoardFlow) -> impl IntoView {
    let range = format!("{} to {} (UTC)", flow.from, flow.to);
    let start = if flow.start_columns.is_empty() {
        "This board has no start column (no column with claims, and no column named \
         Active), so it has no cycle time."
            .to_string()
    } else {
        format!(
            "Cycle time: from the first move into {} to the last move into a done column. \
             Cancelled tasks count apart.",
            flow.start_columns.join(", ")
        )
    };
    let done = flow.done_count.to_string();
    let cancelled = flow.cancelled_count.to_string();
    let median = days_text(flow.cycle_time.median_days);
    let p85 = days_text(flow.cycle_time.p85_days);
    let in_progress = flow.ageing.len().to_string();
    let cycle_sub = format!("{} items", flow.cycle_time.count);
    let done_series: Vec<f64> = flow.throughput.iter().map(|w| w.done as f64).collect();
    let weeks = flow
        .throughput
        .clone()
        .into_iter()
        .rev()
        .map(|w| {
            view! {
                <tr>
                    <td>{w.week_start}</td>
                    <td class="cl-num">{w.done}</td>
                    <td class="cl-num">{w.cancelled}</td>
                </tr>
            }
        })
        .collect_view();
    let ageing = flow
        .ageing
        .clone()
        .into_iter()
        .map(|a| {
            let href = format!("/items/{}", a.short_code);
            view! {
                <tr>
                    <td><a class="cl-mono kairos-card__code" href=href>{a.short_code}</a></td>
                    <td>{a.title}</td>
                    <td><Pill color=token::ICE>{a.column}</Pill></td>
                    <td class="cl-num">{days_text(Some(a.age_days))}</td>
                </tr>
            }
        })
        .collect_view();
    let no_ageing = flow.ageing.is_empty();
    let cfd = flow.cumulative_flow.clone();
    let colors = [
        token::TEAL,
        token::ICE,
        token::GOLD,
        token::VIOLET,
        token::OK,
    ];
    let cfd_rows = cfd
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let series = column_series(&cfd, i);
            let now = series.last().copied().unwrap_or(0.0);
            let color = colors[i % colors.len()];
            view! {
                <tr data-testid="cfd-row">
                    <td>{name.clone()}</td>
                    <td>
                        <Sparkline values=Signal::derive(move || series.clone()) fill=true
                            width=240.0 height=28.0 color=color
                            label=format!("Live items in {name}, each day")/>
                    </td>
                    <td class="cl-num">{format!("{now:.0}")}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <Stack gap="md">
            <Text size="xs" dimmed=true>{range}</Text>
            <div class="kairos-flow__tiles">
                <StatTile label="Done" value=Signal::derive(move || done.clone())
                    color=token::OK sub="in the range"/>
                <StatTile label="Cancelled" value=Signal::derive(move || cancelled.clone())
                    color=token::MUTED sub="counted apart"/>
                <StatTile label="Cycle time, median" value=Signal::derive(move || median.clone())
                    unit="days" sub=cycle_sub.clone()/>
                <StatTile label="Cycle time, 85%" value=Signal::derive(move || p85.clone())
                    unit="days"/>
                <StatTile label="In progress" value=Signal::derive(move || in_progress.clone())
                    color=token::ICE/>
            </div>
            <Text size="xs" dimmed=true>{start}</Text>
            <div data-testid="flow-throughput">
                <Text bright=true bold=true size="sm">"Throughput per week"</Text>
                <Sparkline values=Signal::derive(move || done_series.clone()) bars=true
                    width=240.0 height=32.0 color=token::OK label="Done items each week"/>
                <Table label="Throughput per week" mono=true>
                    <thead><tr><th>"Week of"</th><th class="cl-num">"Done"</th>
                        <th class="cl-num">"Cancelled"</th></tr></thead>
                    <tbody>{weeks}</tbody>
                </Table>
            </div>
            <div data-testid="flow-ageing">
                <Text bright=true bold=true size="sm">"Work in progress, oldest first"</Text>
                {no_ageing.then(|| view! { <Text size="sm" dimmed=true>"No item is in progress."</Text> })}
                <Table label="Work in progress">
                    <thead><tr><th>"Item"</th><th>"Title"</th><th>"Column"</th>
                        <th class="cl-num">"Age (days)"</th></tr></thead>
                    <tbody>{ageing}</tbody>
                </Table>
            </div>
            <div data-testid="flow-cfd">
                <Text bright=true bold=true size="sm">"Cumulative flow: live items of each column, each day"</Text>
                <Table label="Cumulative flow">
                    <thead><tr><th>"Column"</th><th>"Each day"</th><th class="cl-num">"Now"</th></tr></thead>
                    <tbody>{cfd_rows}</tbody>
                </Table>
            </div>
        </Stack>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_of_n_days_starts_n_minus_1_days_before() {
        assert_eq!(range_start_ms(10.0 * 86_400_000.0, 1), 10.0 * 86_400_000.0);
        assert_eq!(range_start_ms(10.0 * 86_400_000.0, 3), 8.0 * 86_400_000.0);
    }

    #[test]
    fn days_have_one_decimal_at_most() {
        assert_eq!(days_text(Some(2.0)), "2");
        assert_eq!(days_text(Some(1.25)), "1.3");
        assert_eq!(days_text(None), "-");
    }

    #[test]
    fn the_series_of_a_column_is_its_count_each_day() {
        let flow = CumulativeFlow {
            columns: vec!["Todo".into(), "Done".into()],
            days: vec![
                FlowDay {
                    date: "2026-10-01".into(),
                    counts: vec![2, 0],
                },
                FlowDay {
                    date: "2026-10-02".into(),
                    counts: vec![1, 1],
                },
            ],
        };
        assert_eq!(column_series(&flow, 1), vec![0.0, 1.0]);
        assert_eq!(column_series(&flow, 5), vec![0.0, 0.0]);
    }

    #[test]
    fn the_mirror_decodes_the_server_body() {
        let flow: BoardFlow = serde_json::from_value(serde_json::json!({
            "board_id": "b", "board_slug": "web", "from": "2026-10-01", "to": "2026-10-02",
            "start_columns": ["Active"], "done_columns": ["Done"],
            "done_count": 1, "cancelled_count": 0, "done_without_start": 0,
            "cycle_time": {"count": 1, "mean_days": 2.0, "median_days": 2.0,
                           "p85_days": 2.0, "items": []},
            "throughput": [{"week_start": "2026-09-28", "done": 1, "cancelled": 0}],
            "ageing": [],
            "cumulative_flow": {"columns": ["Done"], "days": []}
        }))
        .expect("decodes");
        assert_eq!(flow.cycle_time.median_days, Some(2.0));
    }
}
