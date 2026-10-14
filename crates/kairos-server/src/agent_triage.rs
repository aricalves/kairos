//! Who decides the triage of intake (KAIROS-T-0428): a person, or also an
//! agent when the board allows it.
//!
//! Intake is a task in the support lane: a request of another team, or
//! support work. Triage decides it: into the planned lane (planned work), or
//! claimed now in the support lane (into a column with claims), or put back
//! in the entry column. A person reviews that decision. An agent proposes it
//! with a note (`append_note`) and leaves the move to a person, unless the
//! board setting `agents_decide` is on.
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
}

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
/// planned lane, or a move that is not triage passes. An agent's triage
/// move passes only when the board allows it; then the activity log gets
/// a line that names the agent.
pub(crate) fn check(
    conn: &mut PgConnection,
    auth: &AuthContext,
    task_id: Uuid,
    triage: TriageMove,
) -> Result<(), ApiError> {
    let facts: TaskFacts = sql_query(
        "SELECT work_class = 'support' AS support, board_id, column_id FROM tasks WHERE id = $1",
    )
    .bind::<SqlUuid, _>(task_id)
    .get_result(conn)
    .map_err(ApiError::internal)?;
    if !facts.support {
        return Ok(());
    }
    if let TriageMove::Column(to) = triage
        && to == facts.column_id
    {
        return Ok(());
    }
    if !is_agent(conn, auth)? {
        return Ok(());
    }
    if !agents_decide(conn, facts.board_id)? {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "AGENT_TRIAGE_OFF",
            "This task is intake (support lane). On this board an agent proposes the triage \
             and a person decides it: write the proposal (planned backlog or work now, and \
             why) as a note with append_note, and leave the task. A person with \
             configure_boards can let agents decide (agents_decide_triage).",
        )
        .with_details(json!({ "board_id": facts.board_id, "setting": "agents_decide_triage" })));
    }
    let what = match triage {
        TriageMove::Plan => "planned".to_string(),
        TriageMove::Column(to) => format!("column:{to}"),
    };
    log(
        conn,
        auth.user_id,
        ActivityAction::Update,
        task_id,
        "task",
        format!("agent_triage:{what}"),
    )
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
