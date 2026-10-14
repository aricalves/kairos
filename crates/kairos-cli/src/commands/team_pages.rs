//! `kairos teams pages` — the team page of a team (KAIROS-T-0434): its
//! charter and the tree of folders and pages that the team grows. A thin
//! veneer over `/api/teams/{id}/pages`. A team is a slug or a UUID. A page
//! is a path of slugs from the root (`documentation/runbooks`, `charter`)
//! or a UUID.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use kairos_client::types_team_pages::{CreateTeamPageRequest, TeamPage, UpdateTeamPageRequest};

use super::teams::team_id;
use crate::commands::entities::require_confirm;
use crate::context::{Common, client, print_json};
use crate::error::CliError;

/// Operations on the team page of a team.
#[derive(clap::Subcommand, Debug)]
pub enum TeamPagesCommand {
    /// Show the tree of the team page: the charter first, then the folders
    /// and pages in position order
    List {
        /// Team slug or UUID
        team: String,
        #[command(flatten)]
        common: Common,
    },
    /// Show one page or folder in full: its content and its version
    Show {
        /// Team slug or UUID
        team: String,
        /// Path of slugs from the root, or the page UUID
        page: String,
        #[command(flatten)]
        common: Common,
    },
    /// Create a page or a folder. You must be a member of the team or an
    /// organization admin
    Create {
        /// Team slug or UUID
        team: String,
        /// page or folder. A folder holds folders and pages
        #[arg(long)]
        kind: String,
        /// The title
        #[arg(long)]
        title: String,
        /// The slug (the default comes from the title)
        #[arg(long)]
        slug: Option<String>,
        /// The folder: a path of slugs or a UUID (the default is the root)
        #[arg(long)]
        parent: Option<String>,
        /// The markdown content of a page
        #[arg(long, conflicts_with = "content_file")]
        content: Option<String>,
        /// Read the content from a file (`-` reads stdin)
        #[arg(long, value_name = "FILE")]
        content_file: Option<PathBuf>,
        /// The position among the siblings (the default is after the last)
        #[arg(long)]
        position: Option<i32>,
        #[command(flatten)]
        common: Common,
    },
    /// Replace the content of a page (and optionally its title)
    Edit {
        /// Team slug or UUID
        team: String,
        /// Path of slugs from the root, or the page UUID
        page: String,
        /// A new title
        #[arg(long)]
        title: Option<String>,
        /// The new markdown content, full replacement
        #[arg(long, conflicts_with = "content_file")]
        content: Option<String>,
        /// Read the new content from a file (`-` reads stdin)
        #[arg(long, value_name = "FILE")]
        content_file: Option<PathBuf>,
        /// Base the edit on this version instead of the current one (a
        /// stale value is refused with 409 CONFLICT)
        #[arg(long)]
        version: Option<i32>,
        #[command(flatten)]
        common: Common,
    },
    /// Move, reorder or rename a page or a folder. The charter cannot move
    Move {
        /// Team slug or UUID
        team: String,
        /// Path of slugs from the root, or the page UUID
        page: String,
        /// The new folder: a path of slugs or a UUID
        #[arg(long, conflicts_with = "root")]
        parent: Option<String>,
        /// Move it to the root
        #[arg(long)]
        root: bool,
        /// A new slug
        #[arg(long)]
        slug: Option<String>,
        /// A new position among the siblings
        #[arg(long)]
        position: Option<i32>,
        #[command(flatten)]
        common: Common,
    },
    /// Archive a page, or a folder with everything in it (requires
    /// --confirm). The charter
    /// cannot be archived
    Delete {
        /// Team slug or UUID
        team: String,
        /// Path of slugs from the root, or the page UUID
        page: String,
        /// Actually archive
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

/// Whether a reference has the shape of a UUID (8-4-4-4-12 hex digits). A
/// team slug never does: a slug starts with a letter and has no 36-character
/// form of this shape in practice.
pub(crate) fn looks_like_uuid(reference: &str) -> bool {
    reference.len() == 36
        && reference.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The path of slugs of a page, from the root.
fn page_path(pages: &[TeamPage], page: &TeamPage) -> String {
    let mut slugs = vec![page.slug.clone()];
    let mut parent = page.parent_id.clone();
    for _ in 0..64 {
        let Some(id) = parent else { break };
        let Some(up) = pages.iter().find(|p| p.id == id) else {
            break;
        };
        slugs.push(up.slug.clone());
        parent = up.parent_id.clone();
    }
    slugs.reverse();
    slugs.join("/")
}

/// A page by its UUID or its path of slugs from the root.
fn page_by_ref<'a>(pages: &'a [TeamPage], reference: &str) -> Result<&'a TeamPage, CliError> {
    if let Some(page) = pages.iter().find(|p| p.id == reference) {
        return Ok(page);
    }
    let mut parent: Option<String> = None;
    let mut found = None;
    for slug in reference.trim_matches('/').split('/') {
        match pages
            .iter()
            .find(|p| p.parent_id == parent && p.slug == slug)
        {
            Some(next) => {
                parent = Some(next.id.clone());
                found = Some(next);
            }
            None => {
                found = None;
                break;
            }
        }
    }
    found.ok_or_else(|| {
        CliError::Failure(format!(
            "The team page has no page {reference:?}. Use a path of slugs from the root \
             (for example charter) or a page UUID. `kairos teams pages list` shows the tree."
        ))
    })
}

/// A slug from a title: lower case, each run of other characters one hyphen.
fn slug_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

fn read_content(
    content: Option<String>,
    content_file: Option<PathBuf>,
) -> Result<Option<String>, CliError> {
    match (content, content_file) {
        (Some(content), _) => Ok(Some(content)),
        (None, Some(path)) if path == Path::new("-") => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text).map_err(|err| {
                CliError::Failure(format!("The CLI cannot read standard input: {err}."))
            })?;
            Ok(Some(text))
        }
        (None, Some(path)) => Ok(Some(std::fs::read_to_string(&path).map_err(|err| {
            CliError::Failure(format!("The CLI cannot read {}: {err}.", path.display()))
        })?)),
        (None, None) => Ok(None),
    }
}

/// The tree, one line for each node, indented by depth.
fn render_tree(pages: &[TeamPage]) -> String {
    fn walk(pages: &[TeamPage], parent: Option<&String>, depth: usize, out: &mut String) {
        let mut level: Vec<_> = pages
            .iter()
            .filter(|p| p.parent_id.as_ref() == parent)
            .collect();
        level.sort_by(|a, b| {
            b.is_protected
                .cmp(&a.is_protected)
                .then(a.position.cmp(&b.position))
                .then(a.title.cmp(&b.title))
        });
        for page in level {
            let marker = if page.is_protected {
                " [charter]"
            } else if page.kind == "folder" {
                " [folder]"
            } else {
                ""
            };
            out.push_str(&format!(
                "{}{} — {}{marker}  v{}  {}\n",
                "  ".repeat(depth),
                page.slug,
                page.title,
                page.version,
                page.id
            ));
            if page.kind == "folder" && depth < 64 {
                walk(pages, Some(&page.id), depth + 1, out);
            }
        }
    }
    let mut out = String::new();
    walk(pages, None, 0, &mut out);
    out
}

impl TeamPagesCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { team, common } => {
                let client = client(&common)?;
                let team_id = team_id(&client, &team).await?;
                let pages = client.list_team_pages(&team_id).await?;
                if common.json {
                    return print_json(&pages);
                }
                print!("{}", render_tree(&pages));
                Ok(())
            }
            Self::Show { team, page, common } => {
                let client = client(&common)?;
                let team_id = team_id(&client, &team).await?;
                let pages = client.list_team_pages(&team_id).await?;
                let page = page_by_ref(&pages, &page)?;
                if common.json {
                    return print_json(page);
                }
                println!("{} ({}, v{})", page.title, page.kind, page.version);
                println!("path: {}", page_path(&pages, page));
                println!("id:   {}", page.id);
                if !page.content.is_empty() {
                    println!("\n{}", page.content);
                }
                Ok(())
            }
            Self::Create {
                team,
                kind,
                title,
                slug,
                parent,
                content,
                content_file,
                position,
                common,
            } => {
                let client = client(&common)?;
                let team_id = team_id(&client, &team).await?;
                let pages = client.list_team_pages(&team_id).await?;
                let parent_id = match &parent {
                    Some(reference) => Some(page_by_ref(&pages, reference)?.id.clone()),
                    None => None,
                };
                let position = position.unwrap_or_else(|| {
                    pages
                        .iter()
                        .filter(|p| p.parent_id == parent_id)
                        .map(|p| p.position + 1)
                        .max()
                        .unwrap_or(0)
                });
                let request = CreateTeamPageRequest {
                    parent_id,
                    kind,
                    slug: slug.unwrap_or_else(|| slug_from_title(&title)),
                    title,
                    content: read_content(content, content_file)?.unwrap_or_default(),
                    position,
                };
                let created = client.create_team_page(&team_id, &request).await?;
                if common.json {
                    return print_json(&created);
                }
                let mut all = pages;
                all.push(created.clone());
                println!(
                    "Kairos created the {} {} at {} ({}).",
                    created.kind,
                    created.title,
                    page_path(&all, &created),
                    created.id
                );
                Ok(())
            }
            Self::Edit {
                team,
                page,
                title,
                content,
                content_file,
                version,
                common,
            } => {
                let client = client(&common)?;
                let team_id = team_id(&client, &team).await?;
                let pages = client.list_team_pages(&team_id).await?;
                let current = page_by_ref(&pages, &page)?;
                let content = read_content(content, content_file)?;
                if title.is_none() && content.is_none() {
                    return Err(CliError::Failure(
                        "The command has no change. Use --title, --content or --content-file."
                            .to_string(),
                    ));
                }
                let request = UpdateTeamPageRequest {
                    title,
                    content: Some(content.unwrap_or_else(|| current.content.clone())),
                    version: Some(version.unwrap_or(current.version)),
                    ..Default::default()
                };
                let updated = client
                    .update_team_page(&team_id, &current.id, &request)
                    .await?;
                if common.json {
                    return print_json(&updated);
                }
                println!(
                    "Kairos saved {} ({}): version {}.",
                    updated.title, updated.id, updated.version
                );
                Ok(())
            }
            Self::Move {
                team,
                page,
                parent,
                root,
                slug,
                position,
                common,
            } => {
                if parent.is_none() && !root && slug.is_none() && position.is_none() {
                    return Err(CliError::Failure(
                        "The command has no change. Use --parent, --root, --slug or --position."
                            .to_string(),
                    ));
                }
                let client = client(&common)?;
                let team_id = team_id(&client, &team).await?;
                let pages = client.list_team_pages(&team_id).await?;
                let current = page_by_ref(&pages, &page)?;
                let parent_id = match &parent {
                    Some(reference) => Some(page_by_ref(&pages, reference)?.id.clone()),
                    None => None,
                };
                let request = UpdateTeamPageRequest {
                    slug,
                    parent_id,
                    move_to_root: root,
                    position,
                    ..Default::default()
                };
                let updated = client
                    .update_team_page(&team_id, &current.id, &request)
                    .await?;
                if common.json {
                    return print_json(&updated);
                }
                let all = client.list_team_pages(&team_id).await?;
                println!(
                    "Kairos moved {} to {} (position {}).",
                    updated.title,
                    page_path(&all, &updated),
                    updated.position
                );
                Ok(())
            }
            Self::Delete {
                team,
                page,
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the page {page}"))?;
                let client = client(&common)?;
                let team_id = team_id(&client, &team).await?;
                let pages = client.list_team_pages(&team_id).await?;
                let current = page_by_ref(&pages, &page)?;
                let path = page_path(&pages, current);
                let response = client.delete_team_page(&team_id, &current.id).await?;
                if common.json {
                    return print_json(&response);
                }
                println!("Kairos archived {path} ({}).", current.id);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, slug: &str) -> TeamPage {
        TeamPage {
            id: id.into(),
            team_id: "t".into(),
            parent_id: parent.map(Into::into),
            kind: "folder".into(),
            slug: slug.into(),
            title: slug.into(),
            content: String::new(),
            position: 0,
            is_protected: false,
            version: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn a_page_is_found_by_path_or_id() {
        let pages = vec![
            node("1", None, "documentation"),
            node("2", Some("1"), "runbooks"),
            node("3", None, "runbooks"),
        ];
        assert_eq!(
            page_by_ref(&pages, "documentation/runbooks").unwrap().id,
            "2"
        );
        assert_eq!(page_by_ref(&pages, "runbooks").unwrap().id, "3");
        assert_eq!(page_by_ref(&pages, "2").unwrap().id, "2");
        assert!(page_by_ref(&pages, "documentation/nothing").is_err());
        assert_eq!(page_path(&pages, &pages[1]), "documentation/runbooks");
    }

    #[test]
    fn a_uuid_is_told_from_a_slug() {
        assert!(looks_like_uuid("762e994c-547e-4fd7-97dc-e4d3b3eb7dde"));
        assert!(!looks_like_uuid("platform"));
        assert!(!looks_like_uuid("762e994c-547e-4fd7-97dc-e4d3b3eb7ddz"));
    }

    #[test]
    fn the_slug_comes_from_the_title() {
        assert_eq!(slug_from_title("How-to Guides"), "how-to-guides");
        assert_eq!(slug_from_title("  Ways of working!"), "ways-of-working");
    }
}
