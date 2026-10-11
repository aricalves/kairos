//! `/api/me/notification-settings` (KAIROS-T-0381, COLLIERY-I-0614): the
//! kinds of notification that a person turned off.
//!
//! Each person reads and changes only their own settings. A service account
//! gets no notifications, so it has no settings (403).

use axum::extract::{Extension, State};
use axum::routing::get;
use axum::{Json, Router};
use kairos_client::types_notifications::{NotificationSettings, UpdateNotificationSettingsRequest};
use kairos_core::notifications::NotificationKind;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::app::AppState;
use crate::body::ApiJson;
use crate::error::ApiError;
use crate::middleware::auth::AuthContext;
use crate::middleware::tenant::TenantContext;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route(
        "/api/me/notification-settings",
        get(get_notification_settings).put(update_notification_settings),
    )
}

/// Refuse a service account: notifications are for people.
pub(crate) fn require_person(
    conn: &mut diesel::pg::PgConnection,
    user: Uuid,
) -> Result<(), ApiError> {
    if kairos_db::task_claims::is_person(conn, user).map_err(ApiError::internal)? {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "A service account gets no notifications. Notifications are for people.",
    ))
}

fn settings_view(settings: &BTreeMap<NotificationKind, bool>) -> NotificationSettings {
    let on = |kind| settings.get(&kind).copied().unwrap_or(true);
    NotificationSettings {
        hand_off: on(NotificationKind::HandOff),
        blocker_cleared: on(NotificationKind::BlockerCleared),
        cancelled: on(NotificationKind::Cancelled),
        mention: on(NotificationKind::Mention),
        p0_bug: on(NotificationKind::P0Bug),
    }
}

/// The caller's notification settings: each kind, and whether it is on.
#[utoipa::path(
    get,
    path = "/api/me/notification-settings",
    tag = "me",
    responses(
        (status = 200, description = "Each kind, and whether it is on", body = NotificationSettings),
        (status = 403, description = "The caller is a service account", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn get_notification_settings(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<NotificationSettings>, ApiError> {
    let user = auth.user_id;
    let settings = state
        .blocking
        .run(&tenant.slug, move |conn| {
            require_person(conn, user)?;
            kairos_db::notifications::settings(conn, user).map_err(ApiError::internal)
        })
        .await?;
    Ok(Json(settings_view(&settings)))
}

/// Turn kinds of notification on or off for the caller. A kind that is not
/// in the body does not change. Kairos refuses a body with no kind.
#[utoipa::path(
    put,
    path = "/api/me/notification-settings",
    tag = "me",
    request_body = UpdateNotificationSettingsRequest,
    responses(
        (status = 200, description = "The settings after the change", body = NotificationSettings),
        (status = 403, description = "The caller is a service account", body = kairos_client::types::ErrorEnvelope),
        (status = 422, description = "The body names no kind", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn update_notification_settings(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
    ApiJson(body): ApiJson<UpdateNotificationSettingsRequest>,
) -> Result<Json<NotificationSettings>, ApiError> {
    let changes: Vec<(NotificationKind, bool)> = [
        (NotificationKind::HandOff, body.hand_off),
        (NotificationKind::BlockerCleared, body.blocker_cleared),
        (NotificationKind::Cancelled, body.cancelled),
        (NotificationKind::Mention, body.mention),
        (NotificationKind::P0Bug, body.p0_bug),
    ]
    .into_iter()
    .filter_map(|(kind, value)| value.map(|v| (kind, v)))
    .collect();
    if changes.is_empty() {
        return Err(ApiError::validation(
            "The body names no kind. Send one or more of: hand_off, blocker_cleared, \
             cancelled, mention, p0_bug, each with true or false.",
        ));
    }
    let user = auth.user_id;
    let settings = state
        .blocking
        .run(&tenant.slug, move |conn| {
            require_person(conn, user)?;
            kairos_db::notifications::set_settings(conn, user, &changes).map_err(ApiError::internal)
        })
        .await?;
    Ok(Json(settings_view(&settings)))
}
