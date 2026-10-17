//! Who decides the triage of intake (KAIROS-T-0428): a person, or also an
//! agent when the board allows it.
//!
//! Intake is a task in the support lane in the entry column of its board: a
//! request of another team, or support work. Triage decides it: into the
//! planned lane (planned work), or out of the entry column in the support
//! lane (work now), or to the board of another team, or cancelled (won't
//! do). A person reviews that decision. An agent proposes it with a note
//! (`append_note`) and leaves the move to a person, unless the board
//! setting `agents_decide` is on. Once a person has triaged the task out of
//! the entry column, an agent moves it as any other task (KAIROS-T-0456).
//!
//! An agent is a caller with an agent key (the key of a person, used by
//! their agent) or a service account. The check runs in the transaction of
//! the move, so REST and MCP give the same answer.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{Bool, Uuid as SqlUuid};
use kairos_db::models::enums::{ActivityAction, WorkClass};
use kairos_db::models::graph::NewActivityLogEntry;
use serde_json::json;
use uuid::Uuid;

use crate::error::ApiError;
use crate::middleware::auth::AuthContext;

/// A triage move of a task in the support lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriageMove {
    /// Into the planned lane.
    Plan,
    /// Into another column.
    Column(Uuid),
    /// To the board of another team.
    Board(Uuid),
    /// Cancelled: won't do.
    Cancel,
}

/// A triage move that [`check`] let through. The caller writes the
/// activity line with [`record`] once the move is done, so a move that the
/// board refuses after the check (the transition graph, a WIP limit) leaves
/// no line.
#[derive(Debug)]
#[must_use = "record the triage after the move"]
pub(crate) struct Triaged(String);

/// Whether the caller is an agent: an agent key, or a service account.
pub(crate) fn is_agent(conn: &mut PgConnection, auth: &AuthContext) -> Result<bool, ApiError> {
    if auth.agent_key.is_some() {
        return Ok(true);
    }
    use kairos_db::schema::users::dsl;
    let kind: Option<String> = dsl::users
        .filter(dsl::id.eq(auth.user_id))
        .select(dsl::kind)
        .first(conn)
        .optional()
        .map_err(ApiError::internal)?;
    Ok(kind.as_deref() == Some(kairos_db::models::USER_KIND_SERVICE_ACCOUNT))
}

#[derive(QueryableByName)]
struct Decides {
    #[diesel(sql_type = Bool)]
    agents_decide: bool,
}

/// The setting of the board: true when agents decide triage.
pub(crate) fn agents_decide(conn: &mut PgConnection, board_id: Uuid) -> Result<bool, ApiError> {
    let row: Option<Decides> =
        sql_query("SELECT agents_decide FROM board_agent_triage WHERE board_id = $1")
            .bind::<SqlUuid, _>(board_id)
            .get_result(conn)
            .optional()
            .map_err(ApiError::internal)?;
    Ok(row.is_some_and(|row| row.agents_decide))
}

/// Set the setting of the board. The caller checked `configure_boards`.
/// A value that the board has already writes nothing.
pub(crate) fn set_agents_decide(
    conn: &mut PgConnection,
    board_id: Uuid,
    decide: bool,
    user: Uuid,
) -> Result<bool, ApiError> {
    if agents_decide(conn, board_id)? == decide {
        return Ok(decide);
    }
    sql_query(
        "INSERT INTO board_agent_triage (board_id, agents_decide, updated_by) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (board_id) DO UPDATE SET agents_decide = $2, updated_by = $3, \
             updated_at = now()",
    )
    .bind::<SqlUuid, _>(board_id)
    .bind::<Bool, _>(decide)
    .bind::<SqlUuid, _>(user)
    .execute(conn)
    .map_err(ApiError::internal)?;
    log(
        conn,
        user,
        ActivityAction::BoardConfig,
        board_id,
        "board",
        format!("agents_decide_triage:{decide}"),
    )?;
    // KAIROS-T-0574.
    kairos_db::events::emit_board_configured(conn, board_id, user, "agents_decide_triage")
        .map_err(ApiError::internal)?;
    Ok(decide)
}

#[derive(QueryableByName)]
struct TaskFacts {
    #[diesel(sql_type = Bool)]
    support: bool,
    #[diesel(sql_type = SqlUuid)]
    board_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    column_id: Uuid,
}

/// Check a move of the task `task_id` by `auth`. A person, a task in the
/// planned lane, or a move that is not triage passes with `None`. Triage
/// is the decision on intake: a task in the support lane that is in the
/// entry column of its board. A column move out of the entry column (or
/// back into it), a cancel, and a move to another board are triage; a lane
/// change to planned is triage from any column. A column move of a task
/// that a person triaged out of the entry column is not: an agent moves
/// its support work from Todo to Active and on to the done column.
///
/// An agent's triage passes only when the board allows it; then the
/// caller gets `Some`, and writes the line of the activity log with
/// [`record`] after the move.
pub(crate) fn check(
    conn: &mut PgConnection,
    auth: &AuthContext,
    task_id: Uuid,
    triage: TriageMove,
) -> Result<Option<Triaged>, ApiError> {
    let facts: TaskFacts = sql_query(
        "SELECT work_class = 'support' AS support, board_id, column_id FROM tasks WHERE id = $1",
    )
    .bind::<SqlUuid, _>(task_id)
    .get_result(conn)
    .map_err(ApiError::internal)?;
    if !facts.support {
        return Ok(None);
    }
    let entry =
        kairos_db::boards::entry_column(conn, facts.board_id).map_err(ApiError::internal)?;
    let in_entry = entry == Some(facts.column_id);
    let is_triage = match triage {
        TriageMove::Plan => true,
        TriageMove::Column(to) => to != facts.column_id && (in_entry || entry == Some(to)),
        TriageMove::Board(_) | TriageMove::Cancel => in_entry,
    };
    if !is_triage {
        return Ok(None);
    }
    if !is_agent(conn, auth)? {
        return Ok(None);
    }
    if !agents_decide(conn, facts.board_id)? {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "AGENT_TRIAGE_OFF",
            "This task is intake (support lane). On this board an agent proposes the triage \
             and a person decides it: write the proposal (planned backlog, work now, another \
             team, or won't do, and why) as a note with append_note, and leave the task. A \
             person with configure_boards can let agents decide: the MCP tool set_agent_triage \
             (admin profile), `kairos boards dispatch agent-triage <board> --on`, or the Agent \
             dispatch panel of the board.",
        )
        .with_details(json!({ "board_id": facts.board_id, "setting": "agents_decide" })));
    }
    let what = match triage {
        TriageMove::Plan => "planned".to_string(),
        TriageMove::Column(to) => format!("column:{to}"),
        TriageMove::Board(to) => format!("board:{to}"),
        TriageMove::Cancel => "cancelled".to_string(),
    };
    Ok(Some(Triaged(format!("agent_triage:{what}"))))
}

/// Write the line of the activity log for a triage that [`check`] let
/// through, after the move.
pub(crate) fn record(
    conn: &mut PgConnection,
    auth: &AuthContext,
    task_id: Uuid,
    triaged: Option<Triaged>,
) -> Result<(), ApiError> {
    match triaged {
        Some(Triaged(details)) => log(
            conn,
            auth.user_id,
            ActivityAction::Update,
            task_id,
            "task",
            details,
        ),
        None => Ok(()),
    }
}

/// The lane move of [`check`].
pub(crate) fn lane_move(work_class: WorkClass) -> Option<TriageMove> {
    (work_class == WorkClass::Planned).then_some(TriageMove::Plan)
}

fn log(
    conn: &mut PgConnection,
    actor_id: Uuid,
    action: ActivityAction,
    entity_id: Uuid,
    entity_type: &str,
    details: String,
) -> Result<(), ApiError> {
    diesel::insert_into(kairos_db::schema::activity_log::table)
        .values(NewActivityLogEntry {
            actor_id,
            action,
            entity_id: Some(entity_id),
            entity_type: Some(entity_type.to_string()),
            details,
        })
        .execute(conn)
        .map_err(ApiError::internal)?;
    Ok(())
}
