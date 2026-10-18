//! Issue intake (KAIROS-T-0482, decision KAIROS-A-0028): an opened GitHub
//! or GitLab issue that the forge filter of the repository admits becomes
//! ONE support task in the entry column, Support lane, of the delivery
//! board of the team of the repository.
//!
//! - The forge system actor (KAIROS-T-0481) creates the task. It is a
//!   request like any other: the same rule as `POST /api/tasks` from a
//!   member who does not manage the board, so the board's usual triage
//!   (a person, or an agent where the board allows it) takes it.
//! - The key is the forge's own id of the issue (`forge_issue_intake`). A
//!   redelivered event, or an import of an issue that a webhook took
//!   already (KAIROS-T-0483), makes nothing.
//! - The task links to the issue (`item_links`, kind `issue`) with its URL,
//!   number and the login of its author.
//! - After intake nothing is read from the issue. With a write token, the
//!   end of the task writes back to it ([`super::issue_writeback`],
//!   KAIROS-T-0575).
//!
//! The webhook and the import call [`take_issue`]: one path.

use diesel::pg::PgConnection;
use diesel::prelude::*;
use kairos_core::forge::ForgeIssue;
use kairos_db::events::{EventKind, ThinEvent, emit_event};
use kairos_db::models::enums::{ActivityAction, LinkKind, LinkState, TaskType};
use kairos_db::models::forge::NewItemLink;
use kairos_db::models::graph::NewActivityLogEntry;
use kairos_db::{items, repositories};
use uuid::Uuid;

use crate::error::ApiError;

/// Where the issue came from (`forge_issue_intake.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Webhook,
    Import,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Webhook => "webhook",
            Source::Import => "import",
        }
    }
}

/// What [`take_issue`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A task was made; its short code.
    Created(String),
    /// The issue was taken before: nothing was made.
    Duplicate,
    /// The filter of the repository does not admit it: nothing was made.
    Filtered,
    /// The task cannot be made (for example, the team of the repository has
    /// no delivery board). Nothing was written; the reason says why.
    NotRouted(String),
}

/// The longest title that a task gets from an issue, in characters.
const MAX_TITLE_CHARS: usize = 250;

/// The title of the task: the title of the issue, or `Issue #N` when it is
/// blank. Pure.
pub(crate) fn task_title(issue: &ForgeIssue) -> String {
    let title = issue.title.trim();
    if title.is_empty() {
        return format!("Issue #{}", issue.number);
    }
    title.chars().take(MAX_TITLE_CHARS).collect()
}

/// The content of the task: where the issue is, who opened it, and the
/// text of the issue. Pure.
pub(crate) fn task_content(issue: &ForgeIssue, forge: &str) -> String {
    let author = if issue.author.is_empty() {
        String::new()
    } else {
        format!(", opened by {}", issue.author)
    };
    let body = issue.body.trim();
    let body = if body.is_empty() {
        "(The issue has no text.)"
    } else {
        body
    };
    format!(
        "Issue [#{number}]({url}) on {forge}{author}. Kairos took it from the forge; \
         nothing is synced after this.\n\n---\n\n{body}\n",
        number = issue.number,
        url = issue.url,
    )
}

/// Take one issue of a repository: filter, route, then create the task
/// once. Runs in one transaction; a refusal writes nothing.
pub(crate) fn take_issue(
    conn: &mut PgConnection,
    tenant_slug: &str,
    actor: Uuid,
    repository_id: Uuid,
    connection_id: Uuid,
    issue: &ForgeIssue,
    source: Source,
) -> Result<Outcome, ApiError> {
    let filter =
        kairos_db::forge_filters::filter(conn, repository_id).map_err(ApiError::internal)?;
    if !filter.admits_issue(&issue.title, &issue.author) {
        return Ok(Outcome::Filtered);
    }
    let repository = repositories::load(conn, repository_id)
        .map_err(crate::api::org::repositories::map_error)?;
    crate::api::org::run_in_transaction(conn, |conn| {
        // The delivery board of the team of the repository.
        let route = match repositories::route_task(
            conn,
            None,
            Some(repository.team_id),
            Some(&repository.slug),
        ) {
            Ok(route) => route,
            Err(repositories::RepositoryError::Database(e)) => return Err(ApiError::internal(e)),
            Err(other) => return Ok(Outcome::NotRouted(other.to_string())),
        };
        // The rule of a request, as for any member (the entry column, the
        // support lane).
        let access = match crate::api::tasks::require_task_create_capability(
            conn,
            tenant_slug,
            actor,
            &route,
            None,
            None,
            None,
        ) {
            Ok(access) => access,
            Err(e) if e.status.is_server_error() => return Err(e),
            Err(e) => return Ok(Outcome::NotRouted(e.message)),
        };
        // The key first: a second delivery waits here for the first to
        // commit, then finds the row.
        let claimed = diesel::sql_query(
            "INSERT INTO forge_issue_intake \
                 (repository_id, issue_id, number, item_id, url, author, source) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (repository_id, issue_id) DO NOTHING",
        )
        .bind::<diesel::sql_types::Uuid, _>(repository.id)
        .bind::<diesel::sql_types::Text, _>(&issue.issue_id)
        .bind::<diesel::sql_types::BigInt, _>(issue.number)
        .bind::<diesel::sql_types::Uuid, _>(Uuid::nil())
        .bind::<diesel::sql_types::Text, _>(&issue.url)
        .bind::<diesel::sql_types::Text, _>(&issue.author)
        .bind::<diesel::sql_types::Text, _>(source.as_str())
        .execute(conn)
        .map_err(ApiError::internal)?;
        if claimed == 0 {
            return Ok(Outcome::Duplicate);
        }
        let task = items::create_task(
            conn,
            items::CreateTask {
                board_id: route.board_id,
                column_id: None,
                title: &task_title(issue),
                content: &task_content(issue, repository.forge.as_str()),
                task_type: TaskType::Support,
                work_class: access.work_class(None, TaskType::Support),
                repository_id: route.repository_id,
            },
            actor,
        )
        .map_err(crate::api::map_item_error)?;
        diesel::sql_query(
            "UPDATE forge_issue_intake SET item_id = $3 \
              WHERE repository_id = $1 AND issue_id = $2",
        )
        .bind::<diesel::sql_types::Uuid, _>(repository.id)
        .bind::<diesel::sql_types::Text, _>(&issue.issue_id)
        .bind::<diesel::sql_types::Uuid, _>(task.id)
        .execute(conn)
        .map_err(ApiError::internal)?;
        kairos_db::forge::upsert_link(
            conn,
            NewItemLink {
                item_id: task.id,
                connection_id,
                kind: LinkKind::Issue,
                external_id: issue.number.to_string(),
                title: issue.title.clone(),
                url: issue.url.clone(),
                state: LinkState::Open,
                author: issue.author.clone(),
                forge_updated_at: issue.created_at,
            },
        )
        .map_err(ApiError::internal)?;
        emit_event(
            conn,
            &ThinEvent {
                event: EventKind::ItemLinksChanged,
                entity_type: "task".to_string(),
                short_code: task.short_code.clone(),
                board_id: None,
                column_id: None,
                actor,
            },
        )
        .map_err(ApiError::internal)?;
        let author = if issue.author.is_empty() {
            String::new()
        } else {
            format!(" by {}", issue.author)
        };
        diesel::insert_into(kairos_db::schema::activity_log::table)
            .values(NewActivityLogEntry {
                actor_id: actor,
                action: ActivityAction::ForgeEvent,
                entity_id: Some(repository.id),
                entity_type: Some("repository".to_string()),
                details: format!(
                    "issue {} open{author} -> {} ({})",
                    issue.number,
                    task.short_code,
                    source.as_str()
                ),
            })
            .execute(conn)
            .map_err(ApiError::internal)?;
        Ok(Outcome::Created(task.short_code))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn issue(title: &str, body: &str, author: &str) -> ForgeIssue {
        ForgeIssue {
            issue_id: "1".into(),
            number: 12,
            title: title.into(),
            body: body.into(),
            url: "https://github.com/acme/app/issues/12".into(),
            author: author.into(),
            repo_full_name: "acme/app".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn the_title_is_the_issue_title_or_its_number() {
        assert_eq!(task_title(&issue("  Crash  ", "", "robin")), "Crash");
        assert_eq!(task_title(&issue("   ", "", "robin")), "Issue #12");
        let long = "x".repeat(400);
        assert_eq!(task_title(&issue(&long, "", "robin")).chars().count(), 250);
    }

    #[test]
    fn the_content_links_the_issue_and_keeps_its_text() {
        let content = task_content(&issue("Crash", "It crashes.", "robin"), "github");
        assert!(content.starts_with(
            "Issue [#12](https://github.com/acme/app/issues/12) on github, opened by robin."
        ));
        assert!(content.contains("nothing is synced after this"));
        assert!(content.ends_with("It crashes.\n"));
        let empty = task_content(&issue("Crash", " ", ""), "gitlab");
        assert!(empty.starts_with("Issue [#12](https://github.com/acme/app/issues/12) on gitlab."));
        assert!(empty.contains("(The issue has no text.)"));
    }
}
