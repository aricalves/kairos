//! The reopen of a done task on the server (KAIROS-T-0564). The rules are
//! in `kairos_db::boards::reopen_task`; this module has the one path of
//! the two surfaces: REST `POST /api/tasks/{short_code}/reopen` and MCP
//! `reopen_item`.

use diesel::pg::PgConnection;
use kairos_db::boards::{self, Reopened};
use uuid::Uuid;

use crate::error::ApiError;

/// The capability of a reopen: the capability of a move. A reopen moves
/// the task out of the done column, as a cancel moves it in.
pub const CAPABILITY: &str = "transition_items";

/// Reopen the task: the capability check ([`CAPABILITY`] on the board of
/// the task), the reopen, and the WIP note of the column it goes to.
pub fn reopen(
    conn: &mut PgConnection,
    slug: &str,
    task_id: Uuid,
    board_id: Uuid,
    user: Uuid,
) -> Result<Reopened, ApiError> {
    crate::api::require_capability(conn, slug, Some(board_id), user, CAPABILITY)?;
    let reopened = boards::reopen_task(conn, task_id, user).map_err(crate::api::map_board_error)?;
    // KAIROS-T-0371: a move into a column over its WIP limit.
    crate::claims::note_wip(conn, reopened.to_column_id, &reopened.short_code)?;
    Ok(reopened)
}

/// The text of a reopen for an agent and the CLI: the move, and the end
/// of the cancel mark when the task had one.
pub fn reopen_text(reopened: &Reopened) -> String {
    let mark = if reopened.was_cancelled {
        " The cancel mark is removed; the history keeps the reason."
    } else {
        ""
    };
    format!(
        "Reopened {}: {} -> {}.{mark}",
        reopened.short_code, reopened.from_column, reopened.to_column
    )
}
