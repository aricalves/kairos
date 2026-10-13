//! The sites' own files at `/pages/_/<file>` (KAIROS-D-0013): the
//! stylesheet, the script that draws Mermaid fences, and the vendored
//! Mermaid. All are in the binary, so a deployment without the web bundle
//! serves the sites too.

use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::error_page;

/// The layout of the sites.
const LAYOUT_CSS: &str = include_str!("pages.css");
/// The rules of the app for markdown, Gherkin and Mermaid fences.
const APP_CSS: &str = include_str!("../../../kairos-web/app.css");
/// The script of the sites.
const PAGES_JS: &str = include_str!("pages.js");
/// The vendored Mermaid (kairos-web/vendor/mermaid/README.md has its version).
const MERMAID_JS: &[u8] =
    include_bytes!("../../../kairos-web/vendor/mermaid/mermaid-12.1.0.min.js");

/// Serve one file, or 404.
pub(super) fn serve(method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return error_page(StatusCode::METHOD_NOT_ALLOWED);
    }
    match path.strip_prefix("/pages/_/") {
        Some("pages.css") => respond(
            format!("{}\n{APP_CSS}\n{LAYOUT_CSS}", aurora_dark::TOKENS_CSS).into_bytes(),
            "text/css; charset=utf-8",
        ),
        Some("pages.js") => respond(
            PAGES_JS.as_bytes().to_vec(),
            "text/javascript; charset=utf-8",
        ),
        Some("mermaid.min.js") => respond(MERMAID_JS.to_vec(), "text/javascript; charset=utf-8"),
        _ => error_page(StatusCode::NOT_FOUND),
    }
}

fn respond(body: Vec<u8>, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}
