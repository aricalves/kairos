//! The claim of a task on the item page (KAIROS-T-0369, KAIROS-T-0359).
//!
//! The claim says who works on a task in Active. A person who moves the
//! task to Active gets it. [`ClaimPanel`] shows the claim and gives two
//! controls:
//!
//! - Hand off: give the claim to a different member of the organization.
//! - Release: end the claim. The task stays in Active with no claim.
//!
//! The holder of the claim and a person who can move the task
//! (`transition_items` on the board) see the controls. The server stays
//! the authority: a refusal shows in the panel.

use aurora_dark::components::{Alert, Button, Group, Panel, Select, Stack, Text};
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;

use super::api::{self, BoardInfo, Claim};
use crate::auth::use_auth;
use crate::pages::boards;

/// mirror of: `kairos_client::types_org::OrgMember` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct Member {
    user_id: String,
    email: String,
    display_name: String,
}

/// The text of the claim: who has it, and since when. Pure, host-tested.
pub(crate) fn claim_text(claim: &Claim) -> String {
    let who = if claim.agent {
        format!("{} (agent)", claim.display_name)
    } else {
        claim.display_name.clone()
    };
    let since = claim.claimed_at.get(..10).unwrap_or(&claim.claimed_at);
    if since.is_empty() {
        format!("{who} has the claim.")
    } else {
        format!("{who} has the claim since {since}.")
    }
}

/// The text when the task is in a claims column with no claim.
pub(crate) const NO_CLAIM: &str =
    "No claim. The next person who moves the task to Active gets the claim.";

/// May the signed-in person hand off or release the claim? The holder
/// may, and a person who can move the task may. Pure, host-tested.
pub(crate) fn may_change_claim(me: &str, holder: &str, can_move: bool) -> bool {
    can_move || (!me.is_empty() && me == holder)
}

/// The members a claim can go to: each member that does not have the
/// claim, as `(user id, label)`. Pure, host-tested.
fn hand_off_options(members: &[Member], holder: &str) -> Vec<(String, String)> {
    members
        .iter()
        .filter(|m| m.user_id != holder)
        .map(|m| {
            (
                m.user_id.clone(),
                format!("{} ({})", m.display_name, m.email),
            )
        })
        .collect()
}

/// The claim panel of a task. It shows when the task has a claim, or when
/// the task is in a column that gives a claim.
#[component]
pub fn ClaimPanel(
    #[prop(into)] code: String,
    claim: Option<Claim>,
    /// The page's shared board read.
    board: LocalResource<Result<Option<BoardInfo>, ApiError>>,
    column_id: Option<String>,
    archived: bool,
    on_moved: Callback<String>,
) -> impl IntoView {
    let claim = StoredValue::new(claim);
    let column_id = StoredValue::new(column_id);
    let code = StoredValue::new(code);
    // Show the panel when the task has a claim, or sits in a column that
    // gives one.
    let shown = Memo::new(move |_| {
        if claim.with_value(Option::is_some) {
            return true;
        }
        let Some(Ok(Some(board))) = board.get() else {
            return false;
        };
        column_id.with_value(|column| {
            board
                .columns
                .iter()
                .any(|c| Some(&c.id) == column.as_ref() && c.claims)
        })
    });
    let whoami = use_context::<LocalResource<Result<crate::api::Whoami, ApiError>>>();
    let can_change = Memo::new(move |_| {
        let Some(holder) = claim.with_value(|c| c.as_ref().map(|c| c.user_id.clone())) else {
            return false;
        };
        let Some(me) = whoami
            .and_then(|resource| resource.get())
            .and_then(Result::ok)
        else {
            return false;
        };
        let can_move = match board.get() {
            Some(Ok(Some(board))) => {
                boards::board_powers(&me, &board.slug, board.team_id.as_deref(), None).transition
            }
            _ => false,
        };
        may_change_claim(&me.user.id, &holder, can_move)
    });
    view! {
        {move || shown.get().then(|| view! {
            <Panel title="Claim" caption="who works on the task">
                <div class="kairos-item__claim" data-testid="claim-panel">
                    <Stack gap="sm">
                        {match claim.with_value(|c| c.as_ref().map(claim_text)) {
                            Some(text) => view! {
                                <Text size="sm" bright=true>{text}</Text>
                            }.into_any(),
                            None => view! {
                                <Text size="sm" dimmed=true>{NO_CLAIM}</Text>
                            }.into_any(),
                        }}
                        {move || (can_change.get() && !archived).then(|| view! {
                            <ClaimControls
                                code=code.get_value()
                                holder=claim.with_value(|c| {
                                    c.as_ref().map(|c| c.user_id.clone()).unwrap_or_default()
                                })
                                on_moved
                            />
                        })}
                    </Stack>
                </div>
            </Panel>
        })}
    }
}

/// Hand off and Release.
#[component]
fn ClaimControls(code: String, holder: String, on_moved: Callback<String>) -> impl IntoView {
    let auth = use_auth();
    let code = StoredValue::new(code);
    let holder = StoredValue::new(holder);
    let to = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<ApiError>);
    let members = LocalResource::new(move || {
        let _ = auth.token();
        async move { crate::api::get_all::<Member>(auth, "/api/members").await }
    });
    let hand_off: Callback<()> = Callback::new(move |()| {
        let person = to.get_untracked();
        if person.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match api::hand_off_task(auth, &code.get_value(), &person).await {
                Ok(task) => {
                    let name = task
                        .claim
                        .map(|c| c.display_name)
                        .unwrap_or_else(|| "the person".to_string());
                    on_moved.run(format!("Kairos gave the claim to {name}."));
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    });
    let release: Callback<()> = Callback::new(move |()| {
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match api::release_task(auth, &code.get_value()).await {
                Ok(_) => on_moved.run(
                    "Kairos released the claim. The task stays in Active with no claim."
                        .to_string(),
                ),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    });
    view! {
        <Stack gap="xs">
            {move || match members.get() {
                None => view! { <Text size="xs" dimmed=true>"Loading members…"</Text> }.into_any(),
                Some(Err(_)) => view! {
                    <Text size="xs" dimmed=true>"The members are not available."</Text>
                }.into_any(),
                Some(Ok(list)) => {
                    let options = holder.with_value(|holder| hand_off_options(&list, holder));
                    if to.get_untracked().is_empty()
                        && let Some((first, _)) = options.first()
                    {
                        to.set(first.clone());
                    }
                    let empty = options.is_empty();
                    view! {
                        <Group align="end" gap="sm" wrap=true>
                            <Select label="Hand off to" option_pairs=options value=to/>
                            <Button size="xs" disabled=move || busy.get() || empty
                                on_click=hand_off>
                                "Hand off"
                            </Button>
                        </Group>
                    }.into_any()
                }
            }}
            <Group gap="sm">
                <Button variant="default" size="xs" disabled=busy on_click=release>
                    "Release"
                </Button>
            </Group>
            {move || error.get().map(|e| view! {
                <Alert title="Could not change the claim" color=token::BAD>
                    <Text size="sm">{api::error_text(&e)}</Text>
                </Alert>
            })}
        </Stack>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(agent: bool) -> Claim {
        Claim {
            user_id: "u1".to_string(),
            display_name: "Alice".to_string(),
            agent,
            claimed_at: "2026-10-09T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn the_text_names_the_holder_and_the_day() {
        assert_eq!(
            claim_text(&claim(false)),
            "Alice has the claim since 2026-10-09."
        );
        assert_eq!(
            claim_text(&claim(true)),
            "Alice (agent) has the claim since 2026-10-09."
        );
    }

    #[test]
    fn the_holder_or_a_mover_may_change_the_claim() {
        assert!(may_change_claim("u1", "u1", false));
        assert!(may_change_claim("u2", "u1", true));
        assert!(!may_change_claim("u2", "u1", false));
        assert!(!may_change_claim("", "", false));
    }

    #[test]
    fn the_options_leave_out_the_holder() {
        let members = vec![
            Member {
                user_id: "u1".to_string(),
                email: "a@example.test".to_string(),
                display_name: "Alice".to_string(),
            },
            Member {
                user_id: "u2".to_string(),
                email: "b@example.test".to_string(),
                display_name: "Bob".to_string(),
            },
        ];
        assert_eq!(
            hand_off_options(&members, "u1"),
            vec![("u2".to_string(), "Bob (b@example.test)".to_string())]
        );
    }
}
