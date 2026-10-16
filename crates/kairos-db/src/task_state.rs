//! The live state of a task (KAIROS-T-0506, COLLIERY-I-0629).
//!
//! A task has one current state, separate from its append-only notes: the
//! branch, the last commit, pushed or not, the PR, the next step, and who
//! wrote it when. An agent writes it after each push and before each stop;
//! everyone reads it on the card, on the item page and in `get_item`.
//!
//! One row for each task at most ([`set`] replaces the row). No history:
//! the notes hold the record. The row stays when the task reaches a done
//! column, and it goes with the task when the retention sweep deletes the
//! task. The state is not content: a write makes no new version of the
//! task, and it emits no event.

use std::collections::HashMap;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use uuid::Uuid;

use crate::models::claims::{NewTaskState, TaskState};

/// The fields of one write of the state. Each field is optional: the write
/// replaces the row, so a field that is not sent is cleared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateWrite {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub pushed: bool,
    pub pr_url: Option<String>,
    pub next_step: Option<String>,
}

impl StateWrite {
    /// Trim each text; an empty text is no value.
    pub fn cleaned(self) -> Self {
        let clean =
            |text: Option<String>| text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        StateWrite {
            branch: clean(self.branch),
            commit: clean(self.commit),
            pushed: self.pushed,
            pr_url: clean(self.pr_url),
            next_step: clean(self.next_step),
        }
    }

    /// True when no field has a value and `pushed` is false.
    pub fn is_empty(&self) -> bool {
        self.branch.is_none()
            && self.commit.is_none()
            && !self.pushed
            && self.pr_url.is_none()
            && self.next_step.is_none()
    }
}

/// Write the state of a task: the row is replaced, and `updated_at` is
/// now. Returns the row.
pub fn set(
    conn: &mut PgConnection,
    task_id: Uuid,
    write: StateWrite,
    actor: Uuid,
) -> Result<TaskState, DieselError> {
    use crate::schema::task_state::dsl;
    let write = write.cleaned();
    let row = NewTaskState {
        task_id,
        branch: write.branch,
        commit: write.commit,
        pushed: write.pushed,
        pr_url: write.pr_url,
        next_step: write.next_step,
        updated_by: actor,
    };
    diesel::insert_into(dsl::task_state)
        .values(&row)
        .on_conflict(dsl::task_id)
        .do_update()
        .set((&row, dsl::updated_at.eq(diesel::dsl::now)))
        .returning(TaskState::as_select())
        .get_result(conn)
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
    fn a_write_trims_and_drops_empty_texts() {
        let write = StateWrite {
            branch: Some(" main ".into()),
            commit: Some("  ".into()),
            pushed: true,
            pr_url: None,
            next_step: Some("Run the tests.".into()),
        }
        .cleaned();
        assert_eq!(write.branch.as_deref(), Some("main"));
        assert_eq!(write.commit, None);
        assert!(write.pushed);
        assert_eq!(write.next_step.as_deref(), Some("Run the tests."));
        assert!(!write.is_empty());
        assert!(StateWrite::default().is_empty());
    }
}
