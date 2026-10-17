//! `/admin/members/:user` — the grants of one person on many boards, and
//! the teams of the person (KAIROS-T-0413, KAIROS-A-0025).
//!
//! # The grid (design: KAIROS-T-0413)
//!
//! A row for each board, grouped by level (strategy, initiative, delivery,
//! ADR); a column for each capability; a checkbox in each cell. A cell is
//! enabled only when its capability applies at the level of the board: the
//! server sends that list for each board (`applicable`, from
//! `kairos_core::abac::capabilities_of_level`), so the map is defined one
//! time.
//!
//! The **All boards** row is at the top. A tick in its column ticks that
//! capability on each board where it applies; a second click clears it.
//! The rows are the boards that the viewer administers (the server lists
//! only those), so each enabled cell is one the viewer can change. A single
//! row can be changed after.
//!
//! **Save** sends only the changed cells, as grants and revokes, in one
//! request; the server applies them in one transaction. There are no role
//! presets: each cell is one grant row. A stored glob (`*`, `manage_*`, …)
//! is not a column; it shows as text in its row and is not changed here.
//!
//! The teams section shows when the server sends `teams` (the viewer can
//! change team members). It adds and removes the person with the existing
//! team member routes.
//!
//! The grid logic is pure ([`GridRow`], [`column_state`], [`set_column`],
//! [`grant_changes`]) and host-tested.

use std::collections::BTreeSet;

use aurora_dark::components::{
    Anchor, Button, Code, Empty, ErrorState, Group, Loading, PageHeader, Panel, Pill, Select,
    Stack, Table, Text,
};
use aurora_dark::tokens::token;
use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use super::api::{self, BoardGrantChange, UserBoardGrants, UserGrants};
use super::{MutationNotice, MutationOutcome, run_mutation};
use crate::auth::use_auth;

/// The columns of the grid: each grantable capability that is not a glob,
/// with its short header.
pub(crate) const COLUMNS: &[(&str, &str)] = &[
    ("manage_strategies", "Strategies"),
    ("manage_initiatives", "Initiatives"),
    ("manage_tasks", "Tasks"),
    ("manage_adrs", "ADRs"),
    ("manage_documents", "Documents"),
    ("transition_items", "Move items"),
    ("configure_boards", "Configure"),
    ("administer_members", "Members"),
    ("pages", "Sites"),
];

/// The board levels in the order of the grid, with the title of each group.
pub(crate) const LEVELS: &[(&str, &str)] = &[
    ("strategy", "Strategy boards"),
    ("initiative", "Initiative boards"),
    ("delivery", "Delivery boards"),
    ("adr", "ADR boards"),
];

/// One row of the grid: a board, what applies on it, what is stored, and
/// what is ticked now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GridRow {
    pub board_id: String,
    pub slug: String,
    pub name: String,
    pub level: String,
    /// The capabilities that apply at the level of the board.
    pub applicable: Vec<String>,
    /// The stored grants that are columns of the grid.
    pub original: BTreeSet<String>,
    /// The ticked cells.
    pub ticked: BTreeSet<String>,
    /// The stored grants that are not columns (globs).
    pub other: Vec<String>,
}

impl GridRow {
    pub(crate) fn from_board(board: &UserBoardGrants) -> Self {
        let is_column = |c: &str| COLUMNS.iter().any(|(name, _)| *name == c);
        let original: BTreeSet<String> = board
            .capabilities
            .iter()
            .filter(|c| is_column(c))
            .cloned()
            .collect();
        Self {
            board_id: board.board_id.clone(),
            slug: board.board_slug.clone(),
            name: board.board_name.clone(),
            level: board.board_level.clone(),
            applicable: board.applicable.clone(),
            ticked: original.clone(),
            original,
            other: board
                .capabilities
                .iter()
                .filter(|c| !is_column(c))
                .cloned()
                .collect(),
        }
    }

    pub(crate) fn applies(&self, capability: &str) -> bool {
        self.applicable.iter().any(|c| c == capability)
    }

    /// Tick or clear one cell. A cell that does not apply does not change.
    pub(crate) fn set(&mut self, capability: &str, on: bool) {
        if !self.applies(capability) {
            return;
        }
        if on {
            self.ticked.insert(capability.to_string());
        } else {
            self.ticked.remove(capability);
        }
    }
}

/// The rows of the grid for a person, in the order of the server (by
/// level, then by name).
pub(crate) fn grid_rows(grants: &UserGrants) -> Vec<GridRow> {
    grants.boards.iter().map(GridRow::from_board).collect()
}

/// The state of a column of the All boards row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnState {
    /// The capability applies on no board of the grid.
    NotApplicable,
    /// Ticked on no board where it applies.
    None,
    /// Ticked on some boards where it applies.
    Some,
    /// Ticked on each board where it applies.
    All,
}

pub(crate) fn column_state(rows: &[GridRow], capability: &str) -> ColumnState {
    let applies: Vec<&GridRow> = rows.iter().filter(|r| r.applies(capability)).collect();
    if applies.is_empty() {
        return ColumnState::NotApplicable;
    }
    let ticked = applies
        .iter()
        .filter(|r| r.ticked.contains(capability))
        .count();
    match ticked {
        0 => ColumnState::None,
        n if n == applies.len() => ColumnState::All,
        _ => ColumnState::Some,
    }
}

/// Tick (`on`) or clear the capability on each board where it applies.
pub(crate) fn set_column(rows: &mut [GridRow], capability: &str, on: bool) {
    for row in rows {
        row.set(capability, on);
    }
}

/// A click on a column of the All boards row: clear it when it is ticked
/// on each board, else tick it on each board.
pub(crate) fn click_column(rows: &mut [GridRow], capability: &str) {
    let on = column_state(rows, capability) != ColumnState::All;
    set_column(rows, capability, on);
}

/// The changed cells, as one change for each board with a changed cell.
pub(crate) fn grant_changes(rows: &[GridRow]) -> Vec<BoardGrantChange> {
    rows.iter()
        .filter_map(|row| {
            let grant: Vec<String> = row.ticked.difference(&row.original).cloned().collect();
            let revoke: Vec<String> = row.original.difference(&row.ticked).cloned().collect();
            (!grant.is_empty() || !revoke.is_empty()).then(|| BoardGrantChange {
                board_id: row.board_id.clone(),
                grant,
                revoke,
            })
        })
        .collect()
}

/// The number of changed cells.
pub(crate) fn changed_cells(rows: &[GridRow]) -> usize {
    grant_changes(rows)
        .iter()
        .map(|c| c.grant.len() + c.revoke.len())
        .sum()
}

/// `/admin/members/:user`.
#[component]
pub fn AdminMemberGrantsPage() -> impl IntoView {
    let auth = use_auth();
    let params = use_params_map();
    let user_id = Memo::new(move |_| params.read().get("user").unwrap_or_default());
    let reload = RwSignal::new(0u32);
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);

    let grants = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        let user_id = user_id.get();
        async move { api::user_grants(auth, &user_id).await }
    });

    view! {
        <Stack gap="md">
            <Anchor href="/admin/members">"Back to the members"</Anchor>
            {move || match grants.get() {
                None => view! { <Loading/> }.into_any(),
                Some(Err(error)) => view! {
                    <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                }.into_any(),
                Some(Ok(person)) => {
                    let rows = RwSignal::new(grid_rows(&person));
                    let teams = person.teams.clone();
                    let user = StoredValue::new(person.user_id.clone());
                    view! {
                        <PageHeader title=person.display_name.clone() sub=person.email.clone()/>
                        <MutationNotice outcome/>
                        <GrantsPanel rows user busy outcome reload/>
                        {teams.map(|teams| view! {
                            <TeamsPanel teams user busy outcome reload/>
                        })}
                    }.into_any()
                }
            }}
        </Stack>
    }
}

/// The panel of the grid and its Save button.
#[component]
fn GrantsPanel(
    rows: RwSignal<Vec<GridRow>>,
    user: StoredValue<String>,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let changed = move || rows.with(|r| changed_cells(r));
    let on_save = move |_| {
        let changes = rows.with_untracked(|r| grant_changes(r));
        if changes.is_empty() {
            return;
        }
        let cells: usize = changes.iter().map(|c| c.grant.len() + c.revoke.len()).sum();
        let boards = changes.len();
        let user_id = user.get_value();
        run_mutation(
            busy,
            outcome,
            reload,
            format!("Kairos changed {cells} grants on {boards} boards."),
            async move {
                api::change_user_grants(auth, &user_id, &changes)
                    .await
                    .map(|_| ())
            },
        );
    };
    let on_reset = move |_| {
        rows.update(|r| {
            for row in r.iter_mut() {
                row.ticked = row.original.clone();
            }
        })
    };
    view! {
        <Panel title="Boards"
            caption="Tick a capability to grant it on a board. A cell is off when the \
                     capability does not apply at the level of the board. The All boards \
                     row ticks a column on each board where it applies.">
            {move || if rows.with(|r| r.is_empty()) {
                view! { <Empty message="You administer no board."/> }.into_any()
            } else {
                view! { <GrantsGrid rows/> }.into_any()
            }}
            <Group justify="end" gap="sm">
                <Text dimmed=true size="sm">
                    {move || match changed() {
                        0 => "No change.".to_string(),
                        1 => "1 changed cell.".to_string(),
                        n => format!("{n} changed cells."),
                    }}
                </Text>
                <Button variant="default" disabled=Signal::derive(move || changed() == 0)
                    on_click=Callback::new(on_reset)>
                    "Undo the changes"
                </Button>
                <Button disabled=Signal::derive(move || changed() == 0 || busy.get())
                    on_click=Callback::new(on_save)>
                    "Save"
                </Button>
            </Group>
        </Panel>
    }
}

/// The table of the grid: the All boards row, then a group of rows for
/// each level.
#[component]
fn GrantsGrid(rows: RwSignal<Vec<GridRow>>) -> impl IntoView {
    let header = COLUMNS
        .iter()
        .map(|(name, label)| {
            view! {
                <th title=*name>{*label}</th>
            }
        })
        .collect_view();
    let all_row = COLUMNS
        .iter()
        .map(|(name, label)| {
            let name: &'static str = name;
            let state = move || rows.with(|r| column_state(r, name));
            view! {
                <td>
                    <input type="checkbox"
                        aria-label=format!("{label} on all boards")
                        prop:checked=move || state() == ColumnState::All
                        prop:indeterminate=move || state() == ColumnState::Some
                        disabled=move || state() == ColumnState::NotApplicable
                        on:change=move |_| rows.update(|r| click_column(r, name))/>
                </td>
            }
        })
        .collect_view();
    let groups = LEVELS
        .iter()
        .map(|(level, title)| {
            let level: &'static str = level;
            let indexes: Vec<usize> = rows.with_untracked(|r| {
                r.iter()
                    .enumerate()
                    .filter(|(_, row)| row.level == level)
                    .map(|(i, _)| i)
                    .collect()
            });
            (!indexes.is_empty()).then(|| {
                let body = indexes
                    .into_iter()
                    .map(|index| view! { <BoardRow rows index/> })
                    .collect_view();
                view! {
                    <tr>
                        <td colspan=(COLUMNS.len() + 1).to_string()>
                            <Text bright=true size="sm" bold=true>{*title}</Text>
                        </td>
                    </tr>
                    {body}
                }
            })
        })
        .collect_view();
    view! {
        <Table label="Grants on each board" min_width="760px">
            <thead>
                <tr>
                    <th>"Board"</th>
                    {header}
                </tr>
            </thead>
            <tbody>
                <tr>
                    <td><Text bright=true bold=true>"All boards"</Text></td>
                    {all_row}
                </tr>
                {groups}
            </tbody>
        </Table>
    }
}

/// One board row of the grid.
#[component]
fn BoardRow(rows: RwSignal<Vec<GridRow>>, index: usize) -> impl IntoView {
    let row = rows.with_untracked(|r| r[index].clone());
    let other = (!row.other.is_empty()).then(|| {
        let text = row.other.join(", ");
        view! {
            <Text dimmed=true size="xs">
                "Also: "<Code>{text}</Code>" (change it in the members panel of the board)"
            </Text>
        }
    });
    let cells = COLUMNS
        .iter()
        .map(|(name, label)| {
            let name: &'static str = name;
            let applies = row.applies(name);
            let ticked = move || rows.with(|r| r[index].ticked.contains(name));
            let changed = move || {
                rows.with(|r| r[index].ticked.contains(name) != r[index].original.contains(name))
            };
            view! {
                <td>
                    <input type="checkbox"
                        aria-label=format!("{label} on {}", row.slug)
                        prop:checked=ticked
                        disabled=!applies
                        on:change=move |event| {
                            let on = event_target_checked(&event);
                            rows.update(|r| r[index].set(name, on));
                        }/>
                    {move || changed().then(|| view! { <Pill color=token::GOLD>"changed"</Pill> })}
                </td>
            }
        })
        .collect_view();
    view! {
        <tr>
            <td>
                <Stack gap="xs">
                    <Anchor href=format!("/admin/boards/{}", row.board_id)>{row.name.clone()}</Anchor>
                    <Text dimmed=true size="xs">{row.slug.clone()}</Text>
                    {other}
                </Stack>
            </td>
            {cells}
        </tr>
    }
}

/// The teams of the person: remove from a team, add to a team.
#[component]
fn TeamsPanel(
    teams: Vec<api::UserTeam>,
    user: StoredValue<String>,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let all_teams = LocalResource::new(move || {
        auth.track_session();
        api::list_teams(auth)
    });
    let member_of: BTreeSet<String> = teams.iter().map(|t| t.team_id.clone()).collect();
    let chosen = RwSignal::new(String::new());
    let on_add = move |_| {
        let team_id = chosen.get_untracked();
        if team_id.is_empty() {
            return;
        }
        let user_id = user.get_value();
        run_mutation(
            busy,
            outcome,
            reload,
            "Kairos added the person to the team.".to_string(),
            async move {
                api::add_team_member(auth, &team_id, &user_id)
                    .await
                    .map(|_| ())
            },
        );
    };
    let current = if teams.is_empty() {
        view! { <Text dimmed=true size="sm">"The person is in no team."</Text> }.into_any()
    } else {
        teams
            .into_iter()
            .map(|team| {
                let team_id = StoredValue::new(team.team_id.clone());
                let name = team.name.clone();
                let on_remove = move |_| {
                    let user_id = user.get_value();
                    let team_id = team_id.get_value();
                    run_mutation(
                        busy,
                        outcome,
                        reload,
                        "Kairos removed the person from the team.".to_string(),
                        async move {
                            api::remove_team_member(auth, &team_id, &user_id)
                                .await
                                .map(|_| ())
                        },
                    );
                };
                view! {
                    <Group justify="between">
                        <Group gap="sm">
                            <Text bright=true>{name}</Text>
                            <Text dimmed=true size="sm">{team.slug.clone()}</Text>
                        </Group>
                        <Button variant="default" size="xs" bad=true
                            on_click=Callback::new(on_remove)>
                            "Remove from the team"
                        </Button>
                    </Group>
                }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <Panel title="Teams"
            caption="A member of a team can do the daily work of the boards of the team \
                     with no grant.">
            <Stack gap="sm">
                {current}
                {move || match all_teams.get() {
                    Some(Ok(list)) => {
                        let options: Vec<(String, String)> = list
                            .into_iter()
                            .filter(|t| !member_of.contains(&t.id))
                            .map(|t| (t.id, t.name))
                            .collect();
                        if options.is_empty() {
                            return ().into_any();
                        }
                        view! {
                            <Group align="end" gap="sm">
                                <Select label="Team" option_pairs=options value=chosen
                                    placeholder="Choose a team"/>
                                <Button on_click=Callback::new(on_add)>"Add to the team"</Button>
                            </Group>
                        }.into_any()
                    }
                    Some(Err(error)) => view! { <ErrorState error/> }.into_any(),
                    None => view! { <Loading/> }.into_any(),
                }}
            </Stack>
        </Panel>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(slug: &str, level: &str, own: &str, stored: &[&str]) -> UserBoardGrants {
        UserBoardGrants {
            board_id: format!("id-{slug}"),
            board_slug: slug.to_string(),
            board_name: slug.to_string(),
            board_level: level.to_string(),
            applicable: [
                own,
                "manage_documents",
                "transition_items",
                "configure_boards",
                "administer_members",
            ]
            .iter()
            .map(|c| c.to_string())
            .collect(),
            capabilities: stored.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn rows() -> Vec<GridRow> {
        grid_rows(&UserGrants {
            user_id: "u".into(),
            email: "sam@example.test".into(),
            display_name: "Sam".into(),
            boards: vec![
                board("strategy", "strategy", "manage_strategies", &[]),
                board("alpha", "delivery", "manage_tasks", &["manage_tasks"]),
                board(
                    "beta",
                    "delivery",
                    "manage_tasks",
                    &["manage_*", "transition_items"],
                ),
            ],
            teams: None,
        })
    }

    /// A stored glob is not a column; it is kept apart, and never sent.
    #[test]
    fn a_glob_is_not_a_cell() {
        let rows = rows();
        assert_eq!(rows[2].other, vec!["manage_*".to_string()]);
        assert!(rows[2].original.contains("transition_items"));
        assert!(!rows[2].original.contains("manage_*"));
        assert!(grant_changes(&rows).is_empty());
    }

    /// The All boards row ticks a column on each board where it applies,
    /// and only there; a second click clears it.
    #[test]
    fn all_boards_ticks_each_board_where_the_capability_applies() {
        let mut rows = rows();
        assert_eq!(column_state(&rows, "manage_tasks"), ColumnState::Some);
        click_column(&mut rows, "manage_tasks");
        assert_eq!(column_state(&rows, "manage_tasks"), ColumnState::All);
        assert!(!rows[0].ticked.contains("manage_tasks"), "strategy board");
        assert!(rows[2].ticked.contains("manage_tasks"));
        click_column(&mut rows, "manage_tasks");
        assert_eq!(column_state(&rows, "manage_tasks"), ColumnState::None);
        assert_eq!(
            column_state(&rows, "manage_adrs"),
            ColumnState::NotApplicable
        );
        click_column(&mut rows, "manage_adrs");
        assert!(rows.iter().all(|r| !r.ticked.contains("manage_adrs")));
    }

    /// A single row can be changed after the All boards row.
    #[test]
    fn a_row_changes_after_the_all_boards_row() {
        let mut rows = rows();
        set_column(&mut rows, "configure_boards", true);
        rows[1].set("configure_boards", false);
        assert_eq!(column_state(&rows, "configure_boards"), ColumnState::Some);
        // A cell that does not apply does not change.
        rows[0].set("manage_tasks", true);
        assert!(!rows[0].ticked.contains("manage_tasks"));
    }

    /// Save sends only the changed cells: a grant for each new tick, a
    /// revoke for each cleared stored grant, and no entry for a board with
    /// no change. A tick that is cleared again is no change.
    #[test]
    fn save_sends_only_the_changed_cells() {
        let mut rows = rows();
        assert_eq!(changed_cells(&rows), 0);
        rows[0].set("transition_items", true);
        rows[1].set("manage_tasks", false);
        rows[1].set("manage_documents", true);
        rows[2].set("administer_members", true);
        rows[2].set("administer_members", false);
        let changes = grant_changes(&rows);
        assert_eq!(
            changes,
            vec![
                BoardGrantChange {
                    board_id: "id-strategy".into(),
                    grant: vec!["transition_items".into()],
                    revoke: vec![],
                },
                BoardGrantChange {
                    board_id: "id-alpha".into(),
                    grant: vec!["manage_documents".into()],
                    revoke: vec!["manage_tasks".into()],
                },
            ]
        );
        assert_eq!(changed_cells(&rows), 3);
    }

    /// Each column is a grantable capability, and no glob.
    #[test]
    fn the_columns_are_the_capabilities() {
        assert_eq!(COLUMNS.len(), 9);
        assert!(COLUMNS.iter().all(|(name, _)| !name.contains('*')));
        assert_eq!(
            LEVELS.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            ["strategy", "initiative", "delivery", "adr"]
        );
    }
}
