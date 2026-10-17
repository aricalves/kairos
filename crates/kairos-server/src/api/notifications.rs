//! The notifications of a person (COLLIERY-I-0614):
//!
//! - `/api/me/notification-settings` (KAIROS-T-0381): the kinds of
//!   notification that a person turned off;
//! - `GET /api/me/notifications` and `POST /api/me/notifications/read`
//!   (KAIROS-T-0382): the list, and the mark as read. The MCP tools
//!   `get_events` and `mark_events_read` call the same functions
//!   ([`list_for`], [`mark_for`]).
//!
//! Each person reads and changes only their own notifications and settings.
//! A service account gets no notifications, so it has no settings (403).

use axum::extract::{Extension, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use kairos_client::types_notifications::{
    MarkNotificationsReadRequest, MarkNotificationsReadResponse, Notification, NotificationList,
    NotificationSettings, UpdateNotificationSettingsRequest,
};
use kairos_core::notifications::NotificationKind;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::app::AppState;
use crate::body::ApiJson;
use crate::error::ApiError;
use crate::middleware::auth::AuthContext;
use crate::middleware::tenant::TenantContext;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/me/notification-settings",
            get(get_notification_settings).put(update_notification_settings),
        )
        .route("/api/me/notifications", get(list_notifications))
        .route("/api/me/notifications/read", post(mark_notifications_read))
}

/// The most unread notifications in a list.
pub(crate) const LIST_UNREAD: i64 = 100;
/// The read notifications in a list.
pub(crate) const LIST_READ: i64 = 20;

/// The list of `user`: each unread notification (newest first,
/// [`LIST_UNREAD`] at most), then the [`LIST_READ`] newest read ones, and
/// the count of the unread ones.
pub(crate) fn list_for(
    conn: &mut diesel::pg::PgConnection,
    user: Uuid,
) -> Result<(Vec<kairos_db::notifications::Notification>, i64), ApiError> {
    let items = kairos_db::notifications::list(conn, user, LIST_UNREAD, LIST_READ)
        .map_err(ApiError::internal)?;
    let unread = kairos_db::notifications::unread_count(conn, user).map_err(ApiError::internal)?;
    Ok((items, unread))
}

/// Mark notifications of `user` read: `ids`, or each unread one with
/// `all: true`. Exactly one of the two. An id that is not a UUID, or not a
/// notification of `user`, is refused and named, and nothing changes.
/// Returns the ids that changed, and the count of the unread ones after.
pub(crate) fn mark_for(
    conn: &mut diesel::pg::PgConnection,
    user: Uuid,
    ids: Option<Vec<String>>,
    all: Option<bool>,
) -> Result<(Vec<Uuid>, i64), ApiError> {
    let ids = match (ids, all) {
        (Some(ids), None | Some(false)) if !ids.is_empty() => Some(ids),
        (None, Some(true)) => None,
        _ => {
            return Err(ApiError::validation(
                "Send `ids` (one or more ids of your notifications) or `all: true`, not both.",
            ));
        }
    };
    let changed = match ids {
        Some(ids) => {
            let mut parsed = Vec::with_capacity(ids.len());
            let mut bad = Vec::new();
            for id in &ids {
                match Uuid::parse_str(id.trim()) {
                    Ok(uuid) => parsed.push(uuid),
                    Err(_) => bad.push(id.clone()),
                }
            }
            let unknown = kairos_db::notifications::not_of(conn, user, &parsed)
                .map_err(ApiError::internal)?;
            bad.extend(unknown.iter().map(Uuid::to_string));
            if !bad.is_empty() {
                return Err(ApiError::not_found(format!(
                    "You have no notification with the id {}. Kairos changed nothing. \
                     get_events and GET /api/me/notifications give the ids.",
                    bad.join(", ")
                )));
            }
            kairos_db::notifications::mark_read(conn, user, Some(&parsed))
        }
        None => kairos_db::notifications::mark_read(conn, user, None),
    }
    .map_err(ApiError::internal)?;
    let unread = kairos_db::notifications::unread_count(conn, user).map_err(ApiError::internal)?;
    Ok((changed, unread))
}

fn notification_view(n: kairos_db::notifications::Notification) -> Notification {
    Notification {
        id: n.id.to_string(),
        kind: n.kind,
        short_code: n.short_code,
        entity_type: n.entity_type,
        title: n.title,
        text: n.text,
        actor_id: n.actor_id.map(|id| id.to_string()),
        created_at: n
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        read_at: n
            .read_at
            .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    }
}

/// The caller's notifications: each unread one (newest first, 100 at most),
/// then the 20 newest read ones. A service account has none.
#[utoipa::path(
    get,
    path = "/api/me/notifications",
    tag = "me",
    responses(
        (status = 200, description = "The unread notifications, then the newest read ones", body = NotificationList),
    ),
)]
pub(crate) async fn list_notifications(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<NotificationList>, ApiError> {
    let user = auth.user_id;
    let (items, unread) = state
        .blocking
        .run(&tenant.slug, move |conn| list_for(conn, user))
        .await?;
    Ok(Json(NotificationList {
        unread,
        items: items.into_iter().map(notification_view).collect(),
    }))
}

/// Mark notifications of the caller read: `ids`, or `all: true`. An id that
/// is not a notification of the caller is a 404 that names it, and nothing
/// changes. An id that is read already does not change.
#[utoipa::path(
    post,
    path = "/api/me/notifications/read",
    tag = "me",
    request_body = MarkNotificationsReadRequest,
    responses(
        (status = 200, description = "The ids that changed, and the unread count after", body = MarkNotificationsReadResponse),
        (status = 404, description = "An id is not a notification of the caller", body = kairos_client::types::ErrorEnvelope),
        (status = 422, description = "Neither ids nor all, or both", body = kairos_client::types::ErrorEnvelope),
    ),
)]
pub(crate) async fn mark_notifications_read(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
    ApiJson(body): ApiJson<MarkNotificationsReadRequest>,
) -> Result<Json<MarkNotificationsReadResponse>, ApiError> {
    let user = auth.user_id;
    let (changed, unread) = state
        .blocking
        .run(&tenant.slug, move |conn| {
            mark_for(conn, user, body.ids, body.all)
        })
        .await?;
    Ok(Json(MarkNotificationsReadResponse {
        marked: changed.iter().map(Uuid::to_string).collect(),
        unread,
    }))
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
        blocked_again: on(NotificationKind::BlockedAgain),
        cancelled: on(NotificationKind::Cancelled),
        blocker_cancelled: on(NotificationKind::BlockerCancelled),
        mention: on(NotificationKind::Mention),
        p0_bug: on(NotificationKind::P0Bug),
        delivery_failing: on(NotificationKind::DeliveryFailing),
        agent_key_revoked: on(NotificationKind::AgentKeyRevoked),
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
        (NotificationKind::BlockedAgain, body.blocked_again),
        (NotificationKind::Cancelled, body.cancelled),
        (NotificationKind::BlockerCancelled, body.blocker_cancelled),
        (NotificationKind::Mention, body.mention),
        (NotificationKind::P0Bug, body.p0_bug),
        (NotificationKind::DeliveryFailing, body.delivery_failing),
        (NotificationKind::AgentKeyRevoked, body.agent_key_revoked),
    ]
    .into_iter()
    .filter_map(|(kind, value)| value.map(|v| (kind, v)))
    .collect();
    if changes.is_empty() {
        return Err(ApiError::validation(
            "The body names no kind. Send one or more of: hand_off, blocker_cleared, \
             blocked_again, cancelled, blocker_cancelled, mention, p0_bug, delivery_failing, \
             agent_key_revoked, each with true or false.",
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
