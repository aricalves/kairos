//! The HTML of the rendered docs sites (KAIROS-T-0404, KAIROS-D-0013).
//!
//! maud escapes each value; the only raw HTML is the body of an item, which
//! `kairos-markdown` renders with raw HTML escaped and unsafe URLs dropped.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use kairos_db::models::team_pages::TeamPage;
use kairos_db::site_content::{SiteItem, SiteRepository, SiteTeam};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use uuid::Uuid;

/// The paths of one tenant's sites.
#[derive(Debug, Clone)]
pub(super) struct Paths {
    pub tenant: String,
}

impl Paths {
    pub fn org(&self) -> String {
        format!("/pages/{}/", self.tenant)
    }
    pub fn team(&self, team: &str) -> String {
        format!("/pages/{}/{team}/", self.tenant)
    }
    /// The page of a document (`document`) or an ADR (`adr`), in the org site
    /// (`team` None) or a team site.
    pub fn item(&self, team: Option<&str>, kind: &str, code: &str) -> String {
        let section = if kind == "adr" { "adrs" } else { "docs" };
        match team {
            Some(team) => format!("/pages/{}/{team}/{section}/{code}/", self.tenant),
            None => format!("/pages/{}/{section}/{code}/", self.tenant),
        }
    }
    /// A team page, by the slugs of its folders and itself.
    pub fn team_page(&self, team: &str, slugs: &[&str]) -> String {
        format!("/pages/{}/{team}/{}/", self.tenant, slugs.join("/"))
    }
}

/// A crumb of the breadcrumb: its text and, but for the last, its link.
pub(super) struct Crumb {
    pub text: String,
    pub href: Option<String>,
}

fn date(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

/// The page frame: head, breadcrumb, the side list and the main part.
pub(super) fn layout(title: &str, crumbs: &[Crumb], side: Markup, main: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Kairos" }
                link rel="stylesheet" href="/pages/_/pages.css";
                script src="/pages/_/pages.js" defer {}
            }
            body.kp {
                header.kp-header {
                    nav.kp-crumbs aria-label="Breadcrumb" {
                        @for (i, crumb) in crumbs.iter().enumerate() {
                            @if i > 0 { span.kp-crumbs__sep { "›" } }
                            @match &crumb.href {
                                Some(href) => a href=(href) { (crumb.text) },
                                None => span aria-current="page" { (crumb.text) },
                            }
                        }
                    }
                }
                div.kp-body {
                    aside.kp-side { (side) }
                    main.kp-main { (main) }
                }
            }
        }
    }
}

fn status_badge(item: &SiteItem) -> Markup {
    html! {
        @if let Some(status) = &item.status {
            @if item.kind == "document" {
                @if status != "published" { span.kp-badge { (status) } }
            } @else {
                span.kp-badge { (status) }
            }
        }
    }
}

/// One line of a list: the code, the title, the badges, the last change.
fn item_line(paths: &Paths, team: Option<&str>, item: &SiteItem) -> Markup {
    html! {
        li.kp-item {
            a href=(paths.item(team, &item.kind, &item.short_code)) {
                span.kp-code { (item.short_code) }
                " " (item.title)
            }
            " " (status_badge(item))
            @if let Some(newer) = &item.superseded_by {
                " " span.kp-badge.kp-badge--old { "superseded by " (newer) }
            }
            " " span.kp-date { (date(&item.updated_at)) }
        }
    }
}

fn item_list(paths: &Paths, team: Option<&str>, items: &[&SiteItem]) -> Markup {
    html! {
        ul.kp-list {
            @for item in items { (item_line(paths, team, item)) }
        }
    }
}

/// The team pages as a tree under `parent`.
pub(super) fn page_tree(
    paths: &Paths,
    team: &str,
    pages: &[TeamPage],
    parent: Option<Uuid>,
    prefix: &[&str],
) -> Markup {
    let children: Vec<&TeamPage> = pages.iter().filter(|p| p.parent_id == parent).collect();
    html! {
        @if !children.is_empty() {
            ul.kp-list.kp-tree {
                @for page in children {
                    @let slugs: Vec<&str> =
                        prefix.iter().copied().chain([page.slug.as_str()]).collect();
                    li {
                        a href=(paths.team_page(team, &slugs)) { (page.title) }
                        (page_tree(paths, team, pages, Some(page.id), &slugs))
                    }
                }
            }
        }
    }
}

/// The content of a team site, for its landing and its side list.
pub(super) struct TeamSite {
    pub team: SiteTeam,
    pub documents: Vec<SiteItem>,
    pub adrs: Vec<SiteItem>,
    pub pages: Vec<TeamPage>,
    pub repositories: Vec<SiteRepository>,
    /// The repositories that each item impacts.
    pub impacts: HashMap<Uuid, Vec<Uuid>>,
}

impl TeamSite {
    fn impacting<'a>(&'a self, items: &'a [SiteItem], repo: Uuid) -> Vec<&'a SiteItem> {
        items
            .iter()
            .filter(|i| self.impacts.get(&i.id).is_some_and(|r| r.contains(&repo)))
            .collect()
    }
    fn unlinked<'a>(&'a self, items: &'a [SiteItem]) -> Vec<&'a SiteItem> {
        let repos: Vec<Uuid> = self.repositories.iter().map(|r| r.id).collect();
        items
            .iter()
            .filter(|i| {
                !self
                    .impacts
                    .get(&i.id)
                    .is_some_and(|r| r.iter().any(|id| repos.contains(id)))
            })
            .collect()
    }
}

/// The side list of a team site.
pub(super) fn team_side(paths: &Paths, site: &TeamSite) -> Markup {
    let team = site.team.slug.as_str();
    fn all(items: &[SiteItem]) -> Vec<&SiteItem> {
        items.iter().collect()
    }
    html! {
        a.kp-side__home href=(paths.team(team)) { (site.team.name) }
        @if !site.documents.is_empty() {
            h2 { "Documents" }
            (item_list(paths, Some(team), &all(&site.documents)))
        }
        @if !site.adrs.is_empty() {
            h2 { "Decisions" }
            (item_list(paths, Some(team), &all(&site.adrs)))
        }
        @if !site.pages.is_empty() {
            h2 { "Team pages" }
            (page_tree(paths, team, &site.pages, None, &[]))
        }
        p.kp-side__org { a href=(paths.org()) { "All teams" } }
    }
}

/// The landing of a team site.
pub(super) fn team_landing(paths: &Paths, site: &TeamSite) -> Markup {
    let team = site.team.slug.as_str();
    let main = html! {
        h1 { (site.team.name) }
        @for repo in &site.repositories {
            @let docs = site.impacting(&site.documents, repo.id);
            @let adrs = site.impacting(&site.adrs, repo.id);
            section.kp-section {
                h2 { "Repository " span.kp-code { (repo.slug) } }
                @if let Some(base) = &repo.site_base_path {
                    p { a.kp-site-link href=(base) { "Open the site of " (repo.slug) } }
                }
                @if docs.is_empty() && adrs.is_empty() {
                    p.kp-empty { "No document or decision is about this repository." }
                } @else {
                    (item_list(paths, Some(team), &docs))
                    (item_list(paths, Some(team), &adrs))
                }
            }
        }
        @let docs = site.unlinked(&site.documents);
        @if !docs.is_empty() {
            section.kp-section {
                h2 { "Other documents" }
                (item_list(paths, Some(team), &docs))
            }
        }
        @let adrs = site.unlinked(&site.adrs);
        @if !adrs.is_empty() {
            section.kp-section {
                h2 { "Decisions" }
                (item_list(paths, Some(team), &adrs))
            }
        }
        @if !site.pages.is_empty() {
            section.kp-section {
                h2 { "Team pages" }
                (page_tree(paths, team, &site.pages, None, &[]))
            }
        }
    };
    layout(
        &site.team.name,
        &[
            Crumb {
                text: paths.tenant.clone(),
                href: Some(paths.org()),
            },
            Crumb {
                text: site.team.name.clone(),
                href: None,
            },
        ],
        team_side(paths, site),
        main,
    )
}

/// The content of the org site.
pub(super) struct OrgSite {
    pub teams: Vec<SiteTeam>,
    pub documents: Vec<SiteItem>,
    pub adrs: Vec<SiteItem>,
}

/// The side list of the org site.
pub(super) fn org_side(paths: &Paths, site: &OrgSite) -> Markup {
    html! {
        a.kp-side__home href=(paths.org()) { (paths.tenant) }
        @if !site.teams.is_empty() {
            h2 { "Teams" }
            ul.kp-list {
                @for team in &site.teams {
                    li { a href=(paths.team(&team.slug)) { (team.name) } }
                }
            }
        }
        @if !site.adrs.is_empty() {
            h2 { "Decisions" }
            (item_list(paths, None, &site.adrs.iter().collect::<Vec<_>>()))
        }
        p.kp-side__org { a href="/search" { "Search in Kairos" } }
    }
}

/// The landing of the org site.
pub(super) fn org_landing(paths: &Paths, site: &OrgSite) -> Markup {
    // The documents by the strategy or initiative they support.
    let mut groups: Vec<(String, String, Vec<&SiteItem>)> = Vec::new();
    let mut other: Vec<&SiteItem> = Vec::new();
    for doc in &site.documents {
        match (&doc.supports_code, &doc.supports_title) {
            (Some(code), Some(title)) => match groups.iter_mut().find(|(c, _, _)| c == code) {
                Some((_, _, docs)) => docs.push(doc),
                None => groups.push((code.clone(), title.clone(), vec![doc])),
            },
            _ => other.push(doc),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let main = html! {
        h1 { (paths.tenant) }
        @if !site.teams.is_empty() {
            section.kp-section {
                h2 { "Teams" }
                ul.kp-list {
                    @for team in &site.teams {
                        li { a href=(paths.team(&team.slug)) { (team.name) } }
                    }
                }
            }
        }
        @for (code, title, docs) in &groups {
            section.kp-section {
                h2 {
                    a href=(format!("/items/{code}")) { span.kp-code { (code) } } " " (title)
                }
                (item_list(paths, None, docs))
            }
        }
        @if !other.is_empty() {
            section.kp-section {
                h2 { "Other documents" }
                (item_list(paths, None, &other))
            }
        }
        @if !site.adrs.is_empty() {
            section.kp-section {
                h2 { "Decisions" }
                (item_list(paths, None, &site.adrs.iter().collect::<Vec<_>>()))
            }
        }
        p { a href="/search" { "Search in Kairos" } }
    };
    layout(
        &paths.tenant,
        &[Crumb {
            text: paths.tenant.clone(),
            href: None,
        }],
        org_side(paths, site),
        main,
    )
}

/// What an item page shows above its body.
pub(super) struct ItemHead<'a> {
    pub code: &'a str,
    pub title: &'a str,
    pub kind: &'a str,
    pub status: Option<&'a str>,
    pub board_name: &'a str,
    pub updated_at: &'a DateTime<Utc>,
    /// A superseded ADR: the code and the page of the newer one.
    pub superseded_by: Option<(String, Option<String>)>,
}

/// The page of a document or an ADR, or of a team page (`head.code` empty).
pub(super) fn item_page(
    crumbs: &[Crumb],
    side: Markup,
    head: &ItemHead<'_>,
    body_html: &str,
) -> Markup {
    let main = html! {
        article.kp-article {
            @if !head.code.is_empty() {
                p.kp-meta {
                    span.kp-code { (head.code) }
                    @if let Some(status) = head.status {
                        @if head.kind == "adr" || status != "published" {
                            " " span.kp-badge { (status) }
                        }
                    }
                    " · " (head.board_name) " · " (date(head.updated_at))
                    " · " a href=(format!("/items/{}", head.code)) { "Open in Kairos" }
                }
            }
            h1 { (head.title) }
            @if let Some((newer, href)) = &head.superseded_by {
                p.kp-note {
                    "A newer decision supersedes this one: "
                    @match href {
                        Some(href) => a href=(href) { (newer) },
                        None => span.kp-code { (newer) },
                    }
                    "."
                }
            }
            div.kairos-markdown { (PreEscaped(body_html)) }
        }
    };
    layout(head.title, crumbs, side, main)
}
