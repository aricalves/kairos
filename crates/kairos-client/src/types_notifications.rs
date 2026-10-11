//! Wire types of the notifications of a person (KAIROS-T-0381,
//! COLLIERY-I-0614): the settings that turn a kind off.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `GET` and `PUT /api/me/notification-settings` response: each kind, and
/// whether it is on. A kind that is off makes no notification for the
/// person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationSettings {
    /// A person handed off the claim of a task to you.
    pub hand_off: bool,
    /// A blocker of a task that you have the claim on is done or archived.
    pub blocker_cleared: bool,
    /// A person cancelled a task that you created.
    pub cancelled: bool,
    /// A person wrote `@your_user_name` in a note or in the content of an
    /// item.
    pub mention: bool,
    /// A bug with the priority P0 (critical) is on the board of your team.
    pub p0_bug: bool,
}

/// `PUT /api/me/notification-settings` body: the kinds to turn on (`true`)
/// or off (`false`). A kind that is not in the body does not change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateNotificationSettingsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hand_off: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker_cleared: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p0_bug: Option<bool>,
}
