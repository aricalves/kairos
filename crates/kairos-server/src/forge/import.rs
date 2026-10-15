//! The import of the open issues of a repository (KAIROS-T-0483,
//! COLLIERY-I-0623): the issues that were open before the webhook got the
//! Issues event. Each one goes through the same path as an opened-issue
//! event ([`crate::forge::intake::take_issue`]): the filter, then one task
//! per issue id. An import that runs twice makes nothing the second time.
//!
//! The token is the read token of the repository, the one that the code
//! index builder uses ([`crate::credentials::fetch_token`]). It needs Issues:
//! read on GitHub, and `read_api` on GitLab.
//!
//! - GitHub: `GET {KAIROS_GITHUB_API_URL}/repos/{owner}/{repo}/issues?state=open`.
//!   The list has the pull requests too; they are skipped.
//! - GitLab: `GET {scheme://host}/api/v4/projects/{path}/issues?state=opened`,
//!   the host of the URL of the repository. Confidential issues are skipped.
//!
//! Pages of 100, at most [`MAX_PAGES`]. When the forge answers with its
//! rate limit, the import stops and says so: what it took stays, and the
//! next run goes on (it skips what it took).

use std::time::Duration;

use kairos_core::forge::ForgeIssue;
use kairos_db::models::enums::Forge;
use reqwest::StatusCode;
use serde_json::Value;

/// Issues per page.
pub const PER_PAGE: usize = 100;

/// The most pages of one run.
pub const MAX_PAGES: usize = 50;

/// The time of one request.
const TIMEOUT: Duration = Duration::from_secs(20);

/// What the forge gave.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub issues: Vec<ForgeIssue>,
    /// False: the run stopped before the last page; `stopped` says why.
    pub complete: bool,
    pub stopped: Option<String>,
}

/// Why the list could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ListError {
    /// The forge refused the token (401, 403 that is not the rate limit,
    /// 404).
    #[error("{0}")]
    Refused(String),
    /// The request failed, or the forge answered with an error.
    #[error("{0}")]
    Failed(String),
}

/// The API base of GitLab for a repository URL: `scheme://host[:port]/api/v4`.
/// Pure.
pub fn gitlab_api_base(repo_url: &str) -> Option<String> {
    let url = url::Url::parse(repo_url).ok()?;
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}/api/v4", url.scheme()))
}

/// The URL of one page. Pure.
pub fn page_url(forge: Forge, api_base: &str, repo_full_name: &str, page: usize) -> String {
    let base = api_base.trim_end_matches('/');
    match forge {
        Forge::Gitlab => format!(
            "{base}/projects/{}/issues?state=opened&per_page={PER_PAGE}&page={page}",
            url::form_urlencoded::byte_serialize(repo_full_name.as_bytes()).collect::<String>()
        ),
        _ => format!(
            "{base}/repos/{repo_full_name}/issues?state=open&per_page={PER_PAGE}&page={page}"
        ),
    }
}

/// Whether a refusal is the rate limit of the forge: 429, or 403 with no
/// requests left or a `Retry-After`. Pure.
pub fn is_rate_limit(status: StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    status == StatusCode::FORBIDDEN
        && (headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0")
            || headers.contains_key(reqwest::header::RETRY_AFTER))
}

/// The refusal of the token, for a person. Pure.
fn refusal(forge: Forge, status: StatusCode) -> String {
    let (name, permission) = match forge {
        Forge::Gitlab => ("GitLab", "read_api"),
        _ => ("GitHub", "Issues: read"),
    };
    format!(
        "{name} refused the read token of the repository ({status}). The token needs \
         {permission} on the repository."
    )
}

/// List the open issues of a repository, page by page.
pub async fn list_open_issues(
    forge: Forge,
    api_base: &str,
    repo_full_name: &str,
    token: &str,
) -> Result<Listing, ListError> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("Kairos/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ListError::Failed(format!("The HTTP client did not start: {e}.")))?;
    let mut listing = Listing::default();
    for page in 1..=MAX_PAGES {
        let mut request = client.get(page_url(forge, api_base, repo_full_name, page));
        request = match forge {
            Forge::Gitlab => request.header("PRIVATE-TOKEN", token),
            _ => request
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
        };
        let response = request.send().await.map_err(|e| {
            ListError::Failed(format!(
                "The request to the forge failed: {}.",
                e.without_url()
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            if is_rate_limit(status, response.headers()) {
                listing.stopped = Some(format!(
                    "The forge rate limit stopped the import at page {page}. Run the import \
                     again later: it skips the issues that it took."
                ));
                return Ok(listing);
            }
            if matches!(status.as_u16(), 401 | 403 | 404) {
                return Err(ListError::Refused(refusal(forge, status)));
            }
            return Err(ListError::Failed(format!(
                "The forge answered {status} to the list of issues."
            )));
        }
        let rows: Vec<Value> = response.json().await.map_err(|e| {
            ListError::Failed(format!(
                "The list of issues is not JSON: {}.",
                e.without_url()
            ))
        })?;
        let count = rows.len();
        listing
            .issues
            .extend(rows.iter().filter_map(|row| match forge {
                Forge::Gitlab => kairos_core::forge::gitlab_api_issue(row, repo_full_name),
                _ => kairos_core::forge::github_issue(row, repo_full_name),
            }));
        if count < PER_PAGE {
            listing.complete = true;
            return Ok(listing);
        }
    }
    listing.stopped = Some(format!(
        "The import read {MAX_PAGES} pages, the most in one run. Run it again: it skips the \
         issues that it took."
    ));
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_urls() {
        assert_eq!(
            page_url(Forge::Github, "https://api.github.com/", "acme/app", 2),
            "https://api.github.com/repos/acme/app/issues?state=open&per_page=100&page=2"
        );
        assert_eq!(
            page_url(
                Forge::Gitlab,
                "https://gitlab.com/api/v4",
                "acme/sub/app",
                1
            ),
            "https://gitlab.com/api/v4/projects/acme%2Fsub%2Fapp/issues?state=opened&per_page=100&page=1"
        );
    }

    #[test]
    fn the_gitlab_api_is_on_the_host_of_the_repository() {
        assert_eq!(
            gitlab_api_base("https://gitlab.com/acme/app").as_deref(),
            Some("https://gitlab.com/api/v4")
        );
        assert_eq!(
            gitlab_api_base("http://192.0.2.10:8929/acme/app").as_deref(),
            Some("http://192.0.2.10:8929/api/v4")
        );
        assert_eq!(gitlab_api_base("not a url"), None);
    }

    #[test]
    fn the_rate_limit_is_told_from_a_refusal() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert!(is_rate_limit(StatusCode::TOO_MANY_REQUESTS, &headers));
        assert!(!is_rate_limit(StatusCode::FORBIDDEN, &headers));
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert!(is_rate_limit(StatusCode::FORBIDDEN, &headers));
        assert!(!is_rate_limit(StatusCode::UNAUTHORIZED, &headers));
    }

    #[test]
    fn a_refusal_names_the_permission() {
        assert!(refusal(Forge::Github, StatusCode::FORBIDDEN).contains("Issues: read"));
        assert!(refusal(Forge::Gitlab, StatusCode::UNAUTHORIZED).contains("read_api"));
    }
}
