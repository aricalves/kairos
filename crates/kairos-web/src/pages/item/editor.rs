//! Content editing for the item detail page (KAIROS-T-0041): a thin
//! wrapper over the generalized [`crate::pages::editor::MarkdownEditor`]
//! (extracted in KAIROS-T-0086) that binds the save to the item family's
//! versioned content PATCH. All edit/preview, toolbar, and 409 merge
//! behavior lives in the shared component.

use std::rc::Rc;

use leptos::prelude::*;

use super::api::{self, Family};
use crate::auth::use_auth;
use crate::pages::editor::{MarkdownEditor, Saver, draft_key};

/// The editable content panel. Recreated (with fresh server state) when
/// the page refetches after a successful save — the parent owns that
/// refetch via `on_saved`.
#[component]
pub fn ContentEditor(
    family: Family,
    #[prop(into)] code: String,
    #[prop(into)] initial_title: String,
    #[prop(into)] initial_content: String,
    initial_version: i32,
    /// The editor renders the content instead of offering an edit that
    /// the server refuses: the item is archived (KAIROS-T-0164, ADR-20),
    /// or the person may not edit it (KAIROS-T-0581, [`super::access`]).
    #[prop(into)]
    read_only: Signal<bool>,
    /// Why the content is read-only; empty while the page does not know.
    #[prop(into)]
    read_only_reason: Signal<String>,
    /// Fired after a successful save; the parent refetches (server state
    /// is the source of truth) and posts the page-level notice.
    on_saved: Callback<i32>,
) -> impl IntoView {
    let auth = use_auth();
    // KAIROS-T-0581: the draft is kept outside this subtree, so a reload
    // of the page (a panel action, a save elsewhere) does not lose it.
    let key = draft_key(&code);
    let saver: Saver = Rc::new(move |title, content, version| {
        let code = code.clone();
        Box::pin(async move {
            api::update_content(auth, family, &code, &title, &content, version)
                .await
                .map(|updated| updated.version)
        })
    });
    view! {
        <MarkdownEditor
            initial_title
            initial_content
            initial_version
            read_only
            read_only_reason
            draft_key=key
            on_saved
            saver
            // KAIROS-T-0323: a ticket is read more than it is changed.
            start_in_preview=true
        />
    }
}
