//! The sites' own files at `/pages/_/<file>` (KAIROS-D-0013).

use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::error_page;

/// The stylesheet of the rendered sites. KAIROS-T-0404 gives it the look of
/// the app.
const PAGES_CSS: &str = "body { font-family: system-ui, sans-serif; margin: 0 auto; max-width: 60rem; padding: 1rem; }\n";

/// Serve one file, or 404.
pub(super) fn serve(method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return error_page(StatusCode::METHOD_NOT_ALLOWED);
    }
    let (body, content_type) = match path.strip_prefix("/pages/_/") {
        Some("pages.css") => (PAGES_CSS, "text/css; charset=utf-8"),
        _ => return error_page(StatusCode::NOT_FOUND),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}
