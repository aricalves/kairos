//! The published site of a repository on the team page (KAIROS-T-0408): a
//! link to it, when and by whom it was published, its MD5, and a mark when
//! CI must publish it again. The site switch is on the page Admin,
//! Repositories.

use aurora_dark::components::{Anchor, Group, Pill, Text};
use aurora_dark::tokens::token;
use leptos::prelude::*;

use super::api::RepositorySite;

/// The date part of an RFC 3339 time.
fn day(at: &str) -> String {
    at.get(..10).unwrap_or(at).to_string()
}

/// One line about the site of a repository.
#[component]
pub fn RepositorySiteLine(site: RepositorySite) -> impl IntoView {
    if !site.enabled {
        return view! {
            <Text dimmed=true size="xs" attr:data-testid="repo-site">"Site: off"</Text>
        }
        .into_any();
    }
    let Some(published) = site.published else {
        return view! {
            <Text dimmed=true size="xs" attr:data-testid="repo-site">
                "Site: on, not published yet. CI publishes it with kairos pages publish."
            </Text>
        }
        .into_any();
    };
    let by = published
        .principal_name
        .map(|name| format!(" by {name}"))
        .unwrap_or_default();
    let detail = format!(
        "published {}{by} · MD5 {}",
        day(&published.published_at),
        published.md5.get(..8).unwrap_or(&published.md5)
    );
    view! {
        <Group gap="sm" wrap=true attr:data-testid="repo-site">
            // The server renders the site: a full page load, not a route of
            // the app.
            <Anchor href=site.base_path attr:rel="external">"Open the site"</Anchor>
            <Text dimmed=true size="xs">{detail}</Text>
            {site.needs_republish.then(|| view! {
                <Pill color=token::GOLD>"needs a new publish"</Pill>
            })}
        </Group>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_day_of_a_time() {
        assert_eq!(day("2026-10-13T02:14:26+00:00"), "2026-10-13");
        assert_eq!(day("short"), "short");
    }
}
