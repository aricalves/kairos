//! The work of a person (KAIROS-T-0489, COLLIERY-I-0624): `GET
//! /api/me/work`. The MCP tool `my_work`, the CLI `kairos me` and the "for
//! me" page read the same answer ([`work_for`]). The rules of the lanes and
//! of the states are in `kairos_db::my_work`.
//!
//! The caller is the person: a request with an agent key gives the work of
//! the person of the key. A service account owns nothing and has no claim,
//! so its work is empty.

use axum::extract::{Extension, State};
use axum::routing::get;
use axum::{Json, Router};
use kairos_client::types_work::{MyWork, WorkBlocker, WorkBoard, WorkItem};
use kairos_db::my_work;
use uuid::Uuid;

use crate::app::AppState;
use crate::error::ApiError;
use crate::middleware::auth::AuthContext;
use crate::middleware::tenant::TenantContext;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/api/me/work", get(get_my_work))
}

/// The work of `user`, as the wire type. Five queries and the names of the
/// people.
pub(crate) fn work_for(
    conn: &mut diesel::pg::PgConnection,
    user: Uuid,
) -> Result<MyWork, ApiError> {
    let work = my_work::my_work(conn, user).map_err(ApiError::internal)?;
    let people: Vec<Uuid> = work
        .initiatives
        .iter()
        .chain(work.tasks.iter())
        .flat_map(|item| {
            item.claim
                .iter()
                .map(|c| c.user_id)
                .chain(item.owner.iter().map(|o| o.user_id))
        })
        .collect();
    let names = kairos_db::task_claims::display_names(conn, &people).map_err(ApiError::internal)?;
    let view = |item: my_work::WorkItem| WorkItem {
        id: item.id.to_string(),
        short_code: item.short_code,
        title: item.title,
        entity_type: item.entity_type,
        state: item.state.as_str().to_string(),
        reasons: item
            .reasons
            .iter()
            .map(|r| r.as_str().to_string())
            .collect(),
        board: WorkBoard {
            id: item.board_id.to_string(),
            slug: item.board_slug,
            name: item.board_name,
        },
        column_id: item.column_id.to_string(),
        column_name: item.column_name,
        claim: item
            .claim
            .as_ref()
            .map(|c| crate::claims::claim_dto(c, &names)),
        owner: item
            .owner
            .as_ref()
            .map(|o| crate::owners::owner_dto(o, &names)),
        blockers: item
            .blockers
            .into_iter()
            .map(|b| WorkBlocker {
                short_code: b.short_code,
                title: b.title,
            })
            .collect(),
    };
    Ok(MyWork {
        initiatives: work.initiatives.into_iter().map(view).collect(),
        tasks: work.tasks.into_iter().map(view).collect(),
    })
}

/// The work of the caller (COLLIERY-I-0624). Initiatives: the strategies
/// and the initiatives that the caller owns. Tasks: the tasks that the
/// caller or an agent key of the caller has the claim on, the tasks in
/// Todo, Active or Blocked under an initiative that the caller owns, and
/// the tasks that one of these blocks. Each item has a state (`todo`,
/// `active`, `blocked`) and its open blockers. No item is done.
#[utoipa::path(
    get,
    path = "/api/me/work",
    tag = "me",
    responses(
        (status = 200, description = "The two lanes of the work of the caller", body = MyWork),
        (status = 401, description = "Missing/invalid token", body = kairos_client::types::ErrorEnvelope),
        (status = 403, description = "Not a member of the organization", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn get_my_work(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<MyWork>, ApiError> {
    let user = auth.user_id;
    let work = state
        .blocking
        .run(&tenant.slug, move |conn| work_for(conn, user))
        .await?;
    Ok(Json(work))
}

/// The states in the order of the page: Todo, Active, Blocked.
const STATES: [(&str, &str); 3] = [
    ("todo", "Todo"),
    ("active", "Active"),
    ("blocked", "Blocked"),
];

/// One item of the work as a line of text (the MCP tool `my_work`).
fn item_line(item: &WorkItem) -> String {
    let mut line = format!(
        "- {} \u{2014} {} ({} / {}",
        item.short_code, item.title, item.board.slug, item.column_name
    );
    if let Some(claim) = &item.claim {
        line.push_str(&format!("; claimed by {}", claim.display_name));
        if claim.agent {
            line.push_str(" (agent)");
        }
    } else if item.entity_type == "task" {
        line.push_str("; no claim");
    }
    if let Some(owner) = &item.owner {
        line.push_str(&format!("; owner: {}", owner.display_name));
    }
    line.push_str(&format!(") [{}]\n", item.reasons.join(", ")));
    for blocker in &item.blockers {
        line.push_str(&format!(
            "  blocked by {} \u{2014} {}\n",
            blocker.short_code, blocker.title
        ));
    }
    line
}

/// The work as text: each lane, then each state, then each item.
pub(crate) fn render_text(work: &MyWork) -> String {
    let mut out = String::from("# Your work\n");
    for (lane, items) in [("Initiatives", &work.initiatives), ("Tasks", &work.tasks)] {
        out.push_str(&format!("\n## {lane} ({})\n", items.len()));
        if items.is_empty() {
            out.push_str("(none)\n");
            continue;
        }
        for (state, heading) in STATES {
            let in_state: Vec<&WorkItem> = items.iter().filter(|i| i.state == state).collect();
            if in_state.is_empty() {
                continue;
            }
            out.push_str(&format!("\n### {heading}\n"));
            for item in in_state {
                out.push_str(&item_line(item));
            }
        }
    }
    out
}
