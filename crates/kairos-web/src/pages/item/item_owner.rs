//! The owner of a strategy or an initiative on the item page
//! (KAIROS-T-0411, KAIROS-A-0025).
//!
//! One person at most answers for a strategy or an initiative: its owner.
//! [`OwnerPanel`] shows the owner and the controls that the signed-in
//! person may use:
//!
//! - Claim: a member of the board (a grant on it) or an org admin takes an
//!   item that has no owner.
//! - Hand off: the owner or an org admin gives the item to a different
//!   member of the board (a grant on it). An org admin also sets the owner
//!   of an item with no owner with it.
//! - Release: the owner gives up the item.
//! - Clear: an org admin removes the owner of a different person.
//!
//! The owner stays in each column and gives no right. The server stays the
//! authority: a refusal shows in the panel.

use aurora_dark::components::{Alert, Button, Group, Panel, Select, Stack, Text};
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;

use super::api::{self, BoardInfo, Family, Owner};
use crate::auth::use_auth;

/// mirror of: `kairos_client::types_org::BoardMember` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct Member {
    user_id: String,
    email: String,
    display_name: String,
}

/// The text of the owner: who, and since when. Pure, host-tested.
pub(crate) fn owner_text(owner: &Owner) -> String {
    let since = owner.claimed_at.get(..10).unwrap_or(&owner.claimed_at);
    if since.is_empty() {
        format!("{} is the owner.", owner.display_name)
    } else {
        format!("{} is the owner since {since}.", owner.display_name)
    }
}

/// The text when the item has no owner.
pub(crate) const NO_OWNER: &str = "No owner. A member of the board can claim it.";

/// What the signed-in person may do with the owner. Pure, host-tested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OwnerPowers {
    pub claim: bool,
    pub hand_off: bool,
    pub release: bool,
    pub clear: bool,
}

/// The rules of the server (`kairos_server::owners`), mirrored: a member
/// of the board or an admin claims an item with no owner; the owner or an
/// admin hands it off; the owner releases it; an admin clears the owner of
/// a different person.
pub(crate) fn owner_powers(
    me: &str,
    admin: bool,
    member: bool,
    owner: Option<&str>,
) -> OwnerPowers {
    let is_owner = !me.is_empty() && owner == Some(me);
    OwnerPowers {
        claim: owner.is_none() && (member || admin),
        hand_off: is_owner || admin,
        release: is_owner,
        clear: owner.is_some() && admin && !is_owner,
    }
}

/// Is the signed-in person a member of the board (a grant on it)?
pub(crate) fn is_board_member(me: &crate::api::Whoami, board_slug: &str) -> bool {
    me.capabilities
        .iter()
        .any(|board| board.board_slug == board_slug && !board.grants.is_empty())
}

/// The members the item can go to: each member of the board that is not
/// the owner, as `(user id, label)`. Pure, host-tested.
fn hand_off_options(members: &[Member], owner: Option<&str>) -> Vec<(String, String)> {
    members
        .iter()
        .filter(|m| Some(m.user_id.as_str()) != owner)
        .map(|m| {
            (
                m.user_id.clone(),
                format!("{} ({})", m.display_name, m.email),
            )
        })
        .collect()
}

/// The owner panel of a strategy or an initiative.
#[component]
pub fn OwnerPanel(
    family: Family,
    #[prop(into)] code: String,
    owner: Option<Owner>,
    /// The page's shared board read.
    board: LocalResource<Result<Option<BoardInfo>, ApiError>>,
    archived: bool,
    on_moved: Callback<String>,
) -> impl IntoView {
    let owner = StoredValue::new(owner);
    let code = StoredValue::new(code);
    let whoami = use_context::<LocalResource<Result<crate::api::Whoami, ApiError>>>();
    let powers = Memo::new(move |_| {
        let Some(me) = whoami
            .and_then(|resource| resource.get())
            .and_then(Result::ok)
        else {
            return OwnerPowers::default();
        };
        let member = match board.get() {
            Some(Ok(Some(board))) => is_board_member(&me, &board.slug),
            _ => false,
        };
        owner.with_value(|o| {
            owner_powers(
                &me.user.id,
                me.organization.role == "admin",
                member,
                o.as_ref().map(|o| o.user_id.as_str()),
            )
        })
    });
    view! {
        <Panel title="Owner" caption="who answers for the item">
            <div class="kairos-item__owner" data-testid="owner-panel">
                <Stack gap="sm">
                    {match owner.with_value(|o| o.as_ref().map(owner_text)) {
                        Some(text) => view! {
                            <Text size="sm" bright=true>{text}</Text>
                        }.into_any(),
                        None => view! {
                            <Text size="sm" dimmed=true>{NO_OWNER}</Text>
                        }.into_any(),
                    }}
                    {move || (!archived && powers.get() != OwnerPowers::default()).then(|| view! {
                        <OwnerControls
                            family
                            code=code.get_value()
                            board_slug=match board.get() {
                                Some(Ok(Some(board))) => Some(board.slug),
                                _ => None,
                            }
                            owner=owner.with_value(|o| o.as_ref().map(|o| o.user_id.clone()))
                            powers=powers.get()
                            on_moved
                        />
                    })}
                </Stack>
            </div>
        </Panel>
    }
}

/// Claim, Hand off, Release and Clear.
#[component]
fn OwnerControls(
    family: Family,
    code: String,
    /// The board of the item: a new owner is a member of it.
    board_slug: Option<String>,
    owner: Option<String>,
    powers: OwnerPowers,
    on_moved: Callback<String>,
) -> impl IntoView {
    let auth = use_auth();
    let code = StoredValue::new(code);
    let owner = StoredValue::new(owner);
    let to = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<ApiError>);
    let members = LocalResource::new(move || {
        auth.track_session();
        let board_slug = board_slug.clone();
        async move {
            match (powers.hand_off, board_slug.as_deref()) {
                (true, Some(slug)) => {
                    crate::api::get_json::<Vec<Member>>(
                        auth,
                        &format!("/api/boards/{slug}/members"),
                    )
                    .await
                }
                _ => Ok(Vec::new()),
            }
        }
    });
    let name_of = |response: api::OwnerResponse| {
        response
            .owner
            .map(|o| o.display_name)
            .unwrap_or_else(|| "the person".to_string())
    };
    let claim: Callback<()> = Callback::new(move |()| {
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match api::claim_owner(auth, family, &code.get_value()).await {
                Ok(_) => on_moved.run("You are the owner.".to_string()),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    });
    let hand_off: Callback<()> = Callback::new(move |()| {
        let person = to.get_untracked();
        if person.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match api::hand_off_owner(auth, family, &code.get_value(), &person).await {
                Ok(response) => {
                    on_moved.run(format!("{} is the owner.", name_of(response)));
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
            match api::release_owner(auth, family, &code.get_value()).await {
                Ok(_) => on_moved.run("The item has no owner.".to_string()),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    });
    view! {
        <Stack gap="xs">
            {powers.claim.then(|| view! {
                <Group gap="sm">
                    <Button size="xs" disabled=busy on_click=claim>"Claim"</Button>
                </Group>
            })}
            {move || powers.hand_off.then(|| match members.get() {
                None => view! { <Text size="xs" dimmed=true>"Loading members…"</Text> }.into_any(),
                Some(Err(_)) => view! {
                    <Text size="xs" dimmed=true>"The members of the board are not available."</Text>
                }.into_any(),
                Some(Ok(list)) => {
                    let options = owner.with_value(|o| hand_off_options(&list, o.as_deref()));
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
            })}
            {powers.release.then(|| view! {
                <Group gap="sm">
                    <Button variant="default" size="xs" disabled=busy on_click=release>
                        "Release"
                    </Button>
                </Group>
            })}
            {powers.clear.then(|| view! {
                <Group gap="sm">
                    <Button variant="default" size="xs" disabled=busy on_click=release>
                        "Clear owner"
                    </Button>
                </Group>
            })}
            {move || error.get().map(|e| view! {
                <Alert title="Could not change the owner" color=token::BAD>
                    <Text size="sm">{api::error_text(&e)}</Text>
                </Alert>
            })}
        </Stack>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_names_the_owner_and_the_day() {
        let owner = Owner {
            user_id: "u1".to_string(),
            display_name: "Robin".to_string(),
            claimed_at: "2026-10-12T10:00:00Z".to_string(),
        };
        assert_eq!(owner_text(&owner), "Robin is the owner since 2026-10-12.");
    }

    #[test]
    fn the_powers_follow_the_rules_of_the_server() {
        // A member claims an item with no owner; nobody claims an owned one.
        assert!(owner_powers("u1", false, true, None).claim);
        assert!(!owner_powers("u1", false, true, Some("u2")).claim);
        assert!(!owner_powers("u1", false, false, None).claim);
        assert!(owner_powers("u1", true, false, None).claim);
        // The owner and an admin hand off; a member does not.
        assert!(owner_powers("u1", false, false, Some("u1")).hand_off);
        assert!(!owner_powers("u2", false, true, Some("u1")).hand_off);
        assert!(owner_powers("u2", true, false, Some("u1")).hand_off);
        // The owner releases; a member does not; an admin who is the owner
        // releases too.
        assert!(owner_powers("u1", false, true, Some("u1")).release);
        assert!(!owner_powers("u2", false, true, Some("u1")).release);
        assert!(owner_powers("u1", true, false, Some("u1")).release);
        // An admin clears the owner of a different person, and only an
        // owned item.
        assert!(!owner_powers("u1", false, true, Some("u1")).clear);
        assert!(!owner_powers("u1", true, false, Some("u1")).clear);
        assert!(owner_powers("u2", true, false, Some("u1")).clear);
        assert!(!owner_powers("u2", true, false, None).clear);
        assert_eq!(owner_powers("", false, false, None), OwnerPowers::default());
    }

    #[test]
    fn the_options_leave_out_the_owner() {
        let members = vec![
            Member {
                user_id: "u1".to_string(),
                email: "robin@example.test".to_string(),
                display_name: "Robin".to_string(),
            },
            Member {
                user_id: "u2".to_string(),
                email: "sam@example.test".to_string(),
                display_name: "Sam".to_string(),
            },
        ];
        assert_eq!(
            hand_off_options(&members, Some("u1")),
            vec![("u2".to_string(), "Sam (sam@example.test)".to_string())]
        );
        assert_eq!(hand_off_options(&members, None).len(), 2);
    }
}
