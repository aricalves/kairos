//! `GET /api/{entity_type}/{short_code}/history` (KAIROS-S-0005, model
//! per KAIROS-A-0004): the append-only content version history.
//!
//! Open tenant-wide (A-0006 reads). Without `?version=`, the paginated
//! version list (newest first — `version`, `edited_by`, `edited_at`);
//! with `?version=N`, that snapshot's full title + content. Version 1 is
//! the create-time baseline (T-0012), so every item has history from
//! birth. A version can be a diff row (KAIROS-T-0651): the snapshot has
//! the full text all the same.

use axum::extract::{Extension, Path, State};
use axum::routing::get;
use axum::{Json, Router};
use kairos_client::types as dto_base;
use kairos_client::types_meta as dto;
use kairos_db::models::graph::ItemHistory;
use serde_json::Value;

use super::resolve_family_item;
use crate::api::Liveness;
use crate::api::clamp_pagination;
use crate::api::convert::IntoDto;
use crate::app::AppState;
use crate::error::ApiError;
use crate::input::ApiQuery;
use crate::middleware::tenant::TenantContext;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/{entity_type}/{short_code}/history", get(get_history))
}

/// Content version history: the paginated version list, or one full
/// snapshot with `?version=N`.
#[utoipa::path(
    get,
    path = "/api/{entity_type}/{short_code}/history",
    tag = "history",
    params(
        ("entity_type" = String, Path, description = "Plural family name (strategies|initiatives|tasks|documents|adrs)"),
        ("short_code" = String, Path, description = "Item short code"),
        dto::HistoryQuery,
    ),
    responses(
        (status = 200, description = "Version list envelope (or a HistorySnapshot with ?version=N)", body = dto_base::ListEnvelope<dto::HistoryVersion>),
        (status = 404, description = "Unknown family, short code, or version", body = dto_base::ErrorEnvelope),
    ),
)]
pub(crate) async fn get_history(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantContext>,
    Path((family, short_code)): Path<(String, String)>,
    ApiQuery(query): ApiQuery<dto::HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let (limit, offset) = clamp_pagination(&dto_base::Pagination {
        limit: query.limit,
        offset: query.offset,
    });
    let response = state
        .blocking
        .run(&tenant.slug, move |conn| {
            let (item_id, _) =
                resolve_family_item(conn, &family, &short_code, Liveness::IncludeArchived)?;

            if let Some(version) = query.version {
                // KAIROS-T-0651: a diff row gets its text from its base row.
                let snapshot: ItemHistory = kairos_db::history::version_row(conn, item_id, version)
                    .map_err(ApiError::internal)?
                    .ok_or_else(|| {
                        ApiError::not_found(format!(
                            "{short_code} has no version {version} in its history."
                        ))
                    })?;
                let snapshot: dto::HistorySnapshot = snapshot.into_dto();
                return serde_json::to_value(snapshot).map_err(ApiError::internal);
            }

            // KAIROS-T-0629: the list reads no title and no content.
            let (total, rows) = kairos_db::items::history_versions(conn, item_id, limit, offset)
                .map_err(ApiError::internal)?;
            let envelope = dto_base::ListEnvelope::<dto::HistoryVersion> {
                items: rows.into_iter().map(IntoDto::into_dto).collect(),
                total,
                limit,
                offset,
            };
            serde_json::to_value(envelope).map_err(ApiError::internal)
        })
        .await?;
    Ok(Json(response))
}
