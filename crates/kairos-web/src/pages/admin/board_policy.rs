//! The claim flags of a column and the policy of a board, on
//! `/admin/boards/:board` (KAIROS-T-0587).
//!
//! - [`ColumnFlagSwitch`]: the switches `claims` and `keeps_claims` of a
//!   column row (KAIROS-T-0359, KAIROS-A-0034), beside the done flag and
//!   the WIP limit.
//! - [`BoardPolicyPanel`]: the policy of the board by its level:
//!   `parent_gate` on a delivery board (KAIROS-A-0040), `owner_gate` on an
//!   initiative board (KAIROS-A-0042). The other levels have no policy, and
//!   the panel is not there.
//!
//! A switch sends one field (`PATCH`); a refusal turns the switch back.

use aurora_dark::components::{Panel, Stack, Switch, Text};
use leptos::prelude::*;

use super::api::{self, BoardPolicy, ColumnFlag};
use super::{MutationOutcome, run_mutation};
use crate::auth::use_auth;

/// The label of the switch of a column flag. Pure, host-tested.
pub(crate) fn flag_label(flag: ColumnFlag) -> &'static str {
    match flag {
        ColumnFlag::Claims => "Gives claims",
        ColumnFlag::KeepsClaims => "Keeps claims",
    }
}

/// What the page says after a change of a column flag. Pure, host-tested.
pub(crate) fn flag_message(flag: ColumnFlag, on: bool) -> String {
    match (flag, on) {
        (ColumnFlag::Claims, true) => "The column gives claims now. A person who moves a free \
                                       task into the column gets its claim."
            .to_string(),
        (ColumnFlag::Claims, false) => "The column gives no claims now. Kairos ended the claims \
                                        of the tasks in the column, unless the column keeps \
                                        claims."
            .to_string(),
        (ColumnFlag::KeepsClaims, true) => "The column keeps claims now. A task keeps its claim \
                                            in the column."
            .to_string(),
        (ColumnFlag::KeepsClaims, false) => "The column does not keep claims now. Kairos ended \
                                             the claims of the tasks in the column, unless the \
                                             column gives claims."
            .to_string(),
    }
}

/// The title, the caption and the switch label of the panel of a policy.
/// Pure, host-tested.
pub(crate) fn policy_words(policy: BoardPolicy) -> (&'static str, &'static str, &'static str) {
    match policy {
        BoardPolicy::ParentGate => (
            "Board policy: parent gate",
            "When the policy is on, a task moves into a column that gives claims only while \
             its initiative is active. An initiative in Active or Monitoring is active. A \
             bucket initiative is always active. A task with no initiative moves freely.",
            "A task needs an active initiative",
        ),
        BoardPolicy::OwnerGate => (
            "Board policy: owner gate",
            "When the policy is on, an initiative moves to Active only with an owner. The \
             owner of an initiative in Active or Monitoring cannot release it. A hand-off \
             changes the owner.",
            "An initiative needs an owner",
        ),
    }
}

/// What the page says after a change of a policy. Pure, host-tested.
pub(crate) fn policy_message(policy: BoardPolicy, on: bool) -> String {
    let name = match policy {
        BoardPolicy::ParentGate => "parent gate",
        BoardPolicy::OwnerGate => "owner gate",
    };
    let state = if on { "on" } else { "off" };
    format!("Kairos turned the {name} of the board {state}.")
}

/// One switch of a column flag. The switch shows the value of the column;
/// a click sends the new value, and a refusal turns the switch back.
#[component]
pub(crate) fn ColumnFlagSwitch(
    board_id: String,
    column_id: String,
    flag: ColumnFlag,
    on: bool,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let checked = RwSignal::new(on);
    let ids = StoredValue::new((board_id, column_id));
    let on_change = Callback::new(move |value: bool| {
        let (board_id, column_id) = ids.get_value();
        run_mutation(
            busy,
            outcome,
            reload,
            flag_message(flag, value),
            async move {
                let result = api::set_column_flag(auth, &board_id, &column_id, flag, value)
                    .await
                    .map(|_| ());
                if result.is_err() {
                    checked.set(!value);
                }
                result
            },
        );
    });
    let testid = match flag {
        ColumnFlag::Claims => "column-claims",
        ColumnFlag::KeepsClaims => "column-keeps-claims",
    };
    view! {
        <span data-testid=testid>
            <Switch checked label=flag_label(flag) disabled=busy on_change/>
        </span>
    }
}

/// The policy of the board, by its level. Nothing for a level with no
/// policy.
#[component]
pub(crate) fn BoardPolicyPanel(
    board: api::Board,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    BoardPolicy::for_level(&board.board_level).map(|policy| {
        let (title, caption, label) = policy_words(policy);
        let checked = RwSignal::new(policy.of(&board));
        let board_id = StoredValue::new(board.id.clone());
        let on_change = Callback::new(move |value: bool| {
            let board_id = board_id.get_value();
            run_mutation(
                busy,
                outcome,
                reload,
                policy_message(policy, value),
                async move {
                    let result = api::set_board_policy(auth, &board_id, policy, value)
                        .await
                        .map(|_| ());
                    if result.is_err() {
                        checked.set(!value);
                    }
                    result
                },
            );
        });
        view! {
            <Panel title=title caption=caption>
                <Stack gap="xs">
                    <span data-testid=policy.field()>
                        <Switch checked label=label disabled=busy on_change/>
                    </span>
                    <Text dimmed=true size="xs">
                        "An agent and the command line set the same policy."
                    </Text>
                </Stack>
            </Panel>
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_flag_has_its_label_and_its_messages() {
        assert_eq!(flag_label(ColumnFlag::Claims), "Gives claims");
        assert_eq!(flag_label(ColumnFlag::KeepsClaims), "Keeps claims");
        assert!(flag_message(ColumnFlag::Claims, true).starts_with("The column gives claims now."));
        assert!(flag_message(ColumnFlag::Claims, false).contains("Kairos ended the claims"));
        assert!(
            flag_message(ColumnFlag::KeepsClaims, true).starts_with("The column keeps claims now.")
        );
        assert!(flag_message(ColumnFlag::KeepsClaims, false).contains("unless the column gives"));
    }

    #[test]
    fn each_policy_has_its_words() {
        assert_eq!(
            policy_words(BoardPolicy::ParentGate).0,
            "Board policy: parent gate"
        );
        assert_eq!(
            policy_words(BoardPolicy::OwnerGate).0,
            "Board policy: owner gate"
        );
        assert_eq!(
            policy_message(BoardPolicy::ParentGate, false),
            "Kairos turned the parent gate of the board off."
        );
        assert_eq!(
            policy_message(BoardPolicy::OwnerGate, true),
            "Kairos turned the owner gate of the board on."
        );
    }

    /// The texts keep the rule of the GUI: sentences of 20 words or fewer.
    #[test]
    fn the_texts_have_short_sentences() {
        let mut texts: Vec<String> = Vec::new();
        for flag in [ColumnFlag::Claims, ColumnFlag::KeepsClaims] {
            texts.push(flag_message(flag, true));
            texts.push(flag_message(flag, false));
        }
        for policy in [BoardPolicy::ParentGate, BoardPolicy::OwnerGate] {
            let (title, caption, label) = policy_words(policy);
            texts.extend([title.to_string(), caption.to_string(), label.to_string()]);
        }
        for text in texts {
            for sentence in text.split(". ") {
                let words = sentence.split_whitespace().count();
                assert!(words <= 20, "{words} words: {sentence}");
            }
        }
    }
}
