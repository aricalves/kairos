//! The security headers of each response (KAIROS-T-0370).
//!
//! - `X-Content-Type-Options: nosniff` — the browser uses the content type
//!   that the server sends.
//! - `X-Frame-Options: DENY` and the CSP `frame-ancestors 'none'` — no
//!   page of a different site can show Kairos in a frame.
//! - `Referrer-Policy: same-origin` — a link to a different site does not
//!   get the path of the Kairos page.
//! - `Content-Security-Policy` — the page of the GUI gets [`spa_csp`]: its
//!   scripts come from the server, with the hash of each inline script of
//!   `index.html`, and WebAssembly. Each other response gets
//!   `frame-ancestors 'none'` only.
//! - `Strict-Transport-Security` — only when the request came over HTTPS.
//!   The server itself speaks HTTP; a proxy in front of it ends TLS. So the
//!   server sends HSTS when `KAIROS_TRUSTED_PROXY` is on and the proxy says
//!   `X-Forwarded-Proto: https`. A header that a client sends with no proxy
//!   is not trusted.
//!
//! A header that a handler set already stays.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::app::AppState;

/// The HSTS value: one year, with the subdomains.
pub const HSTS: &str = "max-age=31536000; includeSubDomains";

/// The CSP of each response that is not the page of the GUI.
pub const DEFAULT_CSP: &str = "frame-ancestors 'none'";

/// Did the request come over HTTPS through a trusted proxy?
pub fn forwarded_https(headers: &HeaderMap, trusted_proxy: bool) -> bool {
    trusted_proxy
        && headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

/// Put the security headers on `headers`; a header that is there stays.
pub fn apply(headers: &mut HeaderMap, https: bool) {
    let mut set = |name: HeaderName, value: &'static str| {
        headers
            .entry(name)
            .or_insert_with(|| HeaderValue::from_static(value));
    };
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::REFERRER_POLICY, "same-origin");
    set(header::CONTENT_SECURITY_POLICY, DEFAULT_CSP);
    if https {
        set(header::STRICT_TRANSPORT_SECURITY, HSTS);
    }
}

/// The middleware (the outermost layer of [`crate::app::router`]).
pub async fn security_headers(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let https = forwarded_https(request.headers(), state.config.trusted_proxy);
    let mut response = next.run(request).await;
    apply(response.headers_mut(), https);
    response
}

/// The CSP of the page of the GUI (`index.html`): the scripts of the
/// server, the inline scripts of the page by their hash, and WebAssembly
/// (`'wasm-unsafe-eval'` lets the page compile its own module, and no
/// `eval` of text). The styles can be inline: the components set `style`
/// attributes, and the diagrams carry `<style>`. Images can come from an
/// HTTPS site, for the images of a markdown text. The fonts of the Aurora
/// stylesheet come from `fonts.gstatic.com`. Pure, host-tested.
pub fn spa_csp(index_html: &[u8]) -> String {
    let hashes: Vec<String> = inline_scripts(&String::from_utf8_lossy(index_html))
        .into_iter()
        .map(|body| {
            let digest = Sha256::digest(body.as_bytes());
            format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            )
        })
        .collect();
    let mut script_src = String::from("'self' 'wasm-unsafe-eval'");
    for hash in hashes {
        script_src.push(' ');
        script_src.push_str(&hash);
    }
    format!(
        "default-src 'self'; script-src {script_src}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob: https:; font-src 'self' data: https://fonts.gstatic.com; connect-src 'self'; \
         worker-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'self'; \
         frame-ancestors 'none'"
    )
}

/// A strong ETag of a body (KAIROS-T-0606): the first 32 hex digits of its
/// SHA-256, in quotes.
pub fn etag_of(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{}\"", &hex[..32])
}

/// The body of each `<script>` of `html` that has no `src`.
fn inline_scripts(html: &str) -> Vec<&str> {
    let mut bodies = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let tag = &after[..open_end];
        let body_start = open_end + 1;
        let Some(close) = after[body_start..].find("</script") else {
            break;
        };
        let body = &after[body_start..body_start + close];
        if !tag.contains("src=") {
            bodies.push(body);
        }
        rest = &after[body_start + close..];
    }
    bodies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsts_only_over_https_through_a_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(forwarded_https(&headers, true));
        assert!(!forwarded_https(&headers, false), "a client sent it");
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!forwarded_https(&headers, true));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("HTTPS, http"));
        assert!(forwarded_https(&headers, true));
        assert!(!forwarded_https(&HeaderMap::new(), true));

        let mut out = HeaderMap::new();
        apply(&mut out, false);
        assert!(out.get(header::STRICT_TRANSPORT_SECURITY).is_none());
        assert_eq!(out[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(out[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(out[header::REFERRER_POLICY], "same-origin");
        assert_eq!(out[header::CONTENT_SECURITY_POLICY], DEFAULT_CSP);
        let mut out = HeaderMap::new();
        apply(&mut out, true);
        assert_eq!(out[header::STRICT_TRANSPORT_SECURITY], HSTS);
    }

    #[test]
    fn a_header_of_the_handler_stays() {
        let mut out = HeaderMap::new();
        out.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'self'"),
        );
        apply(&mut out, false);
        assert_eq!(out[header::CONTENT_SECURITY_POLICY], "default-src 'self'");
    }

    #[test]
    fn the_csp_of_the_page_has_the_hash_of_each_inline_script() {
        let html = br#"<html><head><script>a()</script>
            <script src="/x.js"></script>
            <script type="module">import init from '/k.js';</script></head></html>"#;
        let csp = spa_csp(html);
        let hash = |body: &str| {
            format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body.as_bytes()))
            )
        };
        assert!(csp.contains(&hash("a()")), "{csp}");
        assert!(csp.contains(&hash("import init from '/k.js';")), "{csp}");
        assert!(
            !csp.contains(&hash("")),
            "a script with src is not hashed: {csp}"
        );
        assert!(csp.contains("'wasm-unsafe-eval'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains("object-src 'none'"), "{csp}");
        assert!(!csp.contains("'unsafe-eval'"), "{csp}");
    }
}
