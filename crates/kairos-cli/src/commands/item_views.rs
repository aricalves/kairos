//! Three read-only verbs of each item noun (KAIROS-T-0592):
//!
//! - `<noun> diff <CODE> <VERSION> [<OTHER>]` compares two versions of the
//!   item (`GET /api/{family}/{code}/history?version=N`): the title, and a
//!   unified diff of the content. With one version, the CLI compares it
//!   with the version before it. The versions are arguments, not `--from`
//!   and `--to`: `--to` names a column or a person (KAIROS-A-0057).
//! - `<noun> links <CODE>` lists the branches, pull requests and issues of
//!   the forge that link to the item (`GET /api/{family}/{code}/links`).
//! - `<noun> archive <CODE> --preview` shows what an archive takes, and
//!   archives nothing (`GET /api/{family}/{code}/cascade-preview`).

use clap::Args;
use kairos_client::types::CascadePreviewResponse;
use kairos_client::types_forge::ItemLink;
use kairos_client::types_meta::HistorySnapshot;

use crate::context::{Common, print_json};
use crate::error::CliError;
use crate::listing::print_whole;
use crate::table::Table;

/// Arguments of `<noun> diff`.
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// The item's short code (e.g. ACME-T-0001)
    pub short_code: String,
    /// A version of the item (`<noun> history` lists them). Alone, the CLI
    /// compares it with the version before it
    pub version: i32,
    /// A second version: the CLI compares VERSION with OTHER
    pub other: Option<i32>,
    #[command(flatten)]
    pub common: Common,
}

impl DiffArgs {
    /// The two versions to compare: the old one (none before version 1)
    /// and the new one.
    pub fn versions(&self) -> Result<(Option<i32>, i32), CliError> {
        versions(self.version, self.other)
    }
}

/// [`DiffArgs::versions`]. Pure.
fn versions(version: i32, other: Option<i32>) -> Result<(Option<i32>, i32), CliError> {
    for value in std::iter::once(version).chain(other) {
        if value < 1 {
            return Err(CliError::Failure(format!(
                "The version {value} is not a version. The first version is 1."
            )));
        }
    }
    Ok(match other {
        Some(other) => (Some(version), other),
        None if version == 1 => (None, 1),
        None => (Some(version - 1), version),
    })
}

/// `<noun> diff` output. `--json` prints the two snapshots and the diff.
pub fn emit_diff(
    common: &Common,
    short_code: &str,
    old: Option<&HistorySnapshot>,
    new: &HistorySnapshot,
) -> Result<(), CliError> {
    if common.json {
        return print_json(&serde_json::json!({
            "short_code": short_code,
            "from": old,
            "to": new,
            "diff": content_diff(old.map_or("", |o| o.content.as_str()), &new.content),
        }));
    }
    print!("{}", diff_text(short_code, old, new));
    Ok(())
}

/// The unified diff of two contents, with 3 lines of context. Empty when
/// the contents are the same. Pure.
fn content_diff(old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

/// The text of [`emit_diff`]. Pure.
fn diff_text(short_code: &str, old: Option<&HistorySnapshot>, new: &HistorySnapshot) -> String {
    let head = |snapshot: &HistorySnapshot| {
        format!(
            "{short_code} v{} ({})",
            snapshot.version, snapshot.edited_at
        )
    };
    let mut out = match old {
        Some(old) => format!("--- {}\n+++ {}\n", head(old), head(new)),
        None => format!(
            "--- {short_code} (no version before v1)\n+++ {}\n",
            head(new)
        ),
    };
    let old_title = old.map_or("", |o| o.title.as_str());
    if old_title != new.title {
        out.push_str(&format!("title: {old_title:?} -> {:?}\n", new.title));
    }
    let diff = content_diff(old.map_or("", |o| o.content.as_str()), &new.content);
    if diff.is_empty() {
        out.push_str("The content did not change.\n");
    } else {
        out.push_str(&diff);
        if !diff.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// `<noun> links` output. `--json` prints the list envelope.
pub fn emit_links(common: &Common, short_code: &str, links: &[ItemLink]) -> Result<(), CliError> {
    if common.json {
        return print_whole(links);
    }
    print!("{}", links_text(short_code, links));
    Ok(())
}

/// The text of [`emit_links`]. Pure.
fn links_text(short_code: &str, links: &[ItemLink]) -> String {
    if links.is_empty() {
        return format!("No branch, pull request or issue of a forge links to {short_code}.\n");
    }
    let mut table = Table::new(&["KIND", "REPOSITORY", "REF", "STATE", "TITLE", "URL"]);
    for link in links {
        table.row(vec![
            link.kind.clone(),
            link.repo_full_name.clone(),
            link.external_id.clone(),
            link.state.clone(),
            link.title.clone(),
            link.url.clone(),
        ]);
    }
    format!("{}total: {}\n", table.render(), links.len())
}

/// `<noun> archive --preview` output.
pub fn emit_preview(common: &Common, preview: &CascadePreviewResponse) -> Result<(), CliError> {
    if common.json {
        return print_json(preview);
    }
    print!("{}", preview_text(preview));
    Ok(())
}

/// The text of [`emit_preview`]. Pure.
fn preview_text(preview: &CascadePreviewResponse) -> String {
    let mut out = match preview.cascade_count {
        0 => format!(
            "An archive of {} takes no other item. Kairos archived nothing.\n",
            preview.short_code
        ),
        count => format!(
            "An archive of {} also takes {count} descendant{}: {}. Kairos archived nothing.\n",
            preview.short_code,
            if count == 1 { "" } else { "s" },
            preview.cascaded_short_codes.join(", ")
        ),
    };
    if !preview.not_reached.is_empty() {
        let count = preview.not_reached.len();
        out.push_str(&format!(
            "  The archive cannot reach {count} item{}. They stay live and keep their parent.\n",
            if count == 1 { "" } else { "s" }
        ));
        for item in &preview.not_reached {
            out.push_str(&format!(
                "    - {}: {}\n",
                item.short_code,
                crate::commands::entities::not_reached_reason(item)
            ));
        }
    }
    out.push_str("To archive, run the command again with --confirm in place of --preview.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(version: i32, title: &str, content: &str) -> HistorySnapshot {
        HistorySnapshot {
            version,
            title: title.to_string(),
            content: content.to_string(),
            edited_by: "u-1".to_string(),
            edited_at: format!("2026-10-1{version}T10:00:00Z"),
            agent_key_id: None,
        }
    }

    #[test]
    fn one_version_is_compared_with_the_version_before_it() {
        assert_eq!(versions(3, None).expect("valid"), (Some(2), 3));
        assert_eq!(versions(1, None).expect("valid"), (None, 1));
        assert_eq!(versions(2, Some(5)).expect("valid"), (Some(2), 5));
        let err = versions(0, None).expect_err("no version 0");
        assert!(err.to_string().contains("first version is 1"), "{err}");
        assert!(versions(2, Some(-1)).is_err());
    }

    #[test]
    fn the_diff_names_the_versions_the_title_and_the_changed_lines() {
        let old = snapshot(1, "Old", "one\ntwo\nthree\n");
        let new = snapshot(2, "New", "one\n2\nthree\n");
        let text = diff_text("ACME-T-0001", Some(&old), &new);
        assert!(
            text.starts_with(
                "--- ACME-T-0001 v1 (2026-10-11T10:00:00Z)\n\
                 +++ ACME-T-0001 v2 (2026-10-12T10:00:00Z)\n\
                 title: \"Old\" -> \"New\"\n@@ -1,3 +1,3 @@\n"
            ),
            "{text}"
        );
        assert!(text.contains("\n-two\n+2\n"), "{text}");

        let same = snapshot(3, "New", "one\n2\nthree\n");
        let text = diff_text("ACME-T-0001", Some(&new), &same);
        assert!(text.ends_with("The content did not change.\n"), "{text}");
        assert!(!text.contains("title:"), "{text}");

        let first = diff_text("ACME-T-0001", None, &old);
        assert!(
            first.starts_with("--- ACME-T-0001 (no version before v1)\n"),
            "{first}"
        );
        assert!(first.contains("+one\n+two\n+three\n"), "{first}");
    }

    #[test]
    fn the_links_are_a_table_or_one_line() {
        assert_eq!(
            links_text("ACME-T-0001", &[]),
            "No branch, pull request or issue of a forge links to ACME-T-0001.\n"
        );
        let link = ItemLink {
            id: "l-1".into(),
            item_id: "i-1".into(),
            kind: "pull_request".into(),
            external_id: "12".into(),
            title: "ACME-T-0001: fix".into(),
            url: "https://github.com/acme/api/pull/12".into(),
            state: "open".into(),
            author: "robin".into(),
            forge: "github".into(),
            repo_full_name: "acme/api".into(),
            forge_updated_at: "2026-10-18T00:00:00Z".into(),
        };
        let text = links_text("ACME-T-0001", &[link]);
        assert!(text.starts_with("KIND"), "{text}");
        assert!(
            text.contains("pull_request  acme/api    12   open   ACME-T-0001: fix"),
            "{text}"
        );
        assert!(text.ends_with("total: 1\n"), "{text}");
    }

    #[test]
    fn the_preview_names_what_the_archive_takes_and_archives_nothing() {
        let preview = CascadePreviewResponse {
            short_code: "ACME-I-0001".into(),
            cascade_count: 2,
            cascaded_short_codes: vec!["ACME-T-0001".into(), "ACME-T-0002".into()],
            not_reached: vec![kairos_client::types::NotReached {
                short_code: "WEB-T-0003".into(),
                required_capability: Some("manage_tasks".into()),
                board_id: Some("b-2".into()),
                below: None,
            }],
        };
        let text = preview_text(&preview);
        assert!(
            text.starts_with(
                "An archive of ACME-I-0001 also takes 2 descendants: ACME-T-0001, ACME-T-0002. \
                 Kairos archived nothing.\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("    - WEB-T-0003: You need manage_tasks on the board b-2.\n"),
            "{text}"
        );
        assert!(text.ends_with("in place of --preview.\n"), "{text}");
        let alone = CascadePreviewResponse {
            short_code: "ACME-T-0009".into(),
            cascade_count: 0,
            cascaded_short_codes: vec![],
            not_reached: vec![],
        };
        assert!(
            preview_text(&alone).starts_with("An archive of ACME-T-0009 takes no other item."),
            "{}",
            preview_text(&alone)
        );
    }
}
