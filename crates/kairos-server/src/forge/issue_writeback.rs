//! The issue write-back (KAIROS-T-0575): the runner of the jobs of the kind
//! `issue_writeback` ([`kairos_db::issue_writeback`]).
//!
//! A task born from an issue (issue intake, KAIROS-T-0482) writes back to
//! its issue, with the write token of the repository. One job writes one
//! issue:
//!
//! 1. It takes the lease of the issue
//!    ([`kairos_db::issue_writeback::claim_issue`]) and reads the task. A
//!    job that does not get the lease goes back to the queue with no try
//!    counted.
//! 2. The task is done: a comment ("Done in ACME-T-0012; PR #37.") and the
//!    issue closed (GitHub `state_reason` `completed`).
//! 3. The task is cancelled: a comment with the reason, and the issue
//!    closed as not planned (GitHub `state_reason` `not_planned`; GitLab
//!    has no reason, so the issue is closed).
//! 4. The task is reopened: the issue opened again, with no comment.
//!
//! GitHub: `POST /repos/{owner}/{repo}/issues/{number}/comments` and
//! `PATCH /repos/{owner}/{repo}/issues/{number}`. GitLab:
//! `POST /projects/{path}/issues/{iid}/notes` and
//! `PUT /projects/{path}/issues/{iid}` (`state_event`), on the host of the
//! repository.
//!
//! The comment names the task by its code. It links to the item only with
//! `KAIROS_PUBLIC_URL`, as the comment on a pull request does, and only
//! when that URL has a public host ([`public_base`]): an issue can be
//! public, so a private or tailnet name never goes into it. A comment is
//! made once for a state: a retry of a failed close does not comment again.
//!
//! The rules of the write-back of a pull request apply
//! ([`super::writeback`]): no token, no job, and a job that finds no token
//! writes nothing; a failed write is kept on the token row and tried again
//! with growing waits; a refusal (401, 403, 404, 422) is not tried again.

use std::sync::Arc;

use kairos_db::issue_writeback::{self as db, Claim, IssueTarget, TaskPr};
use kairos_db::writeback::{self as tokens, StoredWriteToken, Target};
use serde_json::{Value, json};
use uuid::Uuid;

use super::writeback::{WriteBackSettings, item_link, one_line, open_write_token, retry_wait};
use crate::blocking::BlockingTenantPool;
use crate::error::ApiError;
use crate::jobs::{JobError, JobFuture, JobHandle, JobHandler, Worker};
use crate::secrets::SecretsKey;

/// The kind of the job.
pub const KIND: &str = db::JOB_KIND;

/// The first line of each issue comment that Kairos writes, with the id of
/// the item.
const MARKER: &str = "<!-- kairos:issue-writeback";

/// The forge of an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueForge {
    Github,
    Gitlab,
}

impl IssueForge {
    fn of(forge: &str) -> Option<Self> {
        match forge {
            "github" => Some(IssueForge::Github),
            "gitlab" => Some(IssueForge::Gitlab),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            IssueForge::Github => "GitHub",
            IssueForge::Gitlab => "GitLab",
        }
    }

    /// What the token needs to write an issue.
    fn permission(self) -> &'static str {
        match self {
            IssueForge::Github => "Issues: write on the repository",
            IssueForge::Gitlab => "the scope api on the project",
        }
    }
}

/// The name endings of private networks: a tailnet, mDNS, and the names
/// that RFC 6762, RFC 8375 and common practice keep for local use.
const PRIVATE_SUFFIXES: [&str; 7] = [
    ".ts.net",
    ".local",
    ".localhost",
    ".internal",
    ".lan",
    ".home.arpa",
    ".corp",
];

/// `public_url` when its host is public, else `None`. A private host is an
/// address of a private, loopback, link-local or shared (CGNAT, a tailnet)
/// network, a name with no dot, or a name under a private ending. Pure,
/// host-tested.
pub fn public_base(public_url: Option<&str>) -> Option<&str> {
    let base = public_url?;
    let url = url::Url::parse(base).ok()?;
    let public = match url.host()? {
        url::Host::Ipv4(ip) => {
            let [a, b, ..] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || (a == 100 && (64..128).contains(&b)))
        }
        url::Host::Ipv6(ip) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
        url::Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name.contains('.') && !PRIVATE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        }
    };
    public.then_some(base)
}

/// A text as one sentence: on one line, with a full stop. Pure.
fn sentence(text: &str) -> String {
    let text = one_line(text);
    if text.ends_with(['.', '!', '?']) {
        text
    } else {
        format!("{text}.")
    }
}

/// How the comment names a pull request: `PR #37` (GitHub), `MR !37`
/// (GitLab), with the name of its repository when it is not the repository
/// of the issue. Pure.
fn pr_ref(pr: &TaskPr, issue_repository: Uuid) -> String {
    let (label, sign) = match pr.forge.as_str() {
        "gitlab" => ("MR", '!'),
        _ => ("PR", '#'),
    };
    if pr.repository_id == issue_repository {
        format!("{label} {sign}{}", pr.number)
    } else {
        format!("{label} {}{sign}{}", pr.repo_full_name, pr.number)
    }
}

/// The comment for the state that the task wants, or `None` for a reopen
/// (no comment). Pure, host-tested.
pub fn comment_body(
    target: &IssueTarget,
    prs: &[TaskPr],
    public_url: Option<&str>,
) -> Option<String> {
    let code = match item_link(public_base(public_url), &target.short_code) {
        Some(link) => format!("[{}]({link})", target.short_code),
        None => target.short_code.clone(),
    };
    let text = match target.wanted.as_str() {
        db::COMPLETED => {
            let refs: Vec<String> = prs
                .iter()
                .map(|pr| pr_ref(pr, target.repository_id))
                .collect();
            if refs.is_empty() {
                format!("Done in {code}.")
            } else {
                format!("Done in {code}; {}.", refs.join(", "))
            }
        }
        db::NOT_PLANNED => match target.cancel_reason.as_deref().map(str::trim) {
            Some(reason) if !reason.is_empty() => {
                format!("Cancelled in {code}: {}", sentence(reason))
            }
            _ => format!("Cancelled in {code}."),
        },
        _ => return None,
    };
    Some(format!("{MARKER} {} -->\n{text}\n", target.item_id))
}

/// The body of the change of the state of an issue. Pure, host-tested.
pub fn state_body(forge: IssueForge, wanted: &str) -> Value {
    match (forge, wanted) {
        (IssueForge::Github, db::OPEN) => json!({ "state": "open" }),
        (IssueForge::Github, reason) => json!({ "state": "closed", "state_reason": reason }),
        (IssueForge::Gitlab, db::OPEN) => json!({ "state_event": "reopen" }),
        (IssueForge::Gitlab, _) => json!({ "state_event": "close" }),
    }
}

/// Why a write failed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteError {
    reason: String,
    /// False: the forge refused the token or does not know the issue; a
    /// try again does not mend it.
    retry: bool,
}

impl WriteError {
    fn of_status(forge: IssueForge, what: &str, status: reqwest::StatusCode) -> Self {
        let code = status.as_u16();
        let retry = !matches!(code, 401 | 403 | 404 | 422);
        let name = forge.name();
        let reason = match code {
            401 => format!("{name} refused the write token ({what}: 401). Set a new token."),
            403 => format!(
                "{name} refused the write ({what}: 403). The token needs {}.",
                forge.permission()
            ),
            404 => format!(
                "{name} does not know the issue, or the token cannot see the repository \
                 ({what}: 404)."
            ),
            _ => format!("{name} answered {status} ({what})."),
        };
        WriteError { reason, retry }
    }

    fn of_request(forge: IssueForge, what: &str, error: reqwest::Error) -> Self {
        WriteError {
            reason: format!(
                "The request to {} failed ({what}): {}.",
                forge.name(),
                error.without_url()
            ),
            retry: true,
        }
    }

    fn database(e: ApiError) -> Self {
        WriteError {
            reason: format!("The database did not answer: {}", e.message),
            retry: true,
        }
    }
}

/// The runner of the issue write-back jobs.
#[derive(Clone)]
pub struct IssueWriteBackService {
    blocking: BlockingTenantPool,
    secrets: Option<SecretsKey>,
    settings: WriteBackSettings,
    public_url: Option<String>,
}

impl std::fmt::Debug for IssueWriteBackService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssueWriteBackService")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// One issue to write, with the token.
struct Issue {
    forge: IssueForge,
    /// The base of the paths of the issue on the forge API.
    base: String,
    token: String,
}

/// What a job loads.
enum Loaded {
    /// No repository, no token, not GitHub or GitLab, or no intake row.
    Nothing,
    /// Another job writes this issue now.
    Busy,
    Taken(Box<(Target, StoredWriteToken, IssueTarget, Vec<TaskPr>)>),
}

impl IssueWriteBackService {
    /// A runner that opens the tokens with `secrets`, calls GitHub at
    /// `settings.api_url`, and links the items with `public_url`
    /// (`KAIROS_PUBLIC_URL`).
    pub fn new(
        blocking: BlockingTenantPool,
        secrets: Option<SecretsKey>,
        settings: WriteBackSettings,
        public_url: Option<String>,
    ) -> Self {
        IssueWriteBackService {
            blocking,
            secrets,
            settings,
            public_url: public_url.map(|url| url.trim_end_matches('/').to_string()),
        }
    }

    /// Run the issue write-back jobs on `worker`.
    pub fn register(self, worker: &Worker) {
        worker.register(KIND, Arc::new(self));
    }

    /// The forge API of an issue of `target`.
    fn issue_of(&self, target: &Target, number: i64, token: String) -> Result<Issue, String> {
        let forge = IssueForge::of(&target.forge)
            .ok_or_else(|| format!("The forge {} has no issue write-back.", target.forge))?;
        let base = match forge {
            IssueForge::Github => format!(
                "{}/repos/{}/issues/{number}",
                self.settings.api_url.trim_end_matches('/'),
                target.repo_full_name
            ),
            IssueForge::Gitlab => {
                let api = super::import::gitlab_api_base(&target.repo_url).ok_or_else(|| {
                    format!("The URL of the repository {} has no host.", target.slug)
                })?;
                format!(
                    "{api}/projects/{}/issues/{number}",
                    url::form_urlencoded::byte_serialize(target.repo_full_name.as_bytes())
                        .collect::<String>()
                )
            }
        };
        Ok(Issue { forge, base, token })
    }

    async fn call(
        &self,
        client: &reqwest::Client,
        issue: &Issue,
        method: reqwest::Method,
        path: &str,
        body: &Value,
        what: &str,
    ) -> Result<(), WriteError> {
        let mut request = client
            .request(method, format!("{}{path}", issue.base))
            .json(body);
        request = match issue.forge {
            IssueForge::Gitlab => request.header("PRIVATE-TOKEN", &issue.token),
            IssueForge::Github => request
                .bearer_auth(&issue.token)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
        };
        let response = request
            .send()
            .await
            .map_err(|e| WriteError::of_request(issue.forge, what, e))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(WriteError::of_status(issue.forge, what, status))
        }
    }

    /// Write the comment and the state of the issue.
    async fn write(
        &self,
        handle: &JobHandle,
        tenant: &str,
        issue: &Issue,
        target: &IssueTarget,
        prs: &[TaskPr],
    ) -> Result<(), WriteError> {
        let client = reqwest::Client::builder()
            .timeout(self.settings.timeout)
            .user_agent(concat!("Kairos/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| WriteError {
                reason: format!("The HTTP client did not start: {e}."),
                retry: true,
            })?;
        let commented = target.comment_state.as_deref() == Some(target.wanted.as_str());
        if let (false, Some(body)) = (
            commented,
            comment_body(target, prs, self.public_url.as_deref()),
        ) {
            handle.progress();
            let path = match issue.forge {
                IssueForge::Github => "/comments",
                IssueForge::Gitlab => "/notes",
            };
            self.call(
                &client,
                issue,
                reqwest::Method::POST,
                path,
                &json!({ "body": body }),
                "comment on the issue",
            )
            .await?;
            let (repository_id, issue_id, state) = (
                target.repository_id,
                target.issue_id.clone(),
                target.wanted.clone(),
            );
            self.blocking
                .run(tenant, move |conn| {
                    db::set_comment_state(conn, repository_id, &issue_id, &state)
                        .map_err(ApiError::internal)
                })
                .await
                .map_err(WriteError::database)?;
        }
        handle.progress();
        let (method, what) = match (issue.forge, target.wanted.as_str()) {
            (IssueForge::Github, db::OPEN) => (reqwest::Method::PATCH, "reopen the issue"),
            (IssueForge::Github, _) => (reqwest::Method::PATCH, "close the issue"),
            (IssueForge::Gitlab, db::OPEN) => (reqwest::Method::PUT, "reopen the issue"),
            (IssueForge::Gitlab, _) => (reqwest::Method::PUT, "close the issue"),
        };
        self.call(
            &client,
            issue,
            method,
            "",
            &state_body(issue.forge, &target.wanted),
            what,
        )
        .await
    }

    async fn run_job(self, handle: JobHandle) -> Result<(), JobError> {
        let job = handle.job().clone();
        let tenant = job
            .tenant
            .clone()
            .ok_or_else(|| JobError::fail("The issue write-back has no organization."))?;
        let payload = job.payload_json();
        let (Some(repository_id), Some(issue_id)) = (
            payload["repository_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok()),
            payload["issue_id"].as_str().map(str::to_string),
        ) else {
            return Err(JobError::fail(
                "The payload of the issue write-back has no repository_id or issue_id.",
            ));
        };
        let db_error =
            |e: ApiError| JobError::retry(format!("The database did not answer: {}", e.message));
        let key = issue_id.clone();
        let loaded = self
            .blocking
            .run(&tenant, move |conn| {
                let Some(target) =
                    tokens::target(conn, repository_id).map_err(ApiError::internal)?
                else {
                    return Ok(Loaded::Nothing);
                };
                if IssueForge::of(&target.forge).is_none() {
                    return Ok(Loaded::Nothing);
                }
                let Some(token) =
                    tokens::load_token(conn, repository_id).map_err(ApiError::internal)?
                else {
                    return Ok(Loaded::Nothing);
                };
                match db::claim_issue(conn, repository_id, &key).map_err(ApiError::internal)? {
                    Claim::Missing => Ok(Loaded::Nothing),
                    Claim::Busy => Ok(Loaded::Busy),
                    Claim::Taken(issue) => {
                        let prs = db::task_prs(conn, issue.item_id).map_err(ApiError::internal)?;
                        Ok(Loaded::Taken(Box::new((target, token, issue, prs))))
                    }
                }
            })
            .await
            .map_err(db_error)?;
        let (target, token, issue, prs) = match loaded {
            Loaded::Nothing => return Ok(()),
            Loaded::Busy => {
                return Err(JobError::not_here("Another job writes this issue now."));
            }
            Loaded::Taken(loaded) => *loaded,
        };
        if issue.wanted == issue.written_state {
            self.end(&tenant, repository_id, &issue_id, None, None)
                .await;
            return Ok(());
        }
        let opened = open_write_token(self.secrets.as_ref(), &tenant, &token)
            .and_then(|token| self.issue_of(&target, issue.number, token));
        let forge_issue = match opened {
            Ok(forge_issue) => forge_issue,
            Err(reason) => {
                self.end(&tenant, repository_id, &issue_id, None, Some(&reason))
                    .await;
                return Err(JobError::fail(reason));
            }
        };
        match self
            .write(&handle, &tenant, &forge_issue, &issue, &prs)
            .await
        {
            Ok(()) => {
                self.end(&tenant, repository_id, &issue_id, Some(&issue.wanted), None)
                    .await;
                tracing::info!(tenant = %tenant, repository = %target.slug, issue = issue.number, state = %issue.wanted, "issue write-back done");
                Ok(())
            }
            Err(error) => {
                self.end(&tenant, repository_id, &issue_id, None, Some(&error.reason))
                    .await;
                tracing::warn!(tenant = %tenant, repository = %target.slug, issue = issue.number, reason = %error.reason, "issue write-back failed");
                if error.retry {
                    let wait = retry_wait(self.settings.retry_base, job.tries);
                    Err(JobError::retry_in(error.reason, wait))
                } else {
                    Err(JobError::fail(error.reason))
                }
            }
        }
    }

    /// End the lease of the issue; record the state that was written, and
    /// the result of a write on the token row.
    async fn end(
        &self,
        tenant: &str,
        repository_id: Uuid,
        issue_id: &str,
        written: Option<&str>,
        error: Option<&str>,
    ) {
        let (issue_id, written, error) = (
            issue_id.to_string(),
            written.map(str::to_string),
            error.map(str::to_string),
        );
        let wrote = written.is_some() || error.is_some();
        let result = self
            .blocking
            .run(tenant, move |conn| {
                db::release_issue(conn, repository_id, &issue_id, written.as_deref())
                    .map_err(ApiError::internal)?;
                if wrote {
                    tokens::record_write(conn, repository_id, error.is_none(), error.as_deref())
                        .map_err(ApiError::internal)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(error = ?e, "issue write-back end not written");
        }
    }
}

impl JobHandler for IssueWriteBackService {
    fn run(&self, handle: JobHandle) -> JobFuture {
        Box::pin(self.clone().run_job(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(wanted: &str) -> IssueTarget {
        IssueTarget {
            repository_id: Uuid::nil(),
            issue_id: "2401557098".into(),
            number: 12,
            item_id: Uuid::nil(),
            short_code: "ACME-T-0012".into(),
            written_state: "open".into(),
            comment_state: None,
            wanted: wanted.into(),
            cancel_reason: None,
        }
    }

    fn pr(repository_id: Uuid, forge: &str, number: &str) -> TaskPr {
        TaskPr {
            repository_id,
            repo_full_name: "acme/other".into(),
            forge: forge.into(),
            number: number.into(),
        }
    }

    #[test]
    fn the_done_comment_names_the_code_and_the_pull_requests() {
        let done = target("completed");
        let body = comment_body(&done, &[], None).expect("a comment");
        assert!(
            body.starts_with("<!-- kairos:issue-writeback 00000000-"),
            "{body}"
        );
        assert!(body.ends_with("\nDone in ACME-T-0012.\n"), "{body}");
        let prs = [
            pr(Uuid::nil(), "github", "37"),
            pr(Uuid::from_u128(1), "github", "4"),
        ];
        let body = comment_body(&done, &prs, None).expect("a comment");
        assert!(
            body.contains("Done in ACME-T-0012; PR #37, PR acme/other#4."),
            "{body}"
        );
        let body = comment_body(&done, &[pr(Uuid::nil(), "gitlab", "5")], None).expect("comment");
        assert!(body.contains("Done in ACME-T-0012; MR !5."), "{body}");
    }

    #[test]
    fn the_link_needs_a_public_url() {
        let body = comment_body(
            &target("completed"),
            &[],
            Some("https://kairos.example.com"),
        )
        .expect("a comment");
        assert!(
            body.contains("Done in [ACME-T-0012](https://kairos.example.com/items/ACME-T-0012)."),
            "{body}"
        );
        let body = comment_body(&target("completed"), &[], None).expect("a comment");
        assert!(
            !body.contains("http"),
            "no link without a public URL: {body}"
        );
    }

    #[test]
    fn a_private_host_gets_no_link() {
        for public in [
            "https://kairos.example.com",
            "https://192.0.2.10",
            "https://[2001:db8::1]",
        ] {
            assert_eq!(public_base(Some(public)), Some(public), "{public}");
        }
        for private in [
            "https://kairos.tail0000.ts.net",
            "http://kairos.local",
            "http://kairos",
            "http://localhost:8080",
            "http://10.0.0.5",
            "http://192.168.1.20",
            "http://172.16.0.1",
            "http://100.101.102.103",
            "http://127.0.0.1",
            "http://[fd00::1]",
            "http://kairos.home.arpa",
            "not a url",
        ] {
            assert_eq!(public_base(Some(private)), None, "{private}");
        }
        assert_eq!(public_base(None), None);
        let body = comment_body(
            &target("completed"),
            &[],
            Some("https://kairos.tail0000.ts.net"),
        )
        .expect("a comment");
        assert!(!body.contains("ts.net"), "{body}");
    }

    #[test]
    fn the_cancel_comment_has_the_reason_and_a_reopen_has_none() {
        let cancelled = IssueTarget {
            cancel_reason: Some("Not needed\nnow".into()),
            ..target("not_planned")
        };
        let body = comment_body(&cancelled, &[], None).expect("a comment");
        assert!(
            body.contains("Cancelled in ACME-T-0012: Not needed now."),
            "{body}"
        );
        let blank = IssueTarget {
            cancel_reason: Some("  ".into()),
            ..target("not_planned")
        };
        assert!(
            comment_body(&blank, &[], None)
                .expect("a comment")
                .contains("Cancelled in ACME-T-0012.")
        );
        assert_eq!(comment_body(&target("open"), &[], None), None);
    }

    #[test]
    fn the_state_body_follows_the_forge() {
        assert_eq!(
            state_body(IssueForge::Github, "completed"),
            json!({"state": "closed", "state_reason": "completed"})
        );
        assert_eq!(
            state_body(IssueForge::Github, "not_planned"),
            json!({"state": "closed", "state_reason": "not_planned"})
        );
        assert_eq!(
            state_body(IssueForge::Github, "open"),
            json!({"state": "open"})
        );
        assert_eq!(
            state_body(IssueForge::Gitlab, "not_planned"),
            json!({"state_event": "close"})
        );
        assert_eq!(
            state_body(IssueForge::Gitlab, "open"),
            json!({"state_event": "reopen"})
        );
    }

    #[test]
    fn a_refusal_is_not_tried_again_and_names_the_permission() {
        let refused = WriteError::of_status(
            IssueForge::Gitlab,
            "close the issue",
            reqwest::StatusCode::FORBIDDEN,
        );
        assert!(!refused.retry);
        assert!(
            refused.reason.contains("the scope api"),
            "{}",
            refused.reason
        );
        let refused = WriteError::of_status(
            IssueForge::Github,
            "close the issue",
            reqwest::StatusCode::FORBIDDEN,
        );
        assert!(
            refused.reason.contains("Issues: write"),
            "{}",
            refused.reason
        );
        for code in [401, 404, 422] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(!WriteError::of_status(IssueForge::Github, "x", status).retry);
        }
        assert!(
            WriteError::of_status(IssueForge::Github, "x", reqwest::StatusCode::BAD_GATEWAY).retry
        );
    }
}
