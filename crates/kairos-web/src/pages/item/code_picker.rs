//! A short-code field that searches (KAIROS-T-0580): the person types a
//! code or some words, and the field lists the items that match. A click
//! on a match puts its code in the field. The Relationships panel, the
//! Parent picker and the create dialog use it.
//!
//! The search waits for a pause in the typing ([`PAUSE_MS`]) and asks for
//! a few items of the types that the caller names. A code typed in full
//! needs no match: the server reads the field.

use aurora_dark::components::{Button, Stack, Text, TextInput};
use leptos::prelude::*;

use crate::auth::use_auth;
use crate::pages::search::data;

/// The pause in the typing before the field searches.
const PAUSE_MS: u64 = 250;

/// The most matches that the field lists.
const MOST_MATCHES: usize = 6;

/// The text to search for: the trimmed text of the field, or `None` when
/// it is shorter than two characters, or when the person just took a
/// match. Pure, host-tested.
pub(crate) fn search_text(text: &str, taken: bool) -> Option<String> {
    let text = text.trim();
    (!taken && text.chars().count() >= 2).then(|| text.to_string())
}

/// The matches of a search as `(short code, title)`: each group in the
/// order of the server, without `exclude` (the item of the page), at most
/// `most`. Pure, host-tested.
pub(crate) fn matches(
    groups: &data::SearchResultGroups,
    exclude: Option<&str>,
    most: usize,
) -> Vec<(String, String)> {
    [
        &groups.strategies,
        &groups.initiatives,
        &groups.tasks,
        &groups.documents,
        &groups.adrs,
    ]
    .into_iter()
    .flatten()
    .filter(|hit| Some(hit.short_code.as_str()) != exclude)
    .take(most)
    .map(|hit| (hit.short_code.clone(), hit.title.clone()))
    .collect()
}

/// The field. `value` holds the code; `types` are the entity types that
/// the search returns (`task`, `initiative`, ...).
#[component]
pub(crate) fn CodePicker(
    #[prop(into)] label: String,
    value: RwSignal<String>,
    #[prop(into)] types: Signal<&'static [&'static str]>,
    /// The item of the page: the list does not offer it.
    #[prop(optional, into)]
    exclude: Option<String>,
    #[prop(optional, into)] disabled: Signal<bool>,
) -> impl IntoView {
    let auth = use_auth();
    let exclude = StoredValue::new(exclude);
    // `taken`: the text is the code of a match that the person took.
    let taken = RwSignal::new(false);
    let query = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let text = value.get();
        let wanted = search_text(&text, taken.get());
        set_timeout(
            move || {
                if value.try_get_untracked().as_deref() == Some(text.as_str()) {
                    let _ = query.try_set(wanted);
                }
            },
            std::time::Duration::from_millis(PAUSE_MS),
        );
    });
    let found = LocalResource::new(move || {
        let q = query.get();
        let types = types.get();
        async move {
            let q = q?;
            let request = data::SearchRequest {
                q: Some(q),
                filter: Some(data::SearchFilter {
                    entity_type: Some(types.iter().map(|t| t.to_string()).collect()),
                    ..data::SearchFilter::default()
                }),
                limit: Some(MOST_MATCHES as i64 + 1),
                ..data::SearchRequest::default()
            };
            Some(data::search(auth, &request).await)
        }
    });
    let take = move |code: String| {
        taken.set(true);
        value.set(code);
        query.set(None);
    };

    view! {
        <Stack gap="xs">
            <TextInput
                label
                value
                mono=true
                disabled
                placeholder="A short code or some words"
                on_input=Callback::new(move |_| taken.set(false))
            />
            {move || match found.get().flatten() {
                None => ().into_any(),
                Some(Err(_)) => view! {
                    <Text size="xs" dimmed=true>"The search failed. Type the short code."</Text>
                }.into_any(),
                Some(Ok(response)) => {
                    let rows = exclude
                        .with_value(|exclude| matches(&response.results, exclude.as_deref(), MOST_MATCHES));
                    if rows.is_empty() {
                        return view! {
                            <Text size="xs" dimmed=true>"No item matches."</Text>
                        }.into_any();
                    }
                    view! {
                        <div class="kairos-code-picker__matches" data-testid="code-picker-matches">
                            <Stack gap="xs">
                                {rows.into_iter().map(|(code, title)| {
                                    let label = format!("{code} — {title}");
                                    view! {
                                        <Button variant="subtle" size="xs"
                                            on_click=Callback::new(move |_| take(code.clone()))>
                                            {label}
                                        </Button>
                                    }
                                }).collect_view()}
                            </Stack>
                        </div>
                    }.into_any()
                }
            }}
        </Stack>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(code: &str) -> data::Hit {
        serde_json::from_value(serde_json::json!({
            "short_code": code,
            "title": format!("Title of {code}"),
            "created_at": "2026-10-18T10:00:00Z",
        }))
        .expect("test hit")
    }

    /// The field searches after two characters, and not for the code of
    /// a match that the person just took.
    #[test]
    fn the_field_searches_for_two_characters_or_more() {
        assert_eq!(search_text("", false), None);
        assert_eq!(search_text(" a ", false), None);
        assert_eq!(search_text(" ab ", false), Some("ab".to_string()));
        assert_eq!(search_text("DEMO-T-0001", true), None);
    }

    /// The matches keep the order of the server, leave out the item of
    /// the page, and stop at the limit.
    #[test]
    fn the_matches_leave_out_the_item_of_the_page() {
        let groups = data::SearchResultGroups {
            initiatives: vec![hit("DEMO-I-0001")],
            tasks: vec![hit("DEMO-T-0001"), hit("DEMO-T-0002"), hit("DEMO-T-0003")],
            ..data::SearchResultGroups::default()
        };
        let rows = matches(&groups, Some("DEMO-T-0001"), 3);
        let codes: Vec<&str> = rows.iter().map(|(code, _)| code.as_str()).collect();
        assert_eq!(codes, ["DEMO-I-0001", "DEMO-T-0002", "DEMO-T-0003"]);
        assert_eq!(rows[0].1, "Title of DEMO-I-0001");
        assert_eq!(matches(&groups, None, 2).len(), 2);
    }
}
