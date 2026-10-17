//! The policies of a board: rules that an admin turns on or off per board.
//!
//! Each policy is one boolean column on `boards`. There is no policy table:
//! a column per policy is the smallest store, and the board DTO reads it
//! with the board.
//!
//! # `parent_gate` (KAIROS-T-0641)
//!
//! On a delivery board with the policy on, a task moves into a column with
//! the `claims` flag (the Active column) only when its parent initiative is
//! active. The rule:
//!
//! 1. The trigger is a move into a claims column: a transition, a move to
//!    the board (its entry column), or a create into the column. The flag
//!    decides, not the name of the column.
//! 2. An initiative is active in a column named Active or Monitoring (as
//!    [`crate::my_work`] reads it). Each other column refuses, Completed
//!    too.
//! 3. A task with no parent initiative passes.
//! 4. A bucket initiative (`is_bucket`) is always active.
//! 5. The policy is on by default, for each new board and, by the
//!    migration, for each existing one. Only a delivery board reads it.
//! 6. The refusal is [`BoardError::ParentNotActive`]. Kairos does not move
//!    the initiative.
//! 7. A task in Active under an inactive initiative stays there: the rule
//!    checks moves only.
//!
//! Each function operates in the CURRENT `search_path` tenant schema.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselError;
use uuid::Uuid;

use crate::boards::BoardError;
use crate::models::enums::RelationshipType;

/// Is an initiative in this column active? A column named Active or
/// Monitoring (case and spaces aside).
pub fn is_active_initiative_column(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case("Active") || name.eq_ignore_ascii_case("Monitoring")
}

/// The `parent_gate` policy of the board. A board that does not exist reads
/// as off.
pub fn parent_gate(conn: &mut PgConnection, board_id: Uuid) -> Result<bool, DieselError> {
    use crate::schema::boards;
    Ok(boards::table
        .filter(boards::id.eq(board_id))
        .select(boards::parent_gate)
        .first::<bool>(conn)
        .optional()?
        .unwrap_or(false))
}

/// Set the `parent_gate` policy of the board. The caller checked
/// `configure_boards`. Returns true when the value changed.
pub fn set_parent_gate(
    conn: &mut PgConnection,
    board_id: Uuid,
    on: bool,
) -> Result<bool, DieselError> {
    use crate::schema::boards;
    let changed = diesel::update(
        boards::table
            .filter(boards::id.eq(board_id))
            .filter(boards::parent_gate.ne(on)),
    )
    .set((
        boards::parent_gate.eq(on),
        boards::updated_at.eq(diesel::dsl::now),
    ))
    .execute(conn)?;
    Ok(changed > 0)
}

/// Check that the task `task_id` may go into the column `to_column_id`
/// (module docs, `parent_gate`). Call it in the transaction of the move,
/// before the write, with the column the task goes to. A column that is
/// not a claims column passes.
pub fn check_active_parent(
    conn: &mut PgConnection,
    task_id: Uuid,
    to_column_id: Uuid,
) -> Result<(), BoardError> {
    use crate::schema::{board_columns, boards, initiatives, item_relationships, tasks};

    let column: Option<(Uuid, bool, String)> = board_columns::table
        .filter(board_columns::id.eq(to_column_id))
        .select((
            board_columns::board_id,
            board_columns::claims,
            board_columns::name,
        ))
        .first(conn)
        .optional()?;
    let Some((board_id, claims, to_name)) = column else {
        return Ok(());
    };
    if !claims {
        return Ok(());
    }
    let gate: Option<(bool, crate::models::enums::BoardLevel)> = boards::table
        .filter(boards::id.eq(board_id))
        .select((boards::parent_gate, boards::board_level))
        .first(conn)
        .optional()?;
    if !matches!(
        gate,
        Some((true, crate::models::enums::BoardLevel::Delivery))
    ) {
        return Ok(());
    }
    // The parent initiatives of the task: live ones only. A task has one
    // parent; old data can have more, and each must be active.
    let parents: Vec<(String, bool, String)> = item_relationships::table
        .inner_join(initiatives::table.on(initiatives::id.eq(item_relationships::source_id)))
        .inner_join(board_columns::table.on(board_columns::id.eq(initiatives::column_id)))
        .filter(item_relationships::target_id.eq(task_id))
        .filter(item_relationships::relationship.eq(RelationshipType::Parent))
        .filter(initiatives::deleted_at.is_null())
        .select((
            initiatives::short_code,
            initiatives::is_bucket,
            board_columns::name,
        ))
        .order(initiatives::short_code.asc())
        .load(conn)?;
    let Some((initiative, _, column)) = parents
        .into_iter()
        .find(|(_, bucket, column)| !bucket && !is_active_initiative_column(column))
    else {
        return Ok(());
    };
    let task: String = tasks::table
        .filter(tasks::id.eq(task_id))
        .select(tasks::short_code)
        .first(conn)?;
    Err(BoardError::ParentNotActive {
        task,
        to_column: to_name,
        initiative,
        initiative_column: column,
        board_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_and_monitoring_are_active() {
        assert!(is_active_initiative_column("Active"));
        assert!(is_active_initiative_column(" monitoring "));
        assert!(!is_active_initiative_column("Discovery"));
        assert!(!is_active_initiative_column("Completed"));
    }
}
