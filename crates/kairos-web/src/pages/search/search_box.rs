//! The search box of the top bar (KAIROS-T-0583): Enter opens the search
//! page with the text as its query (`/search?q=`).

use aurora_dark::components::TextInput;
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use super::url::SearchState;

/// The path that the box opens: the search page with the text as `q`, or
/// the bare search page for an empty text. Pure, host-tested.
pub fn search_href(text: &str) -> String {
    let state = SearchState {
        q: text.trim().to_string(),
        ..SearchState::default()
    };
    format!("/search{}", state.to_query())
}

/// The box: a form with one text field. It has no button, so the
/// "Search" button of the search page stays the only one with that name.
#[component]
pub fn ShellSearchBox() -> impl IntoView {
    let text = RwSignal::new(String::new());
    let navigate = use_navigate();
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        navigate(&search_href(&text.get_untracked()), Default::default());
        text.set(String::new());
    };
    view! {
        <form role="search" data-testid="shell-search" on:submit=submit>
            <TextInput
                value=text
                input_type="search"
                aria_label="Search the work"
                placeholder="Search the work"
            />
        </form>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The box opens the page with the text as `q`, with percent codes.
    #[test]
    fn the_box_opens_the_search_page_with_the_query() {
        assert_eq!(search_href("magic link"), "/search?q=magic%20link");
        assert_eq!(search_href("  auth&billing "), "/search?q=auth%26billing");
        assert_eq!(search_href("   "), "/search");
    }
}
