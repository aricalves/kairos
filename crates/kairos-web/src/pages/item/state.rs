//! The live state of a task on the item page (KAIROS-T-0506).
//!
//! The state is one current line, separate from the notes: the branch,
//! the last commit, pushed or not, the PR, the next step, and who wrote
//! it when. An agent writes it (`set_state`, `kairos state`); the page
//! shows it. [`StatePanel`] shows nothing for a task with no state.

use aurora_dark::components::{Panel, Stack, Text};
use leptos::prelude::*;

use super::api::State;

/// The rows of the panel: `(label, value)`, for the fields with a value.
/// Pure, host-tested.
pub(crate) fn state_rows(state: &State) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = Vec::new();
    if let Some(branch) = &state.branch {
        rows.push(("Branch", branch.clone()));
    }
    if let Some(commit) = &state.commit {
        rows.push(("Commit", commit.clone()));
    }
    if state.branch.is_some() || state.commit.is_some() || state.pushed {
        rows.push((
            "Pushed",
            if state.pushed { "yes" } else { "no" }.to_string(),
        ));
    }
    if let Some(pr) = &state.pr_url {
        rows.push(("PR", pr.clone()));
    }
    if let Some(next) = &state.next_step {
        rows.push(("Next step", next.clone()));
    }
    rows
}

/// Who wrote the state, and when. Pure, host-tested.
pub(crate) fn state_caption(state: &State) -> String {
    let day = state.updated_at.get(..10).unwrap_or(&state.updated_at);
    format!("updated {day} by {}", state.updated_by_name)
}

/// The state panel of a task. Nothing for a task with no state.
#[component]
pub(crate) fn StatePanel(state: Option<State>) -> impl IntoView {
    state.map(|state| {
        let caption = state_caption(&state);
        let rows = state_rows(&state);
        let pr = state.pr_url.clone();
        view! {
            <Panel title="State" caption=caption>
                <div class="kairos-item__state" data-testid="state-panel">
                    <Stack gap="xs">
                        {rows.into_iter().map(|(label, value)| {
                            let is_pr = label == "PR";
                            let href = pr.clone();
                            view! {
                                <div class="kairos-item__state-row">
                                    <Text size="xs" dimmed=true>{label}</Text>
                                    {if is_pr {
                                        view! {
                                            <a href=href.unwrap_or_default() rel="external" target="_blank">
                                                <Text size="sm" bright=true>{value}</Text>
                                            </a>
                                        }.into_any()
                                    } else {
                                        view! {
                                            <Text size="sm" bright=true>{value}</Text>
                                        }.into_any()
                                    }}
                                </div>
                            }
                        }).collect_view()}
                    </Stack>
                </div>
            </Panel>
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            branch: Some("kairos-t-0506".into()),
            commit: Some("abc1234".into()),
            pushed: true,
            pr_url: Some("https://example.com/pr/12".into()),
            next_step: Some("Wait for CI".into()),
            updated_at: "2026-10-15T10:00:00+00:00".into(),
            updated_by_name: "Robin".into(),
        }
    }

    #[test]
    fn the_rows_are_the_fields_with_a_value() {
        assert_eq!(
            state_rows(&state()),
            vec![
                ("Branch", "kairos-t-0506".to_string()),
                ("Commit", "abc1234".to_string()),
                ("Pushed", "yes".to_string()),
                ("PR", "https://example.com/pr/12".to_string()),
                ("Next step", "Wait for CI".to_string()),
            ]
        );
        let mut next_only = state();
        next_only.branch = None;
        next_only.commit = None;
        next_only.pushed = false;
        next_only.pr_url = None;
        assert_eq!(
            state_rows(&next_only),
            vec![("Next step", "Wait for CI".to_string())]
        );
    }

    #[test]
    fn the_caption_names_who_and_the_day() {
        assert_eq!(state_caption(&state()), "updated 2026-10-15 by Robin");
    }
}
