//! `GET /api/org/retention` (KAIROS-T-0631): the retention settings of the
//! deployment ([`kairos_core::retention::RetentionConfig`]) and the record
//! of the last retention pass. Only an organization admin. The Admin
//! overview of the GUI shows it, so an admin sees when `archive` mode has
//! no target and the pass removes nothing.
//!
//! The settings come from the environment of the server
//! ([`crate::app::AppState::retention`]). The response gives the kind of
//! the archive target, never its path.

use axum::extract::{Extension, State};
use axum::routing::get;
use axum::{Json, Router};
use kairos_client::types_org::RetentionStatus;
use kairos_core::retention::ArchiveTarget;

use crate::api::meta::require_org_admin;
use crate::app::AppState;
use crate::error::ApiError;
use crate::middleware::tenant::TenantContext;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/org/retention", get(get_retention))
}

/// The retention settings of the deployment and the last pass.
#[utoipa::path(
    get,
    path = "/api/org/retention",
    tag = "organization",
    responses(
        (status = 200, description = "The retention settings and the last pass", body = RetentionStatus),
        (status = 403, description = "The caller is not an organization admin", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn get_retention(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<RetentionStatus>, ApiError> {
    require_org_admin(&tenant)?;
    let config = std::sync::Arc::clone(&state.retention);
    let key = crate::periodic::key(crate::periodic::RETENTION);
    let last = state
        .blocking
        .run_public(move |conn| kairos_db::jobs::last_done(conn, &key).map_err(ApiError::internal))
        .await?;
    let last_result = last.as_ref().and_then(|job| {
        job.payload_json()
            .get("result")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });
    Ok(Json(RetentionStatus {
        mode: config.mode.as_str().to_string(),
        archive_target: config.archive_target.as_ref().map(|t| {
            match t {
                ArchiveTarget::Filesystem(_) => "filesystem",
                ArchiveTarget::S3(_) => "s3",
            }
            .to_string()
        }),
        status: config.status().as_str().to_string(),
        warning: config.warning(),
        history_hot_days: config.history_hot_days,
        history_keep_latest: config.history_keep_latest,
        activity_retention_days: config.activity_retention_days,
        last_result,
        last_run_at: last.and_then(|job| job.finished_at).map(|t| t.to_rfc3339()),
    }))
}
