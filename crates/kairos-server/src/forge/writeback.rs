//! The GitHub write-back (KAIROS-T-0386, COLLIERY-I-0614): the runner of
//! the jobs of the kind `github_writeback` ([`kairos_db::writeback`]).
//!
//! One job writes one pull request:
//!
//! 1. It takes the lease of the pull request ([`kairos_db::writeback::claim_pr`]),
//!    so 2 servers do not write one pull request at a time. A job that does
//!    not get the lease goes back to the queue with no try counted.
//! 2. For each item that links to the pull request, it makes one comment
//!    (`POST /repos/{owner}/{repo}/issues/{number}/comments`) and keeps its
//!    id; a later job edits that comment in place
//!    (`PATCH /repos/{owner}/{repo}/issues/comments/{id}`). A body that did
//!    not change is not sent again. A comment that a person removed on
//!    GitHub is made again.
//! 3. It reads the head commit of the pull request
//!    (`GET /repos/{owner}/{repo}/pulls/{number}`), and sets the status
//!    `kairos` on it (`POST /repos/{owner}/{repo}/statuses/{sha}`):
//!    `failure` when an item is cancelled, `success` when each item is
//!    done, else `pending`. A status that did not change is not sent again.
//!
//! The token is the write token of the repository, sealed with
//! `KAIROS_SECRETS_KEY` ([`crate::secrets::write_token_aad`]). No token: no
//! job is queued, and a job that finds no token writes nothing. A failed
//! write is kept on the token row (with no token in it) and tried again
//! with growing waits; a refusal of the token (401, 403, 404) is not tried
//! again. A write never blocks the change of an item: the change only
//! queues the job.

use std::sync::Arc;
use std::time::Duration;

use kairos_db::writeback::{self as db, Comment, PrItem, PrState};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::blocking::BlockingTenantPool;
use crate::error::ApiError;
use crate::jobs::{JobError, JobFuture, JobHandle, JobHandler, Worker};
use crate::secrets::SecretsKey;

/// The kind of the job.
pub const KIND: &str = db::JOB_KIND;

/// The context of the commit status.
pub const STATUS_CONTEXT: &str = "kairos";

/// The first line of each comment that Kairos writes, with the id of the
/// item.
const MARKER: &str = "<!-- kairos:writeback";

/// The settings of the write-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteBackSettings {
    /// `KAIROS_GITHUB_API_URL` (`https://api.github.com`): the REST API of
    /// GitHub, or of a GitHub Enterprise Server (`https://host/api/v3`).
    pub api_url: String,
    /// The time of one request (15 s).
    pub timeout: Duration,
    /// The wait after the first failed try (30 s). Each wait is 2 times the
    /// wait before it.
    pub retry_base: Duration,
}

impl Default for WriteBackSettings {
    fn default() -> Self {
        WriteBackSettings {
            api_url: "https://api.github.com".to_string(),
            timeout: Duration::from_secs(15),
            retry_base: Duration::from_secs(30),
        }
    }
}

/// Why a write failed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteError {
    reason: String,
    /// False: GitHub refused the token or does not know the pull request; a
    /// try again does not mend it.
    retry: bool,
}

impl WriteError {
    fn of_status(what: &str, status: reqwest::StatusCode) -> Self {
        let code = status.as_u16();
        let retry = !matches!(code, 401 | 403 | 404 | 422);
        let reason = match code {
            401 => format!("GitHub refused the write token ({what}: 401). Set a new token."),
            403 => format!(
                "GitHub refused the write ({what}: 403). The token needs Pull requests: write \
                 and Commit statuses: write on the repository."
            ),
            404 => format!(
                "GitHub does not know the pull request, or the token cannot see the repository \
                 ({what}: 404)."
            ),
            _ => format!("GitHub answered {status} ({what})."),
        };
        WriteError { reason, retry }
    }

    fn of_request(what: &str, error: reqwest::Error) -> Self {
        WriteError {
            reason: format!(
                "The request to GitHub failed ({what}): {}.",
                error.without_url()
            ),
            retry: true,
        }
    }
}

/// The link of an item in the GUI, or `None` with no public URL.
pub(crate) fn item_link(public_url: Option<&str>, code: &str) -> Option<String> {
    public_url.map(|base| format!("{base}/items/{code}"))
}

/// The cell of the column of an item. Pure.
fn column_cell(item: &PrItem) -> String {
    let column = item.column.clone().unwrap_or_else(|| "-".to_string());
    match (&item.cancel_reason, item.done) {
        (Some(reason), _) => format!("{column} (cancelled: {})", one_line(reason)),
        (None, true) => format!("{column} (done)"),
        (None, false) => column,
    }
}

/// A text on one line, with no `|` (a cell of a Markdown table). Pure.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "/")
}

/// The body of the comment of an item. Pure, host-tested.
pub fn comment_body(item: &PrItem, public_url: Option<&str>) -> String {
    let code = match item_link(public_url, &item.short_code) {
        Some(link) => format!("[{}]({link})", item.short_code),
        None => item.short_code.clone(),
    };
    let claim = match (&item.claim, item.claimed_at) {
        (Some(name), Some(at)) => format!(
            "{}, since {}",
            one_line(name),
            at.format("%Y-%m-%d %H:%M UTC")
        ),
        (Some(name), None) => one_line(name),
        (None, _) => "-".to_string(),
    };
    format!(
        "{MARKER} {} -->\n**{code}** · {}\n\n| Column | Claim |\n|---|---|\n| {} | {claim} |\n\n\
         _Kairos edits this comment when the item changes._\n",
        item.item_id,
        one_line(&item.title),
        column_cell(item),
    )
}

/// The status of the pull request for its items: the state and the
/// description (140 characters at most). Pure, host-tested.
pub fn status_for(items: &[PrItem]) -> (&'static str, String) {
    let cancelled: Vec<&PrItem> = items.iter().filter(|i| i.cancel_reason.is_some()).collect();
    let (state, text) = if !cancelled.is_empty() {
        (
            "failure",
            format!(
                "Cancelled: {}",
                cancelled
                    .iter()
                    .map(|i| i.short_code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    } else if items.iter().all(|i| i.done) {
        (
            "success",
            format!(
                "Done: {}",
                items
                    .iter()
                    .map(|i| i.short_code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    } else {
        (
            "pending",
            items
                .iter()
                .map(|i| {
                    let column = i.column.as_deref().unwrap_or("-");
                    match &i.claim {
                        Some(claim) => format!("{} {column} ({claim})", i.short_code),
                        None => format!("{} {column}", i.short_code),
                    }
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    };
    let text: String = if text.chars().count() > 140 {
        text.chars().take(139).chain(['…']).collect()
    } else {
        text
    };
    (state, text)
}

/// The wait before the next try of a write that failed `tries` times: the
/// base, 2 times longer after each try, at most about one hour.
pub(crate) fn retry_wait(base: Duration, tries: i32) -> Duration {
    let exponent = u32::try_from(tries.saturating_sub(1)).unwrap_or(0).min(10);
    base.checked_mul(1u32 << exponent)
        .unwrap_or(Duration::from_secs(3600))
}

/// The plain write token of a repository, or why not.
pub(crate) fn open_write_token(
    secrets: Option<&SecretsKey>,
    tenant: &str,
    token: &db::StoredWriteToken,
) -> Result<String, String> {
    let key = secrets.ok_or_else(|| {
        "The deployment has no KAIROS_SECRETS_KEY, so Kairos cannot open the write token."
            .to_string()
    })?;
    let plain = key
        .open(
            &crate::secrets::write_token_aad(tenant, token.repository_id),
            &token.key_id,
            &token.nonce,
            &token.ciphertext,
        )
        .map_err(|e| e.to_string())?;
    String::from_utf8(plain).map_err(|_| "The stored token does not decrypt.".to_string())
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The runner of the write-back jobs.
#[derive(Clone)]
pub struct WriteBackService {
    blocking: BlockingTenantPool,
    secrets: Option<SecretsKey>,
    settings: WriteBackSettings,
    public_url: Option<String>,
}

impl std::fmt::Debug for WriteBackService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteBackService")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// One pull request to write, with the token.
struct Pr {
    repository_id: Uuid,
    full_name: String,
    number: i64,
    token: String,
}

impl WriteBackService {
    /// A runner that opens the tokens with `secrets`, and links the items
    /// with `public_url` (`KAIROS_PUBLIC_URL`).
    pub fn new(
        blocking: BlockingTenantPool,
        secrets: Option<SecretsKey>,
        settings: WriteBackSettings,
        public_url: Option<String>,
    ) -> Self {
        WriteBackService {
            blocking,
            secrets,
            settings,
            public_url: public_url.map(|url| url.trim_end_matches('/').to_string()),
        }
    }

    /// Run the write-back jobs on `worker`.
    pub fn register(self, worker: &Worker) {
        worker.register(KIND, Arc::new(self));
    }

    fn client(&self) -> Result<reqwest::Client, WriteError> {
        reqwest::Client::builder()
            .timeout(self.settings.timeout)
            .user_agent(concat!("Kairos/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| WriteError {
                reason: format!("The HTTP client did not start: {e}."),
                retry: true,
            })
    }

    fn api(&self, path: &str) -> String {
        format!("{}{path}", self.settings.api_url.trim_end_matches('/'))
    }

    async fn call(
        &self,
        client: &reqwest::Client,
        pr: &Pr,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        what: &str,
    ) -> Result<(reqwest::StatusCode, Value), WriteError> {
        let mut request = client
            .request(method, self.api(path))
            .bearer_auth(&pr.token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| WriteError::of_request(what, e))?;
        let status = response.status();
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// Write the comments and the status of `pr`. The state of the status
    /// after the write.
    async fn write(
        &self,
        handle: &JobHandle,
        tenant: &str,
        pr: &Pr,
        items: &[PrItem],
        before: &PrState,
    ) -> Result<PrState, WriteError> {
        let client = self.client()?;
        let (repo, number, repository_id) = (&pr.full_name, pr.number, pr.repository_id);
        let comments = {
            self.blocking
                .run(tenant, move |conn| {
                    db::comments_of(conn, repository_id, number).map_err(ApiError::internal)
                })
                .await
                .map_err(|e| WriteError {
                    reason: format!("The database did not answer: {}", e.message),
                    retry: true,
                })?
        };
        for item in items {
            handle.progress();
            let body = comment_body(item, self.public_url.as_deref());
            let sha = sha256_hex(&body);
            let existing = comments.get(&item.item_id);
            if existing.is_some_and(|c| c.body_sha256 == sha) {
                continue;
            }
            let payload = json!({ "body": body });
            let mut id = None;
            if let Some(comment) = existing {
                let (status, _) = self
                    .call(
                        &client,
                        pr,
                        reqwest::Method::PATCH,
                        &format!("/repos/{repo}/issues/comments/{}", comment.comment_id),
                        Some(&payload),
                        "edit the comment",
                    )
                    .await?;
                if status.is_success() {
                    id = Some(comment.comment_id);
                } else if status != reqwest::StatusCode::NOT_FOUND {
                    return Err(WriteError::of_status("edit the comment", status));
                }
            }
            let id = match id {
                Some(id) => id,
                // No comment yet, or a person removed it on GitHub.
                None => {
                    let (status, value) = self
                        .call(
                            &client,
                            pr,
                            reqwest::Method::POST,
                            &format!("/repos/{repo}/issues/{number}/comments"),
                            Some(&payload),
                            "make the comment",
                        )
                        .await?;
                    if !status.is_success() {
                        return Err(WriteError::of_status("make the comment", status));
                    }
                    value["id"].as_i64().ok_or_else(|| WriteError {
                        reason: "GitHub gave no id for the comment.".into(),
                        retry: true,
                    })?
                }
            };
            let comment = Comment {
                item_id: item.item_id,
                comment_id: id,
                body_sha256: sha,
            };
            self.blocking
                .run(tenant, move |conn| {
                    db::set_comment(conn, repository_id, number, &comment)
                        .map_err(ApiError::internal)
                })
                .await
                .map_err(|e| WriteError {
                    reason: format!("The database did not answer: {}", e.message),
                    retry: true,
                })?;
        }

        handle.progress();
        let (status, pull) = self
            .call(
                &client,
                pr,
                reqwest::Method::GET,
                &format!("/repos/{repo}/pulls/{number}"),
                None,
                "read the pull request",
            )
            .await?;
        if !status.is_success() {
            return Err(WriteError::of_status("read the pull request", status));
        }
        let sha = pull["head"]["sha"]
            .as_str()
            .ok_or_else(|| WriteError {
                reason: "GitHub gave no head commit for the pull request.".into(),
                retry: true,
            })?
            .to_string();
        let (state, description) = status_for(items);
        let after = PrState {
            head_sha: Some(sha.clone()),
            status_state: Some(state.to_string()),
            status_description: Some(description.clone()),
        };
        if *before == after {
            return Ok(after);
        }
        let mut body = json!({
            "state": state,
            "description": description,
            "context": STATUS_CONTEXT,
        });
        if let Some(link) = items
            .first()
            .and_then(|i| item_link(self.public_url.as_deref(), &i.short_code))
        {
            body["target_url"] = json!(link);
        }
        let (status, _) = self
            .call(
                &client,
                pr,
                reqwest::Method::POST,
                &format!("/repos/{repo}/statuses/{sha}"),
                Some(&body),
                "set the status",
            )
            .await?;
        if !status.is_success() {
            return Err(WriteError::of_status("set the status", status));
        }
        Ok(after)
    }

    async fn run_job(self, handle: JobHandle) -> Result<(), JobError> {
        let job = handle.job().clone();
        let tenant = job
            .tenant
            .clone()
            .ok_or_else(|| JobError::fail("The write-back has no organization."))?;
        let payload = job.payload_json();
        let (Some(repository_id), Some(number)) = (
            payload["repository_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok()),
            payload["pr_number"].as_i64(),
        ) else {
            return Err(JobError::fail(
                "The payload of the write-back has no repository_id or pr_number.",
            ));
        };
        let db_error =
            |e: ApiError| JobError::retry(format!("The database did not answer: {}", e.message));
        // The repository and its token; the lease of the pull request.
        let loaded = self
            .blocking
            .run(&tenant, move |conn| {
                let Some(target) = db::target(conn, repository_id).map_err(ApiError::internal)?
                else {
                    return Ok(None);
                };
                let Some(token) =
                    db::load_token(conn, repository_id).map_err(ApiError::internal)?
                else {
                    return Ok(None);
                };
                if target.forge != "github" {
                    return Ok(None);
                }
                let Some(before) =
                    db::claim_pr(conn, repository_id, number).map_err(ApiError::internal)?
                else {
                    return Ok(Some(None));
                };
                let items =
                    db::pr_items(conn, repository_id, number).map_err(ApiError::internal)?;
                Ok(Some(Some((target, token, before, items))))
            })
            .await
            .map_err(db_error)?;
        let (target, token, before, items) = match loaded {
            // No repository, no token, or not GitHub: nothing to write.
            None => return Ok(()),
            Some(None) => {
                return Err(JobError::not_here(
                    "Another job writes this pull request now.",
                ));
            }
            Some(Some(loaded)) => loaded,
        };
        let token = match self.open(&tenant, &token) {
            Ok(token) => token,
            Err(reason) => {
                self.end(&tenant, repository_id, number, None, Some(&reason))
                    .await;
                return Err(JobError::fail(reason));
            }
        };
        if items.is_empty() {
            self.end(&tenant, repository_id, number, None, None).await;
            return Ok(());
        }
        let pr = Pr {
            repository_id,
            full_name: target.repo_full_name.clone(),
            number,
            token,
        };
        match self.write(&handle, &tenant, &pr, &items, &before).await {
            Ok(after) => {
                self.end(&tenant, repository_id, number, Some(after), None)
                    .await;
                tracing::info!(tenant = %tenant, repository = %target.slug, pr = number, "github write-back done");
                Ok(())
            }
            Err(error) => {
                self.end(&tenant, repository_id, number, None, Some(&error.reason))
                    .await;
                tracing::warn!(tenant = %tenant, repository = %target.slug, pr = number, reason = %error.reason, "github write-back failed");
                if error.retry {
                    let wait = retry_wait(self.settings.retry_base, job.tries);
                    Err(JobError::retry_in(error.reason, wait))
                } else {
                    Err(JobError::fail(error.reason))
                }
            }
        }
    }

    /// The plain write token, or why not.
    fn open(&self, tenant: &str, token: &db::StoredWriteToken) -> Result<String, String> {
        open_write_token(self.secrets.as_ref(), tenant, token)
    }

    /// End the lease of the pull request, and record the result of the
    /// write on the token row.
    async fn end(
        &self,
        tenant: &str,
        repository_id: Uuid,
        number: i64,
        after: Option<PrState>,
        error: Option<&str>,
    ) {
        let error = error.map(str::to_string);
        let wrote = after.is_some() || error.is_some();
        let result = self
            .blocking
            .run(tenant, move |conn| {
                db::release_pr(conn, repository_id, number, after.as_ref())
                    .map_err(ApiError::internal)?;
                if wrote {
                    db::record_write(conn, repository_id, error.is_none(), error.as_deref())
                        .map_err(ApiError::internal)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(error = ?e, "github write-back end not written");
        }
    }
}

impl JobHandler for WriteBackService {
    fn run(&self, handle: JobHandle) -> JobFuture {
        Box::pin(self.clone().run_job(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn item(code: &str) -> PrItem {
        PrItem {
            item_id: Uuid::nil(),
            short_code: code.to_string(),
            title: "Fix the login".to_string(),
            entity_type: "task".to_string(),
            column: Some("Active".to_string()),
            done: false,
            cancel_reason: None,
            claim: Some("Robin".to_string()),
            claimed_at: Some(
                chrono::Utc
                    .with_ymd_and_hms(2026, 10, 12, 9, 15, 0)
                    .unwrap(),
            ),
            archived: false,
        }
    }

    #[test]
    fn the_comment_has_the_code_the_title_the_column_the_claim_and_the_link() {
        let body = comment_body(&item("ACME-T-1"), Some("https://kairos.example.com"));
        assert!(
            body.starts_with("<!-- kairos:writeback 00000000-"),
            "{body}"
        );
        assert!(
            body.contains(
                "**[ACME-T-1](https://kairos.example.com/items/ACME-T-1)** · Fix the login"
            )
        );
        assert!(
            body.contains("| Active | Robin, since 2026-10-12 09:15 UTC |"),
            "{body}"
        );
        let cancelled = PrItem {
            column: Some("Completed".into()),
            done: true,
            cancel_reason: Some("Not | needed\nnow".into()),
            claim: None,
            ..item("ACME-T-2")
        };
        let body = comment_body(&cancelled, None);
        assert!(body.contains("**ACME-T-2**"), "{body}");
        assert!(
            body.contains("| Completed (cancelled: Not / needed now) | - |"),
            "{body}"
        );
    }

    #[test]
    fn the_status_follows_the_items() {
        let active = item("ACME-T-1");
        let done = PrItem {
            done: true,
            column: Some("Completed".into()),
            ..item("ACME-T-2")
        };
        let cancelled = PrItem {
            cancel_reason: Some("no".into()),
            done: true,
            ..item("ACME-T-3")
        };
        assert_eq!(
            status_for(std::slice::from_ref(&active)),
            ("pending", "ACME-T-1 Active (Robin)".to_string())
        );
        assert_eq!(
            status_for(std::slice::from_ref(&done)),
            ("success", "Done: ACME-T-2".to_string())
        );
        assert_eq!(status_for(&[active.clone(), done.clone()]).0, "pending");
        assert_eq!(
            status_for(&[done, cancelled]),
            ("failure", "Cancelled: ACME-T-3".to_string())
        );
        let many: Vec<PrItem> = (0..20).map(|n| item(&format!("ACME-T-{n}"))).collect();
        assert_eq!(status_for(&many).1.chars().count(), 140);
    }

    #[test]
    fn a_refusal_of_the_token_is_not_tried_again() {
        assert!(!WriteError::of_status("x", reqwest::StatusCode::UNAUTHORIZED).retry);
        assert!(!WriteError::of_status("x", reqwest::StatusCode::FORBIDDEN).retry);
        assert!(WriteError::of_status("x", reqwest::StatusCode::BAD_GATEWAY).retry);
        assert!(WriteError::of_status("x", reqwest::StatusCode::TOO_MANY_REQUESTS).retry);
    }
}
