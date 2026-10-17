//! Git-forge payload normalization (KAIROS-T-0098, design in
//! KAIROS-I-0009): two forge vocabularies in, one [`ForgeEvent`] out,
//! plus short-code extraction.
//!
//! Pure by design (A-0009): no I/O, no database, no HTTP. Parsers take
//! the RAW body string because the webhook endpoint must verify the HMAC
//! over exactly the bytes that arrived before it parses anything — the
//! signature path must never depend on a round-tripped value.
//!
//! # What "one event shape" hides
//!
//! - GitHub has **no `merged` action**: a merged pull request arrives as
//!   `closed` with `pull_request.merged == true`. Reading the action
//!   alone silently records every merge as a close.
//! - GitLab calls them merge requests, numbers them with the
//!   project-scoped `iid` (the number humans see, not `id`), and reports
//!   drafts through `work_in_progress`.
//! - Both send event types nobody asked for (pings, tag pushes). Those
//!   normalize to `None`, never an error — an error would make the forge
//!   retry a delivery that will never succeed.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::short_code::ItemType;

/// What a link points at. A GitHub pull request and a GitLab merge
/// request are the same thing under different names — one variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Branch,
    PullRequest,
}

impl LinkKind {
    /// The wire/storage string (matches `item_links.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Branch => "branch",
            LinkKind::PullRequest => "pull_request",
        }
    }
}

/// The forge-side state of a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Open,
    Merged,
    Closed,
    Draft,
}

impl LinkState {
    /// The wire/storage string (matches `item_links.state`).
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Open => "open",
            LinkState::Merged => "merged",
            LinkState::Closed => "closed",
            LinkState::Draft => "draft",
        }
    }
}

/// One normalized forge event, ready to become an `item_links` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeEvent {
    pub kind: LinkKind,
    /// PR/MR number, or the branch ref.
    pub external_id: String,
    pub title: String,
    pub url: String,
    pub state: LinkState,
    pub author: String,
    /// The forge's own last-update time — the ordering guard the upsert
    /// compares against.
    pub forge_updated_at: DateTime<Utc>,
    /// Everything short codes may appear in (branch name, title, body),
    /// concatenated so extraction has one input.
    pub match_text: String,
    /// The branch: the head branch of a pull request, or the branch
    /// itself (KAIROS-T-0479: the filter reads it).
    pub head_branch: String,
    /// The repository the event came from (`owner/repo`), used to resolve
    /// the connection.
    pub repo_full_name: String,
}

/// Parse a GitHub delivery. `event_type` is the `X-GitHub-Event` header.
/// Returns `None` for event types this integration does not consume.
pub fn parse_github(event_type: &str, body: &str) -> Option<ForgeEvent> {
    let v: Value = serde_json::from_str(body).ok()?;
    let repo_full_name = v["repository"]["full_name"].as_str()?.to_string();
    match event_type {
        "pull_request" => {
            let pr = &v["pull_request"];
            let merged = pr["merged"].as_bool().unwrap_or(false);
            let draft = pr["draft"].as_bool().unwrap_or(false);
            // GitHub has no `merged` action: a merge is `closed` + merged.
            let state = match (pr["state"].as_str().unwrap_or("open"), merged, draft) {
                (_, true, _) => LinkState::Merged,
                ("closed", false, _) => LinkState::Closed,
                (_, false, true) => LinkState::Draft,
                _ => LinkState::Open,
            };
            let number = pr["number"].as_i64()?;
            let title = pr["title"].as_str().unwrap_or_default().to_string();
            let body_text = pr["body"].as_str().unwrap_or_default();
            let branch = pr["head"]["ref"].as_str().unwrap_or_default();
            Some(ForgeEvent {
                kind: LinkKind::PullRequest,
                external_id: number.to_string(),
                match_text: format!("{branch} {title} {body_text}"),
                head_branch: branch.to_string(),
                title,
                url: pr["html_url"].as_str().unwrap_or_default().to_string(),
                state,
                author: pr["user"]["login"].as_str().unwrap_or_default().to_string(),
                forge_updated_at: parse_time(pr["updated_at"].as_str())?,
                repo_full_name,
            })
        }
        // Branch creation. `create` also fires for tags — only branches.
        "create" if v["ref_type"].as_str() == Some("branch") => {
            let branch = v["ref"].as_str()?.to_string();
            let repo_url = v["repository"]["html_url"].as_str().unwrap_or_default();
            Some(ForgeEvent {
                kind: LinkKind::Branch,
                url: format!("{repo_url}/tree/{branch}"),
                title: branch.clone(),
                match_text: branch.clone(),
                head_branch: branch.clone(),
                external_id: branch,
                state: LinkState::Open,
                author: v["sender"]["login"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                // `create` carries no timestamp; the delivery IS the event.
                forge_updated_at: Utc::now(),
                repo_full_name,
            })
        }
        // Branch deletion (KAIROS-T-0480): the branch is closed. `delete`
        // also fires for tags — only branches.
        "delete" if v["ref_type"].as_str() == Some("branch") => {
            let branch = v["ref"].as_str()?.to_string();
            let repo_url = v["repository"]["html_url"].as_str().unwrap_or_default();
            Some(ForgeEvent {
                kind: LinkKind::Branch,
                url: format!("{repo_url}/tree/{branch}"),
                title: branch.clone(),
                match_text: branch.clone(),
                head_branch: branch.clone(),
                external_id: branch,
                state: LinkState::Closed,
                author: v["sender"]["login"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                forge_updated_at: Utc::now(),
                repo_full_name,
            })
        }
        _ => None,
    }
}

/// Parse a GitLab delivery. `event_type` is the `X-Gitlab-Event` header.
pub fn parse_gitlab(event_type: &str, body: &str) -> Option<ForgeEvent> {
    let v: Value = serde_json::from_str(body).ok()?;
    let repo_full_name = v["project"]["path_with_namespace"].as_str()?.to_string();
    match event_type {
        "Merge Request Hook" => {
            let attrs = &v["object_attributes"];
            let draft = attrs["work_in_progress"].as_bool().unwrap_or(false);
            let state = match (attrs["state"].as_str().unwrap_or("opened"), draft) {
                ("merged", _) => LinkState::Merged,
                ("closed", _) => LinkState::Closed,
                (_, true) => LinkState::Draft,
                _ => LinkState::Open,
            };
            // `iid` is the project-scoped number humans see, not `id`.
            let iid = attrs["iid"].as_i64()?;
            let title = attrs["title"].as_str().unwrap_or_default().to_string();
            let description = attrs["description"].as_str().unwrap_or_default();
            let branch = attrs["source_branch"].as_str().unwrap_or_default();
            Some(ForgeEvent {
                kind: LinkKind::PullRequest,
                external_id: iid.to_string(),
                match_text: format!("{branch} {title} {description}"),
                head_branch: branch.to_string(),
                title,
                url: attrs["url"].as_str().unwrap_or_default().to_string(),
                state,
                author: v["user"]["username"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                forge_updated_at: parse_time(attrs["updated_at"].as_str())?,
                repo_full_name,
            })
        }
        "Push Hook" => {
            // refs/heads/<branch>; tag pushes use refs/tags and are ignored.
            let branch = v["ref"].as_str()?.strip_prefix("refs/heads/")?.to_string();
            let repo_url = v["project"]["web_url"].as_str().unwrap_or_default();
            // KAIROS-T-0480: a push whose `after` is all zeros deletes the
            // branch.
            let deleted = v["after"]
                .as_str()
                .is_some_and(|after| !after.is_empty() && after.bytes().all(|b| b == b'0'));
            Some(ForgeEvent {
                kind: LinkKind::Branch,
                url: format!("{repo_url}/-/tree/{branch}"),
                title: branch.clone(),
                match_text: branch.clone(),
                head_branch: branch.clone(),
                external_id: branch,
                state: if deleted {
                    LinkState::Closed
                } else {
                    LinkState::Open
                },
                author: v["user_username"].as_str().unwrap_or_default().to_string(),
                forge_updated_at: Utc::now(),
                repo_full_name,
            })
        }
        _ => None,
    }
}

/// An opened issue (KAIROS-T-0482, KAIROS-A-0028). Kairos makes one
/// support task of it; nothing is synced after that, so only the open
/// action is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeIssue {
    /// The forge's own id of the issue (GitHub `issue.id`, GitLab
    /// `object_attributes.id` / `id`). The key of the intake: a redelivered
    /// event finds it and makes nothing.
    pub issue_id: String,
    /// The number that people see (`#12`; GitLab `iid`).
    pub number: i64,
    pub title: String,
    pub body: String,
    pub url: String,
    /// The forge login of the author.
    pub author: String,
    /// `owner/repo`, to resolve the connection.
    pub repo_full_name: String,
    pub created_at: DateTime<Utc>,
}

/// An issue object of GitHub: the `issue` of an `issues` delivery, or one
/// row of `GET /repos/{owner}/{repo}/issues`. `None` for a pull request
/// (the list of issues has them too, with a `pull_request` key).
pub fn github_issue(issue: &Value, repo_full_name: &str) -> Option<ForgeIssue> {
    if !issue["pull_request"].is_null() {
        return None;
    }
    Some(ForgeIssue {
        issue_id: issue["id"].as_i64()?.to_string(),
        number: issue["number"].as_i64()?,
        title: issue["title"].as_str().unwrap_or_default().to_string(),
        body: issue["body"].as_str().unwrap_or_default().to_string(),
        url: issue["html_url"].as_str().unwrap_or_default().to_string(),
        author: issue["user"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        repo_full_name: repo_full_name.to_string(),
        created_at: parse_time(issue["created_at"].as_str()).unwrap_or_else(Utc::now),
    })
}

/// An issue object of the GitLab API (`GET /projects/:id/issues`). `None`
/// for a confidential issue: it stays on the forge.
pub fn gitlab_api_issue(issue: &Value, repo_full_name: &str) -> Option<ForgeIssue> {
    if issue["confidential"].as_bool() == Some(true) {
        return None;
    }
    Some(ForgeIssue {
        issue_id: issue["id"].as_i64()?.to_string(),
        number: issue["iid"].as_i64()?,
        title: issue["title"].as_str().unwrap_or_default().to_string(),
        body: issue["description"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        url: issue["web_url"].as_str().unwrap_or_default().to_string(),
        author: issue["author"]["username"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        repo_full_name: repo_full_name.to_string(),
        created_at: parse_time(issue["created_at"].as_str()).unwrap_or_else(Utc::now),
    })
}

/// Parse a GitHub `issues` delivery. Only the action `opened` gives an
/// issue: closed, edited, reopened and the others change nothing
/// (KAIROS-A-0028).
pub fn parse_github_issue(event_type: &str, body: &str) -> Option<ForgeIssue> {
    if event_type != "issues" {
        return None;
    }
    let v: Value = serde_json::from_str(body).ok()?;
    if v["action"].as_str() != Some("opened") {
        return None;
    }
    let repo_full_name = v["repository"]["full_name"].as_str()?;
    github_issue(&v["issue"], repo_full_name)
}

/// Parse a GitLab `Issue Hook` delivery. Only the action `open` gives an
/// issue. A confidential issue (`Confidential Issue Hook`, or
/// `confidential: true`) is not read: it stays on the forge.
pub fn parse_gitlab_issue(event_type: &str, body: &str) -> Option<ForgeIssue> {
    if event_type != "Issue Hook" {
        return None;
    }
    let v: Value = serde_json::from_str(body).ok()?;
    let attrs = &v["object_attributes"];
    if attrs["action"].as_str() != Some("open") || attrs["confidential"].as_bool() == Some(true) {
        return None;
    }
    Some(ForgeIssue {
        issue_id: attrs["id"].as_i64()?.to_string(),
        number: attrs["iid"].as_i64()?,
        title: attrs["title"].as_str().unwrap_or_default().to_string(),
        body: attrs["description"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        url: attrs["url"].as_str().unwrap_or_default().to_string(),
        author: v["user"]["username"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        repo_full_name: v["project"]["path_with_namespace"].as_str()?.to_string(),
        created_at: parse_time(attrs["created_at"].as_str()).unwrap_or_else(Utc::now),
    })
}

/// GitLab timestamps are not always RFC 3339 (`2026-09-01 10:00:00 UTC`),
/// so try both shapes.
fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?;
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Some(t.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S UTC")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Every Kairos short code in `text`, deduplicated, in order of first
/// appearance.
///
/// The shape is `PREFIX-<letter>-NNNN` (4 to 18 digits, as
/// [`crate::short_code::parse_short_code`]) where the letter set is derived
/// from [`ItemType`] — a sixth entity type cannot silently fail to match.
/// Case-sensitive: short codes are uppercase, and matching lowercase
/// would turn ordinary prose into false links.
pub fn extract_short_codes(text: &str) -> Vec<String> {
    let letters: Vec<u8> = ItemType::ALL.iter().map(|t| t.letter() as u8).collect();
    let bytes = text.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut i = 0usize;
    // A sliding scan, NOT a token match: the primary carrier is a branch
    // name like `dylan/DEMO-T-0002-fix-auth`, where the code is embedded
    // with a prefix before it and a slug after. Anchor on the `-L-NNNN`
    // tail, then walk backwards over the prefix run.
    //
    // Short codes are pure ASCII, so byte scanning is safe on UTF-8 input:
    // non-ASCII bytes match none of these classes, and every slice
    // boundary below lands on a verified ASCII byte.
    while i < bytes.len() {
        // The digit run after `-L-`: the rule of `parse_short_code`, 4 to 18
        // digits (KAIROS-T-0573). A code past 9999 has five or more.
        let digits = if bytes[i] == b'-'
            && i + 3 < bytes.len()
            && letters.contains(&bytes[i + 1])
            && bytes[i + 2] == b'-'
        {
            bytes[i + 3..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count()
        } else {
            0
        };
        let end = i + 3 + digits;
        let tail_matches = crate::short_code::NUMBER_DIGITS.contains(&digits);
        if tail_matches {
            // The prefix is the maximal [A-Z0-9] run ending at `i`, and it
            // must START with a letter (`0EMO-T-0001` is not a code).
            let mut start = i;
            while start > 0
                && (bytes[start - 1].is_ascii_uppercase() || bytes[start - 1].is_ascii_digit())
            {
                start -= 1;
            }
            if start < i && bytes[start].is_ascii_uppercase() {
                let code = &text[start..end];
                if !found.iter().any(|f| f == code) {
                    found.push(code.to_string());
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/forge/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    // ---- short-code extraction --------------------------------------------

    #[test]
    fn extracts_codes_from_branches_titles_and_bodies() {
        assert_eq!(
            extract_short_codes("dylan/DEMO-T-0002-fix-auth"),
            vec!["DEMO-T-0002"]
        );
        assert_eq!(
            extract_short_codes("Fix login (DEMO-T-0002) and docs DEMO-D-0001."),
            vec!["DEMO-T-0002", "DEMO-D-0001"]
        );
        // Every entity letter participates.
        assert_eq!(
            extract_short_codes("A-S-0001 A-I-0002 A-T-0003 A-D-0004 A-A-0005").len(),
            5
        );
    }

    /// KAIROS-T-0573: a code past 9999 links, in a branch and in a title,
    /// and the scan uses the digit rule of `parse_short_code`.
    #[test]
    fn matches_codes_of_five_digits_and_more() {
        assert_eq!(
            extract_short_codes("dylan/DEMO-T-10000-fix-auth"),
            vec!["DEMO-T-10000"]
        );
        assert_eq!(
            extract_short_codes("Fix refunds (DEMO-T-123456)"),
            vec!["DEMO-T-123456"]
        );
        for text in ["DEMO-T-0002", "DEMO-T-10000", "DEMO-T-123456789012345678"] {
            assert_eq!(extract_short_codes(text), vec![text]);
            assert!(
                crate::short_code::parse_short_code(text).is_some(),
                "{text}"
            );
        }
    }

    #[test]
    fn deduplicates_preserving_first_appearance() {
        assert_eq!(
            extract_short_codes("DEMO-T-0002 then DEMO-T-0001 then DEMO-T-0002"),
            vec!["DEMO-T-0002", "DEMO-T-0001"]
        );
    }

    #[test]
    fn rejects_lookalikes() {
        // Lowercase, wrong digit count, unknown type letter, empty or
        // digit-leading prefix.
        for text in [
            "demo-t-0002",
            "DEMO-T-002",
            "DEMO-T-0000000000000000021",
            "DEMO-X-0002",
            "DEMO--0002",
            "0EMO-T-0002",
            "-T-0002",
        ] {
            assert!(
                extract_short_codes(text).is_empty(),
                "{text:?} must not match"
            );
        }
    }

    /// A trailing slug is NORMAL, not a lookalike: branch names carry one
    /// (`dylan/DEMO-T-0002-fix-auth`), which is the whole point of
    /// scanning rather than matching whole tokens.
    #[test]
    fn matches_codes_embedded_in_branch_names() {
        for (text, expected) in [
            ("dylan/DEMO-T-0002-fix-auth", "DEMO-T-0002"),
            ("DEMO-T-0002-EXTRA", "DEMO-T-0002"),
            ("feature/DEMO-I-0003", "DEMO-I-0003"),
            ("refs/heads/carol/DEMO-T-0004-portal", "DEMO-T-0004"),
        ] {
            assert_eq!(extract_short_codes(text), vec![expected], "{text:?}");
        }
    }

    #[test]
    fn trims_surrounding_punctuation() {
        assert_eq!(extract_short_codes("[DEMO-T-0002]"), vec!["DEMO-T-0002"]);
        assert_eq!(extract_short_codes("(DEMO-T-0002),"), vec!["DEMO-T-0002"]);
        assert_eq!(extract_short_codes("#DEMO-T-0002!"), vec!["DEMO-T-0002"]);
    }

    // ---- GitHub -----------------------------------------------------------

    #[test]
    fn github_pull_request_opened() {
        let event = parse_github("pull_request", &fixture("github_pr_opened.json"))
            .expect("PR events normalize");
        assert_eq!(event.kind, LinkKind::PullRequest);
        assert_eq!(event.state, LinkState::Open);
        assert_eq!(event.external_id, "42");
        assert_eq!(event.repo_full_name, "acme/payments-api");
        assert_eq!(event.author, "dylan");
        // Branch, title, and body all feed matching.
        assert_eq!(
            extract_short_codes(&event.match_text),
            vec!["DEMO-T-0002", "DEMO-D-0001"]
        );
    }

    /// The trap: GitHub reports a merge as `closed` + `merged: true`.
    #[test]
    fn github_merged_is_closed_plus_merged_flag() {
        let event = parse_github("pull_request", &fixture("github_pr_merged.json"))
            .expect("merged PR normalizes");
        assert_eq!(
            event.state,
            LinkState::Merged,
            "a merged PR must not be recorded as merely closed"
        );
    }

    #[test]
    fn github_closed_without_merge_is_closed() {
        let event = parse_github("pull_request", &fixture("github_pr_closed.json"))
            .expect("closed PR normalizes");
        assert_eq!(event.state, LinkState::Closed);
    }

    #[test]
    fn github_draft_is_draft() {
        let event = parse_github("pull_request", &fixture("github_pr_draft.json"))
            .expect("draft PR normalizes");
        assert_eq!(event.state, LinkState::Draft);
    }

    #[test]
    fn github_branch_creation() {
        let event =
            parse_github("create", &fixture("github_create_branch.json")).expect("branch event");
        assert_eq!(event.kind, LinkKind::Branch);
        assert_eq!(event.external_id, "dylan/DEMO-T-0002-fix-auth");
        assert_eq!(extract_short_codes(&event.match_text), vec!["DEMO-T-0002"]);
    }

    #[test]
    fn github_tag_creation_and_pings_are_ignored() {
        assert!(parse_github("create", &fixture("github_create_tag.json")).is_none());
        assert!(parse_github("ping", &fixture("github_ping.json")).is_none());
        // Malformed bodies never panic.
        assert!(parse_github("pull_request", "not json").is_none());
        // KAIROS-T-0482: an issue is not a link event; `parse_github_issue`
        // reads it.
        assert!(parse_github("issues", &fixture("github_issue_opened.json")).is_none());
    }

    // ---- issues (KAIROS-T-0482) ------------------------------------------

    #[test]
    fn github_opened_issue() {
        let issue = parse_github_issue("issues", &fixture("github_issue_opened.json"))
            .expect("an opened issue");
        assert_eq!(issue.issue_id, "2401557001");
        assert_eq!(issue.number, 12);
        assert_eq!(issue.title, "Crash when the config file is empty");
        assert!(issue.body.starts_with("Steps:"));
        assert_eq!(issue.url, "https://github.com/acme/payments-api/issues/12");
        assert_eq!(issue.author, "robin");
        assert_eq!(issue.repo_full_name, "acme/payments-api");
        assert_eq!(issue.created_at.to_rfc3339(), "2026-09-05T09:00:00+00:00");
    }

    #[test]
    fn github_issue_actions_other_than_opened_change_nothing() {
        for name in [
            "github_issue_closed.json",
            "github_issue_edited.json",
            "github_issue_reopened.json",
        ] {
            assert!(
                parse_github_issue("issues", &fixture(name)).is_none(),
                "{name}"
            );
        }
        // Another event type, or a malformed body.
        assert!(parse_github_issue("pull_request", &fixture("github_issue_opened.json")).is_none());
        assert!(parse_github_issue("issues", "not json").is_none());
    }

    #[test]
    fn github_issue_list_skips_pull_requests() {
        let v: Value = serde_json::from_str(&fixture("github_issue_opened.json")).unwrap();
        assert!(github_issue(&v["issue"], "acme/payments-api").is_some());
        let mut pr = v["issue"].clone();
        pr["pull_request"] = serde_json::json!({"url": "https://api.github.com/x"});
        assert!(github_issue(&pr, "acme/payments-api").is_none());
    }

    #[test]
    fn gitlab_opened_issue() {
        let issue = parse_gitlab_issue("Issue Hook", &fixture("gitlab_issue_open.json"))
            .expect("an opened issue");
        assert_eq!(issue.issue_id, "301");
        assert_eq!(issue.number, 4);
        assert_eq!(issue.author, "sam");
        assert_eq!(issue.repo_full_name, "acme/portal-web");
        assert_eq!(issue.created_at.to_rfc3339(), "2026-09-05T09:00:00+00:00");
        assert!(parse_gitlab_issue("Issue Hook", &fixture("gitlab_issue_close.json")).is_none());
        assert!(
            parse_gitlab_issue("Issue Hook", &fixture("gitlab_issue_confidential.json")).is_none(),
            "a confidential issue stays on the forge"
        );
        assert!(
            parse_gitlab_issue(
                "Confidential Issue Hook",
                &fixture("gitlab_issue_open.json")
            )
            .is_none()
        );
        // An issue is not a link event.
        assert!(parse_gitlab("Issue Hook", &fixture("gitlab_issue_open.json")).is_none());
    }

    #[test]
    fn gitlab_api_issue_rows() {
        let row = serde_json::json!({
            "id": 310, "iid": 9, "title": "T", "description": "D",
            "web_url": "https://gitlab.com/acme/portal-web/-/issues/9",
            "author": {"username": "jamie"}, "created_at": "2026-09-01T10:00:00.000Z",
            "confidential": false
        });
        let issue = gitlab_api_issue(&row, "acme/portal-web").expect("issue");
        assert_eq!((issue.issue_id.as_str(), issue.number), ("310", 9));
        assert_eq!(issue.author, "jamie");
        let mut secret = row.clone();
        secret["confidential"] = serde_json::json!(true);
        assert!(gitlab_api_issue(&secret, "acme/portal-web").is_none());
    }

    #[test]
    fn github_branch_deletion_closes_the_branch() {
        let event =
            parse_github("delete", &fixture("github_delete_branch.json")).expect("branch deletion");
        assert_eq!(event.kind, LinkKind::Branch);
        assert_eq!(event.state, LinkState::Closed);
        assert_eq!(event.external_id, "robin/cleanup");
        assert_eq!(event.head_branch, "robin/cleanup");
        assert_eq!(event.author, "robin");
        // A tag deletion is ignored.
        assert!(parse_github("delete", &fixture("github_create_tag.json")).is_none());
    }

    // ---- GitLab -----------------------------------------------------------

    #[test]
    fn gitlab_push_that_deletes_a_branch_closes_it() {
        let event =
            parse_gitlab("Push Hook", &fixture("gitlab_push_delete.json")).expect("push event");
        assert_eq!(event.state, LinkState::Closed);
        assert_eq!(event.external_id, "sam/old-work");
        let event = parse_gitlab("Push Hook", &fixture("gitlab_push.json")).expect("push event");
        assert_eq!(event.state, LinkState::Open);
    }

    #[test]
    fn gitlab_merge_request_opened() {
        let event = parse_gitlab("Merge Request Hook", &fixture("gitlab_mr_open.json"))
            .expect("MR events normalize");
        assert_eq!(event.kind, LinkKind::PullRequest);
        assert_eq!(event.state, LinkState::Open);
        // `iid` (project-scoped), not the global `id`.
        assert_eq!(event.external_id, "7");
        assert_eq!(event.repo_full_name, "acme/portal-web");
        assert_eq!(extract_short_codes(&event.match_text), vec!["DEMO-T-0004"]);
    }

    #[test]
    fn gitlab_merged_and_draft_states() {
        let merged = parse_gitlab("Merge Request Hook", &fixture("gitlab_mr_merged.json"))
            .expect("merged MR");
        assert_eq!(merged.state, LinkState::Merged);
        let draft =
            parse_gitlab("Merge Request Hook", &fixture("gitlab_mr_draft.json")).expect("draft MR");
        assert_eq!(draft.state, LinkState::Draft);
    }

    #[test]
    fn gitlab_push_hook_branch_only() {
        let event = parse_gitlab("Push Hook", &fixture("gitlab_push.json")).expect("push event");
        assert_eq!(event.kind, LinkKind::Branch);
        assert_eq!(event.external_id, "dylan/DEMO-T-0004-portal");
        // Tag pushes carry refs/tags and are ignored.
        assert!(parse_gitlab("Push Hook", &fixture("gitlab_push_tag.json")).is_none());
        assert!(parse_gitlab("Pipeline Hook", &fixture("gitlab_push.json")).is_none());
    }

    #[test]
    fn gitlab_accepts_its_non_rfc3339_timestamps() {
        // GitLab sends `2026-09-01 10:00:00 UTC` in places.
        let event = parse_gitlab("Merge Request Hook", &fixture("gitlab_mr_open.json"))
            .expect("MR with GitLab-style timestamp");
        assert_eq!(
            event.forge_updated_at.to_rfc3339(),
            "2026-09-01T10:00:00+00:00"
        );
    }
}
