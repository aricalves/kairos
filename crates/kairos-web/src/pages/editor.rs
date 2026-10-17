//! The generalized markdown editor (KAIROS-T-0086): title + markdown
//! edit/preview with a syntax toolbar, the A-0004 versioned save, and the
//! 409 merge UI — extracted from the item detail's `ContentEditor`
//! (KAIROS-T-0041) behind a `Saver` callback so team pages (and any
//! future versioned content) reuse the exact same machinery instead of
//! forking the merge dialog.
//!
//! Save carries the version the edit was based on. A 409 opens the merge
//! dialog — server-current vs. yours side by side — with three ways out,
//! all of which carry the *new* version forward:
//!
//! - **Keep mine**: retry the save immediately, based on the server's
//!   current version (overwrites their text with yours).
//! - **Take theirs**: load the server-current title/content into the
//!   editor and drop your draft.
//! - **Merge manually**: keep your draft, rebase onto the server version,
//!   and pin the server copy below the editor for reference until the
//!   next successful save.
//!
//! A draft survives a reload of the page (KAIROS-T-0581). With a
//! `draft_key`, the dirty draft (title, content, and the version it is
//! based on) is in `sessionStorage`; the next mount restores it. The
//! restored draft keeps its old base version, so a save after a change on
//! the server opens the merge dialog and overwrites nothing. A save, Take
//! theirs and Discard remove it; a logout removes each draft. While the
//! draft is dirty, a `beforeunload` guard asks before the tab closes.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use aurora_dark::components::{
    ActionIcon, Alert, Button, Group, Panel, Pill, SegmentedControl, SimpleGrid, Text, TextInput,
};
use aurora_dark::frame::Modal;
use aurora_dark::tokens::token;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use super::item::api::{CurrentVersion, SaveError};
use super::item::markdown;

/// One save attempt: `(title, content, based-on version)` → the new
/// version, or the typed failure the merge UI understands.
pub type SaveFuture = Pin<Box<dyn Future<Output = Result<i32, SaveError>>>>;
/// The domain-specific save call (item content PATCH, team-page PATCH…).
pub type Saver = Rc<dyn Fn(String, String, i32) -> SaveFuture>;

/// The toolbar's insertions: `(label, tooltip, prefix, suffix,
/// placeholder-when-nothing-selected)`.
const TOOLBAR: &[(&str, &str, &str, &str, &str)] = &[
    ("H", "Heading", "## ", "", "Heading"),
    ("B", "Bold", "**", "**", "bold"),
    ("I", "Italic", "*", "*", "italic"),
    ("•", "List item", "- ", "", "item"),
    ("🔗", "Link", "[", "](url)", "text"),
    ("`", "Code", "`", "`", "code"),
];

/// The first mode of the editor (KAIROS-T-0323). A restored draft opens
/// in Edit (KAIROS-T-0581): the person was typing.
fn initial_mode(start_in_preview: bool, restored: bool) -> &'static str {
    if start_in_preview && !restored {
        "Preview"
    } else {
        "Edit"
    }
}

/// The `sessionStorage` key prefix of each draft.
const DRAFT_PREFIX: &str = "kairos-draft:";

/// The `sessionStorage` key of the draft of one item.
pub(crate) fn draft_key(short_code: &str) -> String {
    format!("{DRAFT_PREFIX}{short_code}")
}

/// One stored draft: what the person typed, and the version it is based on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Draft {
    title: String,
    content: String,
    version: i32,
}

/// The draft to restore on mount: the stored one, when it reads and it
/// differs from the server copy. Pure, host-tested.
fn draft_to_restore(
    stored: Option<&str>,
    server_title: &str,
    server_content: &str,
) -> Option<Draft> {
    let draft: Draft = serde_json::from_str(stored?).ok()?;
    (draft.title != server_title || draft.content != server_content).then_some(draft)
}

/// What to keep after a change: the JSON of the draft while it differs
/// from its baseline, `None` (remove) when it does not. Pure, host-tested.
fn draft_to_store(
    title: &str,
    content: &str,
    orig_title: &str,
    orig_content: &str,
    version: i32,
) -> Option<String> {
    if title == orig_title && content == orig_content {
        return None;
    }
    serde_json::to_string(&Draft {
        title: title.to_string(),
        content: content.to_string(),
        version,
    })
    .ok()
}

fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok().flatten()
}

fn read_draft(key: &str) -> Option<String> {
    session_storage()?.get_item(key).ok().flatten()
}

/// Write (`Some`) or remove (`None`) one draft. Best-effort: a blocked
/// storage only means that a reload loses the draft, as before.
fn write_draft(key: &str, value: Option<&str>) {
    let Some(storage) = session_storage() else {
        return;
    };
    let _ = match value {
        Some(value) => storage.set_item(key, value),
        None => storage.remove_item(key),
    };
}

/// Remove each stored draft (a logout): the next person in this tab must
/// not see them.
pub(crate) fn clear_drafts() {
    let Some(storage) = session_storage() else {
        return;
    };
    let count = storage.length().unwrap_or(0);
    let keys: Vec<String> = (0..count)
        .filter_map(|i| storage.key(i).ok().flatten())
        .filter(|key| key.starts_with(DRAFT_PREFIX))
        .collect();
    for key in keys {
        let _ = storage.remove_item(&key);
    }
}

#[component]
pub fn MarkdownEditor(
    #[prop(into)] initial_title: String,
    #[prop(into)] initial_content: String,
    initial_version: i32,
    /// Render the content instead of offering to edit it: no textarea, no
    /// toolbar, Save disabled (KAIROS-T-0164 — an archived item's content
    /// PATCH resolves live-only, and an editor whose Save 404s reads as a
    /// bug rather than as a rule). A signal since KAIROS-T-0581: the right
    /// to edit is known when `whoami` resolves.
    #[prop(optional, into)]
    read_only: Signal<bool>,
    /// Why editing is off — shown next to the disabled Save, because a
    /// disabled control with no reason reads as a broken page.
    #[prop(optional, into)]
    read_only_reason: Signal<String>,
    /// Keep the dirty draft in `sessionStorage` under this key
    /// (KAIROS-T-0581, [`draft_key`]). `None` keeps it in memory only.
    #[prop(optional, into)]
    draft_key: Option<String>,
    /// Fired after a successful save; the parent refetches (server state
    /// is the source of truth) and posts the page-level notice.
    on_saved: Callback<i32>,
    /// The domain's versioned save call.
    saver: Saver,
    /// Open in Preview instead of Edit (KAIROS-T-0323): a page that people
    /// read more than they change, such as the page of an item.
    #[prop(optional)]
    start_in_preview: bool,
) -> impl IntoView {
    // KAIROS-T-0581: a stored draft of this item, when it differs from
    // the server copy.
    let restored = draft_key.as_deref().and_then(|key| {
        draft_to_restore(read_draft(key).as_deref(), &initial_title, &initial_content)
    });
    let draft_restored = RwSignal::new(restored.is_some());
    let mode = RwSignal::new(initial_mode(start_in_preview, restored.is_some()).to_string());

    // Draft + baseline (baseline moves on take-theirs so the dirty check
    // stays honest).
    let (draft_title, draft_content, draft_version) = match restored {
        Some(draft) => (draft.title, draft.content, draft.version),
        None => (
            initial_title.clone(),
            initial_content.clone(),
            initial_version,
        ),
    };
    let title = RwSignal::new(draft_title);
    let content = RwSignal::new(draft_content);
    let orig_title = RwSignal::new(initial_title);
    let orig_content = RwSignal::new(initial_content);
    // The version the current draft is based on (A-0004: submitted with
    // every save; rebased by the merge dialog's outcomes). A restored
    // draft keeps its own base version.
    let base_version = RwSignal::new(draft_version);
    let server_version = StoredValue::new(initial_version);
    let draft_key = StoredValue::new(draft_key);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // Some = the merge dialog is open on this server-current snapshot.
    let conflict = RwSignal::new(None::<CurrentVersion>);
    // Some = manual merge in progress; the server copy stays visible.
    let reference = RwSignal::new(None::<CurrentVersion>);

    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();

    let dirty = move || title.get() != orig_title.get() || content.get() != orig_content.get();

    // KAIROS-T-0581: the draft follows each change into `sessionStorage`,
    // and leaves it when it equals its baseline.
    Effect::new(move |_| {
        let stored = draft_to_store(
            &title.read(),
            &content.read(),
            &orig_title.read(),
            &orig_content.read(),
            base_version.get(),
        );
        draft_key.with_value(|key| {
            if let Some(key) = key {
                write_draft(key, stored.as_deref());
            }
        });
    });
    // Ask before the tab closes or reloads with a dirty draft. The router
    // does not fire this event; the stored draft covers a navigation.
    let unload_guard = window_event_listener(leptos::ev::beforeunload, move |event| {
        let dirty_now = title.with_untracked(|t| orig_title.with_untracked(|o| t != o))
            || content.with_untracked(|c| orig_content.with_untracked(|o| c != o));
        if dirty_now && !read_only.get_untracked() {
            event.prevent_default();
            event.set_return_value("");
        }
    });
    on_cleanup(move || unload_guard.remove());
    // Discard the restored draft: back to the server copy of the mount.
    let discard = Callback::new(move |_| {
        title.set(orig_title.get_untracked());
        content.set(orig_content.get_untracked());
        base_version.set(server_version.get_value());
        draft_restored.set(false);
    });

    // One save routine; the merge dialog re-enters it with the rebased
    // version ("keep mine"). `new_local`: the saver is an `Rc` (wasm is
    // single-threaded; futures here are !Send by design).
    let saver = StoredValue::new_local(saver);
    let save_as = move |version: i32| {
        if saving.get_untracked() {
            return;
        }
        saving.set(true);
        error.set(None);
        let sent_title = title.get_untracked();
        let sent_content = content.get_untracked();
        leptos::task::spawn_local(async move {
            let future = saver.with_value(|s| s(sent_title.clone(), sent_content.clone(), version));
            let result = future.await;
            saving.set(false);
            match result {
                Ok(new_version) => {
                    conflict.set(None);
                    reference.set(None);
                    // The saved text is the new baseline: the stored
                    // draft goes (KAIROS-T-0581).
                    orig_title.set(sent_title);
                    orig_content.set(sent_content);
                    base_version.set(new_version);
                    draft_restored.set(false);
                    on_saved.run(new_version);
                }
                Err(SaveError::Conflict(current)) => conflict.set(Some(current)),
                Err(SaveError::Api(e)) => {
                    error.set(Some(super::item::api::error_text(&e)));
                }
            }
        });
    };
    let save = move |_| save_as(base_version.get_untracked());

    // The merge dialog is open while there is a conflict. Its three ways
    // out read the server copy at the time of the click.
    let conflict_open = RwSignal::new(false);
    Effect::new(move |_| conflict_open.set(conflict.with(Option::is_some)));
    let keep_mine = Callback::new(move |_| {
        if let Some(version) = conflict.with_untracked(|c| c.as_ref().map(|s| s.version)) {
            save_as(version);
        }
    });
    let take_theirs = Callback::new(move |_| {
        if let Some(s) = conflict.get_untracked() {
            title.set(s.title.clone());
            content.set(s.content.clone());
            orig_title.set(s.title.clone());
            orig_content.set(s.content);
            base_version.set(s.version);
        }
        draft_restored.set(false);
        conflict.set(None);
        reference.set(None);
    });
    let merge_manually = Callback::new(move |_| {
        if let Some(s) = conflict.get_untracked() {
            base_version.set(s.version);
            reference.set(Some(s));
        }
        conflict.set(None);
    });

    // Toolbar insertion at the cursor (KAIROS-T-0086). Selection indexes
    // are UTF-16 code units, so splicing happens in UTF-16 space — byte
    // indexes would panic mid-codepoint on non-ASCII content.
    let apply_syntax = move |prefix: &str, suffix: &str, placeholder: &str| {
        let Some(el) = textarea_ref.get_untracked() else {
            return;
        };
        let value = el.value();
        let units: Vec<u16> = value.encode_utf16().collect();
        let start = (el.selection_start().ok().flatten().unwrap_or(0) as usize).min(units.len());
        let end =
            (el.selection_end().ok().flatten().unwrap_or(0) as usize).clamp(start, units.len());
        let selected = String::from_utf16_lossy(&units[start..end]);
        let inserted = if selected.is_empty() {
            format!("{prefix}{placeholder}{suffix}")
        } else {
            format!("{prefix}{selected}{suffix}")
        };
        let inserted_units: Vec<u16> = inserted.encode_utf16().collect();
        let mut new_units = Vec::with_capacity(units.len() + inserted_units.len());
        new_units.extend_from_slice(&units[..start]);
        new_units.extend_from_slice(&inserted_units);
        new_units.extend_from_slice(&units[end..]);
        let new_value = String::from_utf16_lossy(&new_units);
        // Element first (cursor placement), then the signal — same string,
        // so the reactive re-render leaves the cursor where we put it.
        el.set_value(&new_value);
        let cursor = (start + inserted_units.len()) as u32;
        let _ = el.set_selection_range(cursor, cursor);
        let _ = el.focus();
        content.set(new_value);
    };

    view! {
        <Panel title="Content" caption="markdown">
            <div class="kairos-editor">
                <Group justify="between">
                    {move || (!read_only.get()).then(|| view! {
                        <SegmentedControl
                            options=vec!["Edit".to_string(), "Preview".to_string()]
                            value=mode
                        />
                    })}
                    <Group gap="sm">
                        <Pill color=token::ICE>
                            {move || if read_only.get() {
                                format!("v{} — read-only", base_version.get())
                            } else {
                                format!("editing v{}", base_version.get())
                            }}
                        </Pill>
                        <Button
                            disabled=move || read_only.get() || !dirty()
                            loading=saving
                            loading_label="Saving…"
                            on_click=Callback::new(save)
                        >
                            "Save"
                        </Button>
                    </Group>
                </Group>

                {move || {
                    let reason = read_only_reason.get();
                    (read_only.get() && !reason.is_empty()).then(|| view! {
                        <span data-testid="editor-read-only-reason">
                            <Text size="xs" dimmed=true>{reason}</Text>
                        </span>
                    })
                }}

                {move || (draft_restored.get() && dirty() && !read_only.get()).then(|| view! {
                    <div data-testid="editor-draft-restored">
                        <Alert title="Draft restored" color=token::GOLD>
                            <Group justify="between">
                                <Text size="sm" dimmed=true>
                                    "This is your draft from before. Save it, or discard it."
                                </Text>
                                <Button variant="default" size="xs" on_click=discard>
                                    "Discard draft"
                                </Button>
                            </Group>
                        </Alert>
                    </div>
                })}

                {move || error.get().map(|message| view! {
                    <Alert title="Save failed" color=token::BAD>
                        <Text size="sm" dimmed=true>{message}</Text>
                    </Alert>
                })}

                {move || if read_only.get() {
                    // Read-only renders the content as it stands on the
                    // server: the title as text, the markdown rendered. No
                    // textarea, so there is nothing to type into and lose.
                    // A stored draft is not shown here and stays stored.
                    view! {
                        <Text bright=true bold=true>{orig_title.get()}</Text>
                        <div class="kairos-markdown" inner_html=markdown::to_html(&orig_content.get())></div>
                    }.into_any()
                } else if mode.get() == "Preview" {
                    view! {
                        <div class="kairos-markdown" inner_html=markdown::to_html(&content.get())></div>
                    }.into_any()
                } else {
                    view! {
                        <TextInput label="Title" value=title/>
                        <Group justify="between">
                            <label class="cl-field__label" for="kairos-editor-content">"Content"</label>
                            <Group gap="xs">
                                {TOOLBAR.iter().map(|(label, tooltip, prefix, suffix, placeholder)| {
                                    view! {
                                        <ActionIcon
                                            title=*tooltip
                                            on_click=Callback::new(move |_| apply_syntax(prefix, suffix, placeholder))
                                        >
                                            {*label}
                                        </ActionIcon>
                                    }
                                }).collect_view()}
                            </Group>
                        </Group>
                        <textarea
                            class="kairos-editor__textarea"
                            id="kairos-editor-content"
                            node_ref=textarea_ref
                            prop:value=move || content.get()
                            on:input=move |e| content.set(event_target_value(&e))
                        ></textarea>
                    }.into_any()
                }}

                // Manual merge: the server copy stays pinned for reference.
                {move || reference.get().map(|server| view! {
                    <Alert title=format!("Merging by hand onto v{}", server.version) color=token::GOLD>
                        <div class="kairos-editor__reference">
                            <Group justify="between">
                                <Text size="sm" dimmed=true>
                                    "The server copy is below for reference; edit your draft above and save when merged."
                                </Text>
                                <ActionIcon title="Dismiss the server copy" on_click=Callback::new(move |_| reference.set(None))>
                                    "×"
                                </ActionIcon>
                            </Group>
                            <Text size="sm" bright=true>{server.title.clone()}</Text>
                            <textarea
                                class="kairos-editor__textarea kairos-editor__textarea--pane"
                                readonly=true
                                prop:value=server.content.clone()
                            ></textarea>
                        </div>
                    </Alert>
                })}
            </div>
        </Panel>

        // ---- the 409 merge dialog ------------------------------------------
        // COLLIERY-T-1836: an Aurora `Modal` (xl, 1060px) replaces the local
        // dialog. Escape, the scrim and × cancel, as the Cancel button
        // does; the dialog is locked while a save runs.
        <Modal
            open=conflict_open
            title="Edit conflict"
            size="xl"
            locked=saving
            on_close=Callback::new(move |_| conflict.set(None))
            footer=std::sync::Arc::new(move || view! {
                <Button variant="default" on_click=Callback::new(move |_| conflict.set(None))>
                    "Cancel"
                </Button>
                <Button variant="default" on_click=take_theirs>"Take theirs"</Button>
                <Button variant="default" on_click=merge_manually>"Merge manually"</Button>
                <Button loading=saving loading_label="Saving…" on_click=keep_mine>
                    "Keep mine (overwrite)"
                </Button>
            }.into_any())
        >
            {move || conflict.get().map(|server| view! {
                <Group justify="between">
                    <Text size="sm" dimmed=true>
                        "Someone saved this content while you were editing. Nothing was overwritten — pick how to resolve; every path retries against the server's current version."
                    </Text>
                    <Pill color=token::BAD>
                        {format!(
                            "server is at v{} — you edited v{}",
                            server.version,
                            base_version.get_untracked(),
                        )}
                    </Pill>
                </Group>
                <SimpleGrid cols=2>
                    <Panel
                        title=format!("On the server — v{}", server.version)
                        caption="theirs"
                    >
                        <Text size="sm" bright=true>{server.title.clone()}</Text>
                        <textarea
                            class="kairos-editor__textarea kairos-editor__textarea--pane"
                            readonly=true
                            prop:value=server.content.clone()
                        ></textarea>
                    </Panel>
                    <Panel
                        title=format!("Your edit — based on v{}", base_version.get_untracked())
                        caption="yours"
                    >
                        <Text size="sm" bright=true>{title.get_untracked()}</Text>
                        <textarea
                            class="kairos-editor__textarea kairos-editor__textarea--pane"
                            readonly=true
                            prop:value=content.get_untracked()
                        ></textarea>
                    </Panel>
                </SimpleGrid>
            })}
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KAIROS-T-0323: the page of an item opens in Preview; the other
    /// editors open in Edit, as before. A restored draft opens in Edit
    /// (KAIROS-T-0581).
    #[test]
    fn the_editor_opens_in_preview_only_when_asked() {
        assert_eq!(initial_mode(true, false), "Preview");
        assert_eq!(initial_mode(false, false), "Edit");
        assert_eq!(initial_mode(true, true), "Edit");
    }

    /// KAIROS-T-0581: a draft that differs from its baseline is stored; a
    /// clean one is removed.
    #[test]
    fn only_a_dirty_draft_is_stored() {
        assert_eq!(draft_to_store("t", "c", "t", "c", 3), None);
        let stored = draft_to_store("t", "mine", "t", "c", 3).expect("dirty draft");
        let draft: Draft = serde_json::from_str(&stored).expect("draft json");
        assert_eq!(
            draft,
            Draft {
                title: "t".to_string(),
                content: "mine".to_string(),
                version: 3,
            }
        );
        assert!(draft_to_store("new title", "c", "t", "c", 3).is_some());
    }

    /// KAIROS-T-0581: the mount restores a stored draft with its own base
    /// version, and nothing for a draft that equals the server copy, an
    /// unreadable value, or no value.
    #[test]
    fn the_mount_restores_a_draft_that_differs() {
        let stored = draft_to_store("t", "mine", "t", "c", 3);
        let restored = draft_to_restore(stored.as_deref(), "t", "c").expect("restored");
        assert_eq!(restored.content, "mine");
        // The server moved on: the draft keeps v3, so a save gets 409.
        let after_change = draft_to_restore(stored.as_deref(), "t", "theirs").expect("restored");
        assert_eq!(after_change.version, 3);
        // Someone saved the same text: nothing to restore.
        assert_eq!(draft_to_restore(stored.as_deref(), "t", "mine"), None);
        assert_eq!(draft_to_restore(Some("not json"), "t", "c"), None);
        assert_eq!(draft_to_restore(None, "t", "c"), None);
    }

    /// One key for each item, under the prefix that a logout clears.
    #[test]
    fn the_draft_key_names_the_item() {
        assert_eq!(draft_key("DEMO-T-0007"), "kairos-draft:DEMO-T-0007");
        assert!(draft_key("X-T-0001").starts_with(DRAFT_PREFIX));
    }
}
