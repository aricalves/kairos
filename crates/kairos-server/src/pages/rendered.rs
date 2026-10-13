//! The rendered org and team sites (KAIROS-T-0404, KAIROS-D-0013).
//!
//! ```text
//! /pages/<tenant>/                      org landing
//! /pages/<tenant>/docs|adrs/<code>/     an org document or ADR
//! /pages/<tenant>/<team>/               team landing
//! /pages/<tenant>/<team>/docs|adrs/<code>/
//! /pages/<tenant>/<team>/<path>/        a team page, by its folder path
//! ```
//!
//! Each page is built from the current rows on each request, with an ETag of
//! the HTML: no cache to invalidate across servers, and the links of short
//! codes always follow the other items. A code that is not where the path
//! says (a retired code, an item of another site) redirects to its page.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use diesel::pg::PgConnection;
use kairos_db::site_content::{self, Place};
use sha2::{Digest, Sha256};

use super::error_page;
use super::views::{self, Crumb, ItemHead, OrgSite, Paths, TeamSite};
use crate::app::AppState;
use crate::error::ApiError;
use crate::middleware::tenant::TenantContext;

/// The CSP of a rendered page (KAIROS-D-0013 "Headers"). `'unsafe-inline'`
/// in `style-src` is for the SVG that Mermaid writes; no inline script.
pub(super) const RENDERED_CSP: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' https: data:; font-src 'self'; \
     connect-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// What a path asks for.
enum Answer {
    Html(String),
    Redirect(String),
    NotFound,
}

/// Each GET of a rendered page.
pub(super) async fn page(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantContext>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let path = uri.path().to_string();
    if !path.ends_with('/') {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, format!("{path}/"))],
        )
            .into_response();
    }
    let paths = Paths {
        tenant: tenant.slug.clone(),
    };
    let rest = path
        .strip_prefix(&format!("/pages/{}/", tenant.slug))
        .unwrap_or("")
        .to_string();
    let answer = state
        .blocking
        .run(&tenant.slug, move |conn| answer(conn, &paths, &rest, &path))
        .await;
    match answer {
        Ok(Answer::Html(body)) => html_response(&headers, body),
        Ok(Answer::Redirect(to)) => {
            (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)]).into_response()
        }
        Ok(Answer::NotFound) => error_page(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::warn!(error = %e.message, "cannot render a docs site page");
            error_page(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// 200 with the ETag of the HTML, or 304 when the reader has it.
fn html_response(headers: &HeaderMap, body: String) -> Response {
    let digest = Sha256::digest(body.as_bytes());
    let etag = format!(
        "\"{}\"",
        digest[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        body.into_response()
    };
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RENDERED_CSP),
    );
    response
}

fn answer(
    conn: &mut PgConnection,
    paths: &Paths,
    rest: &str,
    path: &str,
) -> Result<Answer, ApiError> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [] => org_landing(conn, paths),
        ["docs" | "adrs", code] => item(conn, paths, None, code, path),
        [team, rest @ ..] => {
            let Some(site_team) =
                site_content::team_by_slug(conn, team).map_err(ApiError::internal)?
            else {
                return Ok(Answer::NotFound);
            };
            if !site_team.site_enabled {
                return Ok(Answer::NotFound);
            }
            match rest {
                [] => team_site(conn, site_team)
                    .map(|site| Answer::Html(views::team_landing(paths, &site).into_string())),
                ["docs" | "adrs", code] => item(conn, paths, Some(team), code, path),
                slugs => team_page(conn, paths, site_team, slugs),
            }
        }
    }
}

fn org_site(conn: &mut PgConnection) -> Result<OrgSite, ApiError> {
    Ok(OrgSite {
        teams: site_content::teams_with_site(conn).map_err(ApiError::internal)?,
        documents: site_content::org_documents(conn).map_err(ApiError::internal)?,
        adrs: site_content::org_adrs(conn).map_err(ApiError::internal)?,
    })
}

fn org_landing(conn: &mut PgConnection, paths: &Paths) -> Result<Answer, ApiError> {
    let site = org_site(conn)?;
    Ok(Answer::Html(views::org_landing(paths, &site).into_string()))
}

fn team_site(conn: &mut PgConnection, team: site_content::SiteTeam) -> Result<TeamSite, ApiError> {
    let documents = site_content::team_documents(conn, team.id).map_err(ApiError::internal)?;
    let adrs = site_content::team_adrs(conn, team.id).map_err(ApiError::internal)?;
    let pages = site_content::team_pages(conn, team.id).map_err(ApiError::internal)?;
    let repositories =
        site_content::team_repositories(conn, team.id).map_err(ApiError::internal)?;
    let ids: Vec<uuid::Uuid> = documents.iter().chain(&adrs).map(|i| i.id).collect();
    let impacts = site_content::impacts(conn, &ids).map_err(ApiError::internal)?;
    Ok(TeamSite {
        team,
        documents,
        adrs,
        pages,
        repositories,
        impacts,
    })
}

/// The page of a place, for a code that has one.
fn href_of(paths: &Paths, kind: &str, place: &Place, code: &str) -> String {
    match place {
        Place::Org => paths.item(None, kind, code),
        Place::Team(team) => paths.item(Some(team), kind, code),
    }
}

/// Markdown to HTML, with each code of a published document or ADR linked
/// to its page.
fn render_markdown(conn: &mut PgConnection, paths: &Paths, source: &str) -> String {
    kairos_markdown::to_html_with_codes(source, |codes| match site_content::places(conn, codes) {
        Ok(places) => places
            .into_iter()
            .map(|(code, (kind, place))| {
                let href = href_of(paths, &kind, &place, &code);
                (code, href)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot resolve the short codes of a page");
            Default::default()
        }
    })
}

fn item(
    conn: &mut PgConnection,
    paths: &Paths,
    team: Option<&str>,
    code: &str,
    path: &str,
) -> Result<Answer, ApiError> {
    let found = site_content::page(conn, code).map_err(ApiError::internal)?;
    let Some((page, place)) = found else {
        // A retired code: to the page of the current code, if it has one.
        let current = kairos_db::retired_codes::current_code(conn, code)
            .map_err(ApiError::internal)?
            .and_then(|retired| retired.current);
        if let Some((current, _)) = current
            && let Some((page, place)) =
                site_content::page(conn, &current).map_err(ApiError::internal)?
        {
            return Ok(Answer::Redirect(href_of(
                paths, &page.kind, &place, &current,
            )));
        }
        return Ok(Answer::NotFound);
    };
    let canonical = href_of(paths, &page.kind, &place, &page.short_code);
    if canonical != path {
        return Ok(Answer::Redirect(canonical));
    }
    let body = render_markdown(conn, paths, &page.content);
    let superseded_by = match &page.superseded_by {
        Some(newer) => {
            let places = site_content::places(conn, std::slice::from_ref(newer))
                .map_err(ApiError::internal)?;
            let href = places
                .get(newer)
                .map(|(kind, place)| href_of(paths, kind, place, newer));
            Some((newer.clone(), href))
        }
        None => None,
    };
    let head = ItemHead {
        code: &page.short_code,
        title: &page.title,
        kind: &page.kind,
        status: page.status.as_deref(),
        board_name: &page.board_name,
        updated_at: &page.updated_at,
        superseded_by,
    };
    let mut crumbs = vec![Crumb {
        text: paths.tenant.clone(),
        href: Some(paths.org()),
    }];
    let side = match team {
        Some(team_slug) => {
            let site_team = site_content::team_by_slug(conn, team_slug)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::internal("the team of a page is gone"))?;
            crumbs.push(Crumb {
                text: site_team.name.clone(),
                href: Some(paths.team(team_slug)),
            });
            let site = team_site(conn, site_team)?;
            views::team_side(paths, &site)
        }
        None => views::org_side(paths, &org_site(conn)?),
    };
    crumbs.push(Crumb {
        text: page.short_code.clone(),
        href: None,
    });
    Ok(Answer::Html(
        views::item_page(&crumbs, side, &head, &body).into_string(),
    ))
}

fn team_page(
    conn: &mut PgConnection,
    paths: &Paths,
    team: site_content::SiteTeam,
    slugs: &[&str],
) -> Result<Answer, ApiError> {
    let site = team_site(conn, team)?;
    // KAIROS-T-0403: a top-level page with a reserved slug has no URL.
    if slugs
        .first()
        .is_some_and(|slug| kairos_core::slug::is_site_reserved(slug))
    {
        return Ok(Answer::NotFound);
    }
    let mut parent = None;
    let mut found = None;
    for slug in slugs {
        let Some(node) = site
            .pages
            .iter()
            .find(|p| p.parent_id == parent && p.slug == *slug)
        else {
            return Ok(Answer::NotFound);
        };
        parent = Some(node.id);
        found = Some(node);
    }
    let Some(node) = found else {
        return Ok(Answer::NotFound);
    };
    let body = if node.kind == kairos_db::models::enums::TeamPageKind::Folder {
        views::page_tree(paths, &site.team.slug, &site.pages, Some(node.id), slugs).into_string()
    } else {
        render_markdown(conn, paths, &node.content)
    };
    let mut crumbs = vec![
        Crumb {
            text: paths.tenant.clone(),
            href: Some(paths.org()),
        },
        Crumb {
            text: site.team.name.clone(),
            href: Some(paths.team(&site.team.slug)),
        },
    ];
    let mut walked: Vec<&str> = Vec::new();
    for (i, slug) in slugs.iter().enumerate() {
        walked.push(slug);
        let title = site
            .pages
            .iter()
            .find(|p| p.slug == *slug)
            .map(|p| p.title.clone())
            .unwrap_or_else(|| slug.to_string());
        crumbs.push(Crumb {
            text: title,
            href: (i + 1 < slugs.len()).then(|| paths.team_page(&site.team.slug, &walked)),
        });
    }
    let head = ItemHead {
        code: "",
        title: &node.title,
        kind: "page",
        status: None,
        board_name: "",
        updated_at: &node.updated_at,
        superseded_by: None,
    };
    let side = views::team_side(paths, &site);
    Ok(Answer::Html(
        views::item_page(&crumbs, side, &head, &body).into_string(),
    ))
}
