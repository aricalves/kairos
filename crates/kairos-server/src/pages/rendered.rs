//! The rendered org and team sites (KAIROS-D-0013). KAIROS-T-0404 builds
//! the pages; this module has the routing rules that come first: a path
//! without its trailing slash redirects, and the org landing answers.

use axum::extract::Extension;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use super::{error_page, escape};
use crate::middleware::tenant::TenantContext;

/// Each GET of a rendered page.
pub(super) async fn page(Extension(tenant): Extension<TenantContext>, uri: Uri) -> Response {
    let path = uri.path();
    if !path.ends_with('/') {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, format!("{path}/"))],
        )
            .into_response();
    }
    if path == format!("/pages/{}/", tenant.slug) {
        return org_landing(&tenant);
    }
    error_page(StatusCode::NOT_FOUND)
}

fn org_landing(tenant: &TenantContext) -> Response {
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{slug}</title></head><body><h1>{slug}</h1></body></html>\n",
        slug = escape(&tenant.slug),
    );
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "private, no-cache"),
        ],
        body,
    )
        .into_response()
}
