//! Client types for the service-account + API-key management API
//! (KAIROS-A-0017 / KAIROS-T-0059/T-0060). Mirrors the server response bodies
//! in `kairos-server::service_accounts::routes`.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// `POST /api/service-accounts` body.
#[derive(Debug, Clone, Serialize)]
pub struct CreateServiceAccountRequest {
    pub name: String,
}

/// A service account (never carries a secret).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceAccount {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// `GET /api/service-accounts` envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceAccountList {
    pub items: Vec<ServiceAccount>,
    pub total: i64,
}

/// `POST /api/service-accounts/{id}/keys` body.
#[derive(Debug, Clone, Serialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    /// Optional RFC 3339 expiry. Omitted: the default life of the
    /// deployment (`KAIROS_API_KEY_TTL_DAYS`, 90 days by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// `true`: the key never expires (KAIROS-T-0370).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_expiry: bool,
}

/// `POST /api/service-accounts/{id}/keys` response — the ONLY place the raw
/// `key` appears (shown once).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyCreated {
    pub id: String,
    pub name: String,
    pub key: String,
    pub prefix: String,
    pub created_at: String,
    pub expires_at: Option<String>,
}

/// One key row (`GET /api/service-accounts/{id}/keys`) — prefix only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    /// The key that a rotation made in place of this key (KAIROS-T-0370).
    #[serde(default)]
    pub replaced_by: Option<String>,
}

/// The response of a rotation (KAIROS-T-0370): the new key with its secret
/// (shown once), and the end of the old secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRotated {
    pub id: String,
    pub name: String,
    pub key: String,
    pub prefix: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub replaced_key_id: String,
    pub replaced_key_expires_at: String,
}

/// `GET /api/service-accounts/{id}/keys` envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyList {
    pub items: Vec<ApiKey>,
    pub total: i64,
}

/// The `DELETE` response for a service account or a key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deleted {
    pub id: String,
    pub deleted: bool,
}

/// `GET /api/org/agent-keys` query (KAIROS-T-0388): the page, and an
/// optional owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct OrgAgentKeysQuery {
    /// Only the keys of this person: a user id (UUID) or an email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Page size (default 50, max 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// Rows to skip (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
}

/// One agent key of a person, as an organization admin reads it
/// (KAIROS-T-0388). Never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgAgentKey {
    /// The key id (UUID). `DELETE /api/org/agent-keys/{key_id}` takes it.
    pub id: String,
    pub name: String,
    /// The start of the key (never the secret).
    pub prefix: String,
    /// The user id (UUID) of the person who owns the key.
    pub owner_id: String,
    /// The email of the owner.
    pub owner_email: String,
    /// The display name of the owner.
    pub owner_name: String,
    pub created_at: String,
    /// `null`: the key never expires.
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    /// `null` while the key is not revoked.
    pub revoked_at: Option<String>,
    /// The key that a rotation made in place of this key (KAIROS-T-0370).
    pub replaced_by: Option<String>,
}
