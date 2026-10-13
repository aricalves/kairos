//! The docs sites under `/pages` (COLLIERY-I-0619; KAIROS-D-0012,
//! KAIROS-D-0013).
//!
//! Two kinds of site share the prefix:
//!
//! - the **rendered** sites of the org and of each team, which Kairos builds
//!   from its documents, ADRs and team pages. A reader must be signed in;
//! - the **uploaded** site of a repository, at
//!   `/pages/<tenant>/<team>/repos/<repo>/`, which anyone can read.
//!
//! The router cannot tell them apart by its route table alone (a catch-all
//! and the `repos` route at the same depth conflict), so one entry point
//! ([`dispatch`]) looks at the path and sends the request to the router of
//! its kind. Only the rendered router has the auth layers.
//!
//! The tenant comes from the path, not from the `X-Tenant` header. When
//! `KAIROS_SINGLE_TENANT` or the host fixes a tenant, the path must name the
//! same one, otherwise 404 ([`crate::middleware::tenant::fixed_slug`]).
//!
//! A reader with no session (or an expired one) gets 302 to
//! `/login?next=<path>`. The session cookie is `SameSite=Strict`, so a link
//! from another site arrives without it; the login page sends a reader who is
//! signed in straight back to `next`.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self as axum_middleware, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use kairos_db::tenant::is_valid_tenant_slug;
use tower::ServiceExt as _;

use crate::app::AppState;
use crate::middleware::{auth, tenant};

mod assets;
mod rendered;
mod views;

/// The names that cannot be the slug of a team, or of a team page at the top
/// of the tree, because the sites use them as path segments.
pub use kairos_core::slug::SITE_RESERVED_SLUGS;

/// The `/pages` router (mounted before the SPA fallback, outside the
/// auth and tenant stack of the API).
pub fn router(state: &AppState) -> Router<AppState> {
    let rendered = rendered_router(state);
    let sites = sites_router(state);
    let handler = move |req: Request| {
        let rendered = rendered.clone();
        let sites = sites.clone();
        async move { dispatch(req, rendered, sites).await }
    };
    Router::new()
        .route("/pages", any(handler.clone()))
        .route("/pages/", any(handler.clone()))
        .route("/pages/{*rest}", any(handler))
}

/// The kind of a path under `/pages`.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// `/pages/_/<file>`: the sites' own CSS and scripts.
    Asset,
    /// `/pages/<tenant>/<team>/repos/...`: an uploaded repository site.
    Site,
    /// Everything else: the rendered org and team sites.
    Rendered,
}

fn kind_of(path: &str) -> Kind {
    let rest = path.strip_prefix("/pages/").unwrap_or("");
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["_", ..] => Kind::Asset,
        [_tenant, team, "repos", ..] if !SITE_RESERVED_SLUGS.contains(team) => Kind::Site,
        _ => Kind::Rendered,
    }
}

async fn dispatch(req: Request, rendered: Router, sites: Router) -> Response {
    match kind_of(req.uri().path()) {
        Kind::Asset => assets::serve(req.method(), req.uri().path()),
        Kind::Site => match sites.oneshot(req).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
        Kind::Rendered => match rendered.oneshot(req).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
    }
}

/// The rendered sites, behind login → path tenant → auth → tenant → the
/// check that the tenant of the credential is the tenant of the path.
fn rendered_router(state: &AppState) -> Router {
    Router::new()
        .route("/pages/{tenant}", get(rendered::page))
        .route("/pages/{tenant}/", get(rendered::page))
        .route("/pages/{tenant}/{*rest}", get(rendered::page))
        // route_layer wraps bottom-up: the last one added runs first.
        .route_layer(axum_middleware::from_fn(same_tenant))
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            tenant::require_tenant,
        ))
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            path_tenant,
        ))
        .route_layer(axum_middleware::from_fn(login_redirect))
        .fallback(not_found)
        .with_state(state.clone())
}

/// The uploaded repository sites: no credential is read. KAIROS-T-0407
/// serves them; until then each path is a 404.
fn sites_router(state: &AppState) -> Router {
    Router::new()
        .route("/pages/{tenant}/{*rest}", get(not_found))
        .route_layer(axum_middleware::from_fn_with_state(
            state.clone(),
            path_tenant,
        ))
        .fallback(not_found)
        .with_state(state.clone())
}

async fn not_found() -> Response {
    error_page(StatusCode::NOT_FOUND)
}

/// The tenant of a `/pages/<tenant>/...` path.
fn path_tenant_slug(path: &str) -> Option<&str> {
    path.strip_prefix("/pages/")?
        .split('/')
        .next()
        .filter(|slug| !slug.is_empty())
}

/// Read the tenant from the path. It must be a valid slug, and it must agree
/// with the tenant that the deployment or the host fixes. Then it goes to
/// [`tenant::require_tenant`] as `X-Tenant`, so the API's tenant layer does
/// the rest unchanged.
async fn path_tenant(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(slug) = path_tenant_slug(req.uri().path()).map(str::to_string) else {
        return error_page(StatusCode::NOT_FOUND);
    };
    if !is_valid_tenant_slug(&slug) {
        return error_page(StatusCode::NOT_FOUND);
    }
    if tenant::fixed_slug(&state.config, req.headers()).is_some_and(|fixed| fixed != slug) {
        return error_page(StatusCode::NOT_FOUND);
    }
    let value = HeaderValue::from_str(&slug).expect("a valid slug is a valid header value");
    req.headers_mut().insert("x-tenant", value);
    next.run(req).await
}

/// An API key carries its own tenant, and [`tenant::require_tenant`] takes it
/// over `X-Tenant`. The page is of the tenant in the path, so a key of a
/// different tenant gets 404.
async fn same_tenant(req: Request, next: Next) -> Response {
    let path_slug = path_tenant_slug(req.uri().path()).map(str::to_string);
    let context_slug = req
        .extensions()
        .get::<tenant::TenantContext>()
        .map(|context| context.slug.clone());
    if path_slug.is_none() || path_slug != context_slug {
        return error_page(StatusCode::NOT_FOUND);
    }
    next.run(req).await
}

/// The outermost layer of the rendered sites: a 401 of a GET becomes a
/// redirect to the login page with `next`, and each other error of the API
/// stack (a JSON envelope) becomes a plain HTML page.
async fn login_redirect(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let target = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/pages/".to_string());
    let response = next.run(req).await;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED && (method == Method::GET || method == Method::HEAD) {
        let location = format!("/login?next={}", encode_query_value(&target));
        return (
            StatusCode::FOUND,
            [
                (header::LOCATION, location),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
        )
            .into_response();
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if (status.is_client_error() || status.is_server_error()) && is_json {
        return error_page(status);
    }
    response
}

/// Percent-encode a value for a query string: each byte but the unreserved
/// characters and `/`.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Escape text for HTML content and attributes.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Kairos's plain error page for the sites.
pub(crate) fn error_page(status: StatusCode) -> Response {
    let (title, text) = match status {
        StatusCode::NOT_FOUND => ("Not found", "This page does not exist."),
        StatusCode::FORBIDDEN => (
            "No access",
            "You are not a member of this organization. Ask an organization admin to add you.",
        ),
        StatusCode::METHOD_NOT_ALLOWED => ("Not allowed", "Use GET to read a page."),
        _ => (
            "Error",
            "Kairos cannot show this page now. Try again later.",
        ),
    };
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title></head><body><h1>{title}</h1><p>{text}</p></body></html>\n",
        title = escape(title),
        text = escape(text),
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_go_to_their_kind() {
        assert_eq!(kind_of("/pages/_/pages.css"), Kind::Asset);
        assert_eq!(kind_of("/pages/acme/kairos/repos/kairos/"), Kind::Site);
        assert_eq!(kind_of("/pages/acme/kairos/repos/kairos"), Kind::Site);
        assert_eq!(kind_of("/pages/acme/kairos/repos"), Kind::Site);
        assert_eq!(
            kind_of("/pages/acme/kairos/repos/kairos/a/b.html"),
            Kind::Site
        );
        assert_eq!(kind_of("/pages"), Kind::Rendered);
        assert_eq!(kind_of("/pages/acme"), Kind::Rendered);
        assert_eq!(kind_of("/pages/acme/"), Kind::Rendered);
        assert_eq!(kind_of("/pages/acme/kairos/"), Kind::Rendered);
        assert_eq!(
            kind_of("/pages/acme/kairos/docs/KAIROS-D-0001/"),
            Kind::Rendered
        );
        assert_eq!(kind_of("/pages/acme/docs/repos/"), Kind::Rendered);
        assert_eq!(
            kind_of("/pages/acme/kairos/runbooks/repos/"),
            Kind::Rendered
        );
    }

    #[test]
    fn the_tenant_is_the_first_segment() {
        assert_eq!(path_tenant_slug("/pages/acme/kairos/"), Some("acme"));
        assert_eq!(path_tenant_slug("/pages/acme"), Some("acme"));
        assert_eq!(path_tenant_slug("/pages/"), None);
        assert_eq!(path_tenant_slug("/pages"), None);
    }

    #[test]
    fn next_is_encoded() {
        assert_eq!(
            encode_query_value("/pages/acme/kairos/?a=b&c=d e"),
            "/pages/acme/kairos/%3Fa%3Db%26c%3Dd%20e"
        );
    }
}
