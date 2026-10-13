//! Serving the uploaded site of a repository (KAIROS-T-0407, KAIROS-D-0012
//! "Serving" and "When the base path changes").
//!
//! `GET`/`HEAD /pages/<tenant>/<team>/repos/<repo>/<path>`, with no login:
//! no credential is read and no cookie is set. Each response has the
//! sandbox CSP, so a page has an opaque origin: its scripts cannot read the
//! cookies of Kairos or call the API as the reader.
//!
//! The base path of a site changes when the repository moves to another
//! team or a slug changes. Until CI publishes again, the live publish stays
//! at the base path it was built for, and the new path redirects (302) to
//! it. After a publish under the new path, the old one redirects (301) to
//! the new root.

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use kairos_db::sites::{self, SiteFile};
use uuid::Uuid;

use super::error_page;
use crate::app::AppState;
use crate::error::ApiError;
use crate::site_store::PublishKey;

/// The CSP of each response of an uploaded site. No `allow-same-origin`,
/// ever: the page gets an opaque origin.
pub(super) const SANDBOX_CSP: &str = "sandbox allow-scripts allow-forms allow-popups \
     allow-downloads; frame-ancestors 'none'";

/// A path under `/pages/<tenant>/`, split for a site.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SitePath {
    pub team: String,
    pub repository: String,
    /// The path in the site, decoded, `/`-separated, with no leading `/`.
    /// Empty for the root.
    pub file: String,
    /// The request path ends with `/`.
    pub dir: bool,
}

impl SitePath {
    pub fn base(&self, tenant: &str) -> String {
        sites::base_path(tenant, &self.team, &self.repository)
    }
}

/// Decode one segment. `None`: not UTF-8, an encoded `/` or `\`, or `..`.
fn decode_segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(out).ok()?;
    if text.contains('/')
        || text.contains('\\')
        || text.contains('\0')
        || text == ".."
        || text == "."
    {
        return None;
    }
    Some(text)
}

/// Split `/pages/<tenant>/<team>/repos/<repo>/<path>`. `None`: not a site
/// path, or a bad segment (404).
pub(super) fn parse(path: &str, tenant: &str) -> Option<SitePath> {
    let rest = path.strip_prefix(&format!("/pages/{tenant}/"))?;
    let mut parts = rest.splitn(4, '/');
    let team = parts.next()?.to_string();
    if parts.next()? != "repos" {
        return None;
    }
    let repository = parts.next()?.to_string();
    if team.is_empty() || repository.is_empty() {
        return None;
    }
    let tail = parts.next();
    let dir = tail.is_none_or(|t| t.is_empty() || t.ends_with('/'));
    let mut segments = Vec::new();
    for raw in tail.unwrap_or("").split('/').filter(|s| !s.is_empty()) {
        segments.push(decode_segment(raw)?);
    }
    Some(SitePath {
        team,
        repository,
        file: segments.join("/"),
        dir: dir && tail.is_some(),
    })
}

/// What a site path asks for, from the database.
enum Lookup {
    /// A file of a live publish (or the site's 404 page, with 404).
    File {
        key: PublishKey,
        file: SiteFile,
        status: StatusCode,
    },
    Redirect(StatusCode, String),
    NotFound,
}

/// Each GET of a site path.
pub(super) async fn serve(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    match find(&state, uri.path()).await {
        Ok(Lookup::File { key, file, status }) => {
            send_file(&state, &headers, key, file, status).await
        }
        Ok(Lookup::Redirect(status, to)) => (status, [(header::LOCATION, to)]).into_response(),
        Ok(Lookup::NotFound) => sandboxed(error_page(StatusCode::NOT_FOUND)),
        Err(e) => {
            tracing::warn!(error = %e.message, "cannot serve a repository site");
            sandboxed(error_page(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

async fn find(state: &AppState, path: &str) -> Result<Lookup, ApiError> {
    let tenant = path
        .strip_prefix("/pages/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
        .to_string();
    let Some(site_path) = parse(path, &tenant) else {
        return Ok(Lookup::NotFound);
    };
    let Some(org_id) = org_id(state, &tenant).await? else {
        return Ok(Lookup::NotFound);
    };
    let path = path.to_string();
    let pinned = tenant.clone();
    state
        .blocking
        .run(&pinned, move |conn| {
            let base = site_path.base(&tenant);
            let Some(publish) = sites::live_at(conn, &base).map_err(ApiError::internal)? else {
                // Built for an old base path: the new path goes to the live
                // publish where it is, until CI publishes again.
                if let Some(live) = sites::live_of(conn, &site_path.team, &site_path.repository)
                    .map_err(ApiError::internal)?
                {
                    let rest = path.strip_prefix(&base).unwrap_or("");
                    return Ok(Lookup::Redirect(
                        StatusCode::FOUND,
                        format!("{}{rest}", live.base_path),
                    ));
                }
                // An old base path, after a publish under the new one.
                if let Some(new_base) = sites::moved_to(conn, &base).map_err(ApiError::internal)? {
                    return Ok(Lookup::Redirect(StatusCode::MOVED_PERMANENTLY, new_base));
                }
                return Ok(Lookup::NotFound);
            };
            let key = PublishKey {
                org_id,
                repository_id: publish.repository_id,
                publish_id: publish.id,
            };
            let wanted = if site_path.file.is_empty() || site_path.dir {
                if site_path.file.is_empty() {
                    "index.html".to_string()
                } else {
                    format!("{}/index.html", site_path.file)
                }
            } else {
                site_path.file.clone()
            };
            if let Some(file) =
                sites::file(conn, publish.id, &wanted).map_err(ApiError::internal)?
            {
                return Ok(Lookup::File {
                    key,
                    file,
                    status: StatusCode::OK,
                });
            }
            // A folder asked without its slash.
            if !site_path.dir
                && !site_path.file.is_empty()
                && sites::file(conn, publish.id, &format!("{}/index.html", site_path.file))
                    .map_err(ApiError::internal)?
                    .is_some()
            {
                return Ok(Lookup::Redirect(
                    StatusCode::MOVED_PERMANENTLY,
                    format!("{path}/"),
                ));
            }
            match sites::file(conn, publish.id, "404.html").map_err(ApiError::internal)? {
                Some(file) => Ok(Lookup::File {
                    key,
                    file,
                    status: StatusCode::NOT_FOUND,
                }),
                None => Ok(Lookup::NotFound),
            }
        })
        .await
}

/// The id of the organization with the slug `tenant`, if there is one.
async fn org_id(state: &AppState, tenant: &str) -> Result<Option<Uuid>, ApiError> {
    use diesel::prelude::*;
    use kairos_db::schema::organizations;
    let tenant = tenant.to_string();
    state
        .blocking
        .run_public(move |conn| {
            organizations::table
                .filter(organizations::slug.eq(&tenant))
                .select(organizations::id)
                .first::<Uuid>(conn)
                .optional()
                .map_err(ApiError::internal)
        })
        .await
}

async fn send_file(
    state: &AppState,
    headers: &HeaderMap,
    key: PublishKey,
    file: SiteFile,
    status: StatusCode,
) -> Response {
    let etag = format!("\"{}\"", file.sha256);
    let fresh = status == StatusCode::OK
        && headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let Some(store) = state.site_store.as_ref() else {
            return missing_files();
        };
        match store.get(&key, &file.path).await {
            Ok(Some(bytes)) => (status, bytes).into_response(),
            Ok(None) => {
                tracing::warn!(publish_id = %key.publish_id, path = %file.path, "a file of a site is in the manifest but not in the store");
                return missing_files();
            }
            Err(e) => {
                tracing::warn!(publish_id = %key.publish_id, error = %e, "cannot read a file of a site");
                return missing_files();
            }
        }
    };
    let h = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&file.content_type) {
        h.insert(header::CONTENT_TYPE, value);
    }
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    sandboxed(response)
}

/// 503: the manifest names files that the store does not have.
fn missing_files() -> Response {
    let mut response = error_page(StatusCode::SERVICE_UNAVAILABLE);
    *response.body_mut() = axum::body::Body::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>Site \
         unavailable</title></head><body><h1>Site unavailable</h1><p>The files of this site \
         are missing. Publish it again.</p></body></html>\n",
    );
    sandboxed(response)
}

/// The headers of each response of a site.
fn sandboxed(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SANDBOX_CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.remove(header::SET_COOKIE);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(team: &str, repository: &str, file: &str, dir: bool) -> Option<SitePath> {
        Some(SitePath {
            team: team.into(),
            repository: repository.into(),
            file: file.into(),
            dir,
        })
    }

    #[test]
    fn site_paths_split() {
        assert_eq!(
            parse("/pages/acme/web/repos/app/", "acme"),
            site("web", "app", "", true)
        );
        assert_eq!(
            parse("/pages/acme/web/repos/app", "acme"),
            site("web", "app", "", false)
        );
        assert_eq!(
            parse("/pages/acme/web/repos/app/guide/install.html", "acme"),
            site("web", "app", "guide/install.html", false)
        );
        assert_eq!(
            parse("/pages/acme/web/repos/app/guide/", "acme"),
            site("web", "app", "guide", true)
        );
        assert_eq!(
            parse("/pages/acme/web/repos/app/a%20b.html", "acme"),
            site("web", "app", "a b.html", false)
        );
    }

    #[test]
    fn bad_segments_are_refused() {
        for path in [
            "/pages/acme/web/repos/app/../secret",
            "/pages/acme/web/repos/app/a%2Fb.html",
            "/pages/acme/web/repos/app/a%2fb.html",
            "/pages/acme/web/repos/app/%2e%2e/x",
            "/pages/acme/web/repos/app/a%5Cb",
            "/pages/acme/web/repos/app/a%zz",
            "/pages/acme/web/repos/app/a%ff",
            "/pages/acme/web/docs/app/x",
        ] {
            assert_eq!(parse(path, "acme"), None, "{path}");
        }
    }
}
