//! `POST /api/{entity_type}/{short_code}/notes` (KAIROS-T-0367): append a
//! note at the end of an item's content, in one call, with no version.
//!
//! One wildcard route for the five families, as `/restore` and `/history`.
//! The gate is the edit rule (COLLIERY-T-0228), as for each content edit.
//! The write is [`kairos_db::items::append_note`]: it locks the item row,
//! so two notes at the same time both stay.

use axum::extract::{Extension, Path, State};
use axum::routing::post;
use axum::{Json, Router};
use kairos_client::types as dto;

use super::resolve_family_item;
use crate::api::{Liveness, map_note_error, short_code_of};
use crate::app::AppState;
use crate::body::ApiJson;
use crate::error::ApiError;
use crate::middleware::auth::AuthContext;
use crate::middleware::tenant::TenantContext;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/{entity_type}/{short_code}/notes", post(append_note))
}

/// Append a note to an item. The note gets a heading with its id, the date
/// and the name of the caller, then the text. One new version.
#[utoipa::path(
    post,
    path = "/api/{entity_type}/{short_code}/notes",
    tag = "history",
    params(
        ("entity_type" = String, Path, description = "Plural family name (strategies|initiatives|tasks|documents|adrs)"),
        ("short_code" = String, Path, description = "Item short code"),
    ),
    request_body = dto::AppendNoteRequest,
    responses(
        (status = 200, description = "Appended; the id of the note and the new version", body = dto::AppendNoteResponse),
        (status = 403, description = "Refused by the edit rule: the caller did not create the item and lacks the capability", body = dto::ErrorEnvelope),
        (status = 404, description = "Unknown family or short code, or the item is archived", body = dto::ErrorEnvelope),
        (status = 422, description = "The text is empty, or the body has an unknown field", body = dto::ErrorEnvelope),
    ),
)]
pub(crate) async fn append_note(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
    Path((family, short_code)): Path<(String, String)>,
    ApiJson(body): ApiJson<dto::AppendNoteRequest>,
) -> Result<Json<dto::AppendNoteResponse>, ApiError> {
    let user = auth.user_id;
    let slug = tenant.slug.clone();
    let response = state
        .blocking
        .run(&tenant.slug, move |conn| {
            let (item_id, item_type) =
                resolve_family_item(conn, &family, &short_code, Liveness::LiveOnly)?;
            crate::api::require_item_edit(conn, &slug, user, item_id, item_type)?;
            let appended = kairos_db::items::append_note(
                conn,
                item_type,
                item_id,
                &body.text,
                user,
                chrono::Utc::now().date_naive(),
            )
            .map_err(map_note_error)?;
            Ok(dto::AppendNoteResponse {
                short_code: short_code_of(conn, item_id)?,
                version: appended.version,
                note_id: appended.note_id,
            })
        })
        .await?;
    Ok(Json(response))
}
