//! The live state of a task (KAIROS-T-0506, COLLIERY-I-0629).
//!
//! A task has one current state, separate from its append-only notes: the
//! branch, the last commit, pushed or not, the PR, the next step, and who
//! wrote it when. An agent writes it after each push and before each stop;
//! everyone reads it on the card, on the item page and in `get_item`.
//!
//! One row for each task at most; a write is a patch ([`patch`]): a field
//! that is not sent is kept, `null` clears it (KAIROS-T-0533). No history:
//! the notes hold the record. The row stays when the task reaches a done
//! column; a hand-off or a release clears the next step
//! ([`clear_next_step`]). The state is not content: a write makes no new
//! version of the task; the server emits `item_updated` so the cards
//! refresh, then `task_state_changed` (KAIROS-T-0574).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use uuid::Uuid;

use crate::models::claims::{NewTaskState, TaskState};

/// One field of a patch (KAIROS-T-0533): kept as it is, cleared, or set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Field<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl Field<String> {
    /// A text field from the wire: absent keeps, `null` clears, a text sets
    /// it trimmed; an empty text clears.
    pub fn of(wire: Option<Option<String>>) -> Self {
        match wire {
            None => Field::Keep,
            Some(None) => Field::Clear,
            Some(Some(text)) => {
                let text = text.trim();
                if text.is_empty() {
                    Field::Clear
                } else {
                    Field::Set(text.to_string())
                }
            }
        }
    }

    fn apply(&self, current: Option<String>) -> Option<String> {
        match self {
            Field::Keep => current,
            Field::Clear => None,
            Field::Set(text) => Some(text.clone()),
        }
    }
}

/// One write of the state (KAIROS-T-0533): a patch. A field that is not
/// sent is kept, so an agent that writes `next_step` alone does not erase
/// the branch and the PR. `expected_updated_at` is the stamp of the state
/// the writer read; a state that changed since refuses the write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatePatch {
    pub branch: Field<String>,
    pub commit: Field<String>,
    pub pushed: Option<bool>,
    pub pr_url: Field<String>,
    pub next_step: Field<String>,
    pub expected_updated_at: Option<DateTime<Utc>>,
}

impl StatePatch {
    /// True when the patch changes nothing.
    pub fn is_empty(&self) -> bool {
        self.branch == Field::Keep
            && self.commit == Field::Keep
            && self.pushed.is_none()
            && self.pr_url == Field::Keep
            && self.next_step == Field::Keep
    }
}

/// Why a patch did not land.
#[derive(Debug)]
pub enum PatchError {
    /// The state changed since the writer read it: here is the current one.
    Conflict(Box<TaskState>),
    Database(DieselError),
}

impl From<DieselError> for PatchError {
    fn from(error: DieselError) -> Self {
        PatchError::Database(error)
    }
}

/// Apply `patch` to the state of a task: the row is read for update,
/// merged, and written with `updated_at` now. Returns the row.
pub fn patch(
    conn: &mut PgConnection,
    task_id: Uuid,
    patch: StatePatch,
    actor: Uuid,
) -> Result<TaskState, PatchError> {
    use crate::schema::task_state::dsl;
    conn.transaction::<_, PatchError, _>(|conn| {
        let current: Option<TaskState> = dsl::task_state
            .filter(dsl::task_id.eq(task_id))
            .select(TaskState::as_select())
            .for_update()
            .first(conn)
            .optional()?;
        if let Some(expected) = patch.expected_updated_at
            && let Some(current) = &current
            && current.updated_at != expected
        {
            return Err(PatchError::Conflict(Box::new(current.clone())));
        }
        let (branch, commit, pushed, pr_url, next_step) = match &current {
            Some(c) => (
                c.branch.clone(),
                c.commit.clone(),
                c.pushed,
                c.pr_url.clone(),
                c.next_step.clone(),
            ),
            None => (None, None, false, None, None),
        };
        let row = NewTaskState {
            task_id,
            branch: patch.branch.apply(branch),
            commit: patch.commit.apply(commit),
            pushed: patch.pushed.unwrap_or(pushed),
            pr_url: patch.pr_url.apply(pr_url),
            next_step: patch.next_step.apply(next_step),
            updated_by: actor,
        };
        Ok(diesel::insert_into(dsl::task_state)
            .values(&row)
            .on_conflict(dsl::task_id)
            .do_update()
            .set((&row, dsl::updated_at.eq(diesel::dsl::now)))
            .returning(TaskState::as_select())
            .get_result(conn)?)
    })
}

/// Clear the next step of a task (KAIROS-T-0533): a hand-off or a release
/// ends the plan of the previous claimant; the branch, the commit and the
/// PR stay, they are facts of the work. Nothing when the task has no state.
/// A clear emits `task_state_changed` (KAIROS-T-0574) with `actor`.
pub fn clear_next_step(
    conn: &mut PgConnection,
    task_id: Uuid,
    actor: Uuid,
) -> Result<usize, DieselError> {
    use crate::schema::task_state::dsl;
    let cleared = diesel::update(
        dsl::task_state
            .filter(dsl::task_id.eq(task_id))
            .filter(dsl::next_step.is_not_null()),
    )
    .set((
        dsl::next_step.eq(None::<String>),
        dsl::updated_at.eq(diesel::dsl::now),
    ))
    .execute(conn)?;
    if cleared > 0 {
        crate::events::emit_item_event_by_id(
            conn,
            crate::events::EventKind::TaskStateChanged,
            "task",
            task_id,
            actor,
        )?;
    }
    Ok(cleared)
}

/// The state of a task, or `None`.
pub fn state_of(conn: &mut PgConnection, task_id: Uuid) -> Result<Option<TaskState>, DieselError> {
    use crate::schema::task_state::dsl;
    dsl::task_state
        .filter(dsl::task_id.eq(task_id))
        .select(TaskState::as_select())
        .first(conn)
        .optional()
}

/// The states of the tasks in `task_ids`, by task. A task with no state is
/// not in the map. One query.
pub fn states_of(
    conn: &mut PgConnection,
    task_ids: &[Uuid],
) -> Result<HashMap<Uuid, TaskState>, DieselError> {
    use crate::schema::task_state::dsl;
    if task_ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(dsl::task_state
        .filter(dsl::task_id.eq_any(task_ids))
        .select(TaskState::as_select())
        .load(conn)?
        .into_iter()
        .map(|state| (state.task_id, state))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_from_the_wire_keeps_clears_or_sets_trimmed() {
        assert_eq!(Field::of(None), Field::Keep);
        assert_eq!(Field::of(Some(None)), Field::Clear);
        assert_eq!(Field::of(Some(Some("  ".into()))), Field::Clear);
        assert_eq!(
            Field::of(Some(Some(" main ".into()))),
            Field::Set("main".into())
        );
        assert_eq!(Field::Keep.apply(Some("a".into())), Some("a".into()));
        assert_eq!(Field::Clear.apply(Some("a".into())), None);
        assert_eq!(
            Field::Set("b".to_string()).apply(Some("a".into())),
            Some("b".into())
        );
    }

    #[test]
    fn a_patch_with_no_field_is_empty() {
        assert!(StatePatch::default().is_empty());
        let only_stamp = StatePatch {
            expected_updated_at: Some(Utc::now()),
            ..Default::default()
        };
        assert!(only_stamp.is_empty());
        let pushed = StatePatch {
            pushed: Some(false),
            ..Default::default()
        };
        assert!(!pushed.is_empty());
        let next = StatePatch {
            next_step: Field::Clear,
            ..Default::default()
        };
        assert!(!next.is_empty());
    }
}
