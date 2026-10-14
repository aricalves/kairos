//! The tool profiles of `/mcp` (KAIROS-T-0454, Dylan 2026-10-14).
//!
//! `tools/list` costs an agent its full text in each session. Most sessions
//! work items and never administer the organization, so the default
//! profile lists the tools of the daily work. `?profile=admin` lists the
//! administration tools, and `?profile=all` lists each tool.
//!
//! Only the list changes: a tool of each profile can be called under each
//! profile, so a caller that knows a name does not break.

use axum::http::request::Parts;
use rmcp::model::Tool;

/// The tools of the daily work: the default profile. Picked from the calls
/// of the agents (2026-10-14): these make nearly all of them.
pub(super) const WORK: [&str; 37] = [
    "whoami",
    "my_boards",
    "board_items",
    "search",
    "get_item",
    "get_history",
    "related_work",
    "create_item",
    "edit_item",
    "update_item",
    "transition_item",
    "move_item",
    "link_items",
    "unlink_items",
    "propose_edge",
    "append_note",
    "set_metadata",
    "set_repository",
    "set_work_class",
    "set_team",
    "clear_team",
    "cancel_item",
    "delete_item",
    "restore_item",
    "list_repositories",
    "get_repository",
    // The bootstrap skill adds the repository of a checkout and sets how
    // to work in it.
    "add_repository",
    "update_repository",
    // The implement and triage skills: claims, the notifications, the
    // scales and templates, and the housekeeping of a board.
    "claim_item",
    "hand_off_item",
    "release_item",
    "get_events",
    "mark_events_read",
    "list_metadata_definitions",
    "list_templates",
    "archive_completed",
    // A board session reads the charter of a team: its scope, and how a
    // request reaches it. The four writes of team pages stay admin.
    "team_pages",
];

/// A profile of `tools/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    /// [`WORK`]: the default.
    Work,
    /// Each tool that is not in [`WORK`].
    Admin,
    /// Each tool.
    All,
}

impl Profile {
    /// The profile of the request: the query `profile` of the URL of
    /// `/mcp`. No query is [`Profile::Work`]. An unknown value is an error
    /// that names it.
    pub(super) fn of(parts: Option<&Parts>) -> Result<Self, String> {
        let query = parts.and_then(|parts| parts.uri.query()).unwrap_or("");
        let value = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == "profile")
            .map(|(_, value)| value);
        match value {
            None | Some("work") => Ok(Self::Work),
            Some("admin") => Ok(Self::Admin),
            Some("all") => Ok(Self::All),
            Some(other) => Err(format!(
                "The profile {other:?} is not known. The profiles are work (the default), \
                 admin and all: /mcp?profile=admin."
            )),
        }
    }

    /// The tools of the profile, from each tool.
    pub(super) fn filter(self, tools: Vec<Tool>) -> Vec<Tool> {
        tools
            .into_iter()
            .filter(|tool| {
                let work = WORK.contains(&tool.name.as_ref());
                match self {
                    Self::Work => work,
                    Self::Admin => !work,
                    Self::All => true,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(uri: &str) -> Parts {
        axum::http::Request::builder()
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[test]
    fn the_query_chooses_the_profile() {
        assert_eq!(Profile::of(None), Ok(Profile::Work));
        assert_eq!(Profile::of(Some(&parts("/mcp"))), Ok(Profile::Work));
        assert_eq!(
            Profile::of(Some(&parts("/mcp?profile=admin"))),
            Ok(Profile::Admin)
        );
        assert_eq!(
            Profile::of(Some(&parts("/mcp?x=1&profile=all"))),
            Ok(Profile::All)
        );
        assert!(
            Profile::of(Some(&parts("/mcp?profile=every")))
                .unwrap_err()
                .contains("every")
        );
    }
}
