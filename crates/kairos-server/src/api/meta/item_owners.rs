//! `/api/{entity_type}/{short_code}/owner` (KAIROS-T-0411, KAIROS-A-0025):
//! the owner of a strategy or an initiative.
//!
//! The rules are in [`crate::owners`], the ONE implementation for REST and
//! for the MCP tools `claim_item`, `hand_off_item` and `release_item`.
//! Reads are open tenant-wide. The item DTOs (`GET /api/strategies/...`,
//! `GET /api/initiatives/...`, the board items) carry the owner too.

use axum::extract::{Extension, Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use kairos_client::types as dto;

use super::resolve_family_item;
use crate::api::Liveness;
use crate::app::AppState;
use crate::body::ApiJson;
use crate::error::ApiError;
use crate::middleware::auth::AuthContext;
use crate::middleware::tenant::TenantContext;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/{entity_type}/{short_code}/owner",
            get(get_owner).delete(clear_owner),
        )
        .route(
            "/api/{entity_type}/{short_code}/owner/claim",
            post(claim_owner),
        )
        .route(
            "/api/{entity_type}/{short_code}/owner/hand-off",
            post(hand_off_owner),
        )
}

/// The owner of a strategy or an initiative. Each person in the tenant can
/// read it. An item with no owner has no `owner`.
#[utoipa::path(
    get,
    path = "/api/{entity_type}/{short_code}/owner",
    tag = "relationships",
    params(
        ("entity_type" = String, Path, description = "Plural family name (strategies|initiatives)"),
        ("short_code" = String, Path, description = "Item short code"),
    ),
    responses(
        (status = 200, description = "The owner of the item", body = dto::ItemOwnerResponse),
        (status = 404, description = "Unknown family or short code", body = dto::ErrorEnvelope),
        (status = 422, description = "RELATIONSHIP_RULE: the item is not a strategy and not an initiative", body = dto::ErrorEnvelope),
    ),
)]
pub(crate) async fn get_owner(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantContext>,
    Path((family, short_code)): Path<(String, String)>,
) -> Result<Json<dto::ItemOwnerResponse>, ApiError> {
    let response = state
        .blocking
        .run(&tenant.slug, move |conn| {
            let (item_id, item_type) =
                resolve_family_item(conn, &family, &short_code, Liveness::IncludeArchived)?;
            crate::owners::require_subject(item_type, &short_code)?;
            Ok(dto::ItemOwnerResponse {
                owner: crate::owners::owner_of(conn, item_id)?,
                short_code,
            })
        })
        .await?;
    Ok(Json(response))
}

/// Claim a strategy or an initiative that has no owner. The caller
/// becomes the owner. The caller must be a person. The caller must be a
/// member of the board (a grant on it) or an organization admin. The
/// owner gets no right from the claim.
#[utoipa::path(
    post,
    path = "/api/{entity_type}/{short_code}/owner/claim",
    tag = "relationships",
    params(
        ("entity_type" = String, Path, description = "Plural family name (strategies|initiatives)"),
        ("short_code" = String, Path, description = "Item short code"),
    ),
    responses(
        (status = 200, description = "The caller is the owner (claim activity row written)", body = dto::ItemOwnerResponse),
        (status = 403, description = "The caller is not a member of the board and not an organization admin, or is a service account", body = dto::ErrorEnvelope),
        (status = 404, description = "Unknown family or short code", body = dto::ErrorEnvelope),
        (status = 409, description = "The item has an owner", body = dto::ErrorEnvelope),
        (status = 422, description = "RELATIONSHIP_RULE: the item is not a strategy and not an initiative", body = dto::ErrorEnvelope),
    ),
)]
pub(crate) async fn claim_owner(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
    Path((family, short_code)): Path<(String, String)>,
) -> Result<Json<dto::ItemOwnerResponse>, ApiError> {
    let user = auth.user_id;
    let slug = tenant.slug.clone();
    let response = state
        .blocking
        .run(&tenant.slug, move |conn| {
            let item = resolve_family_item(conn, &family, &short_code, Liveness::LiveOnly)?;
            let owner = crate::owners::claim(conn, &slug, user, item, &short_code)?;
            Ok(dto::ItemOwnerResponse {
                short_code,
                owner: Some(owner),
            })
        })
        .await?;
    Ok(Json(response))
}

/// Give a strategy or an initiative to a different person. `to` is a user
/// id, an email or a user name of a person of the organization. The owner
/// can hand it off. An organization admin can set the owner of each item,
/// also of an item with no owner. The new owner gets a notification.
#[utoipa::path(
    post,
    path = "/api/{entity_type}/{short_code}/owner/hand-off",
    tag = "relationships",
    params(
        ("entity_type" = String, Path, description = "Plural family name (strategies|initiatives)"),
        ("short_code" = String, Path, description = "Item short code"),
    ),
    request_body = dto::HandOffRequest,
    responses(
        (status = 200, description = "The person is the owner (hand_off activity row written)", body = dto::ItemOwnerResponse),
        (status = 403, description = "The caller is not the owner and not an organization admin", body = dto::ErrorEnvelope),
        (status = 404, description = "Unknown family or short code", body = dto::ErrorEnvelope),
        (status = 409, description = "The person is the owner already", body = dto::ErrorEnvelope),
        (status = 422, description = "RELATIONSHIP_RULE: the item is not a strategy and not an initiative. VALIDATION: `to` is not a person of the organization", body = dto::ErrorEnvelope),
    ),
)]
pub(crate) async fn hand_off_owner(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
    Path((family, short_code)): Path<(String, String)>,
    ApiJson(body): ApiJson<dto::HandOffRequest>,
) -> Result<Json<dto::ItemOwnerResponse>, ApiError> {
    let user = auth.user_id;
    let slug = tenant.slug.clone();
    let response = state
        .blocking
        .run(&tenant.slug, move |conn| {
            let item = resolve_family_item(conn, &family, &short_code, Liveness::LiveOnly)?;
            let owner = crate::owners::hand_off(conn, &slug, user, item, &short_code, &body.to)?;
            Ok(dto::ItemOwnerResponse {
                short_code,
                owner: Some(owner),
            })
        })
        .await?;
    Ok(Json(response))
}

/// Remove the owner of a strategy or an initiative. Only an organization
/// admin can do it. The item then has no owner, and a member of its board
/// can claim it.
#[utoipa::path(
    delete,
    path = "/api/{entity_type}/{short_code}/owner",
    tag = "relationships",
    params(
        ("entity_type" = String, Path, description = "Plural family name (strategies|initiatives)"),
        ("short_code" = String, Path, description = "Item short code"),
    ),
    responses(
        (status = 200, description = "The item has no owner (release activity row written)", body = dto::ItemOwnerResponse),
        (status = 403, description = "The caller is not an organization admin", body = dto::ErrorEnvelope),
        (status = 404, description = "Unknown family or short code", body = dto::ErrorEnvelope),
        (status = 422, description = "RELATIONSHIP_RULE: the item is not a strategy and not an initiative. NO_OWNER: the item has no owner", body = dto::ErrorEnvelope),
    ),
)]
pub(crate) async fn clear_owner(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Extension(tenant): Extension<TenantContext>,
    Path((family, short_code)): Path<(String, String)>,
) -> Result<Json<dto::ItemOwnerResponse>, ApiError> {
    let user = auth.user_id;
    let slug = tenant.slug.clone();
    let response = state
        .blocking
        .run(&tenant.slug, move |conn| {
            let item = resolve_family_item(conn, &family, &short_code, Liveness::LiveOnly)?;
            crate::owners::clear(conn, &slug, user, item, &short_code)?;
            Ok(dto::ItemOwnerResponse {
                short_code,
                owner: None,
            })
        })
        .await?;
    Ok(Json(response))
}
