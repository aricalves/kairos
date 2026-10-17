//! The external references of an item (KAIROS-T-0508, COLLIERY-I-0630):
//! the URIs that the item names, each with a label. An RFC in a wiki, a
//! design page, an issue that Kairos does not hold.
//!
//! The panel lists the references with the label as the text of the link.
//! A link leaves the app: it opens in a new tab with `rel="noopener
//! noreferrer"`, so the page that opens gets no handle on this one. The
//! server admits the schemes `http`, `https` and `mailto` only, so an
//! `href` is never a script.
//!
//! The add and the remove take the edit rule of the item
//! ([`boards::may_edit_item`]), as the server does. The server remains
//! the authority: a refusal shows in the panel.

use aurora_dark::components::{Alert, Anchor, Button, Group, Panel, Stack, Text, TextInput};
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;

use super::api::{self, ExternalRef, Family};
use crate::auth::use_auth;
use crate::pages::boards;

/// The text of a link: the label when the reference has one, the URI when
/// it has none. Pure, host-tested.
pub(crate) fn link_text(reference: &ExternalRef) -> String {
    match reference.label.as_deref().map(str::trim) {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => reference.uri.clone(),
    }
}

/// The capability of the edit rule for a family.
fn manage_capability(family: Family) -> &'static str {
    match family {
        Family::Strategy => "manage_strategies",
        Family::Initiative => "manage_initiatives",
        Family::Task => "manage_tasks",
        Family::Document => "manage_documents",
        Family::Adr => "manage_adrs",
    }
}

/// The external references of an item, with the add and the remove of a
/// reference for a user who can edit the item.
#[component]
pub fn ExternalRefsPanel(
    family: Family,
    #[prop(into)] code: String,
    /// The page's shared board read: the board of the edit rule.
    board: LocalResource<Result<Option<api::BoardInfo>, ApiError>>,
    /// The user id of the creator of the item.
    #[prop(into)]
    created_by: String,
    /// The item is archived: each write resolves live items only.
    archived: bool,
) -> impl IntoView {
    let auth = use_auth();
    let code = StoredValue::new(code);
    let created_by = StoredValue::new(created_by);
    let reload = RwSignal::new(0u32);
    let refs = LocalResource::new(move || {
        auth.track_session();
        let _ = reload.get();
        api::fetch_external_refs(auth, family, code.get_value())
    });
    let uri = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error: RwSignal<Option<ApiError>> = RwSignal::new(None);
    let whoami = use_context::<LocalResource<Result<crate::api::Whoami, ApiError>>>();
    let required = manage_capability(family);
    let can_edit = Memo::new(move |_| {
        let me = whoami
            .and_then(|resource| resource.get())
            .and_then(Result::ok);
        let (Some(me), Some(board)) = (me, board.get()) else {
            return false;
        };
        let board = board.ok().flatten();
        created_by.with_value(|created_by| {
            boards::may_edit_item(
                &me,
                created_by,
                board.as_ref().map(|board| board.slug.as_str()),
                board.as_ref().and_then(|board| board.team_id.as_deref()),
                required,
            )
        })
    });

    let add: Callback<()> = Callback::new(move |()| {
        let target = uri.get_untracked().trim().to_string();
        if target.is_empty() {
            return;
        }
        let text = label.get_untracked().trim().to_string();
        let text = (!text.is_empty()).then_some(text);
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let result =
                api::add_external_ref(auth, family, &code.get_value(), &target, text.as_deref())
                    .await;
            busy.set(false);
            match result {
                Ok(_) => {
                    uri.set(String::new());
                    label.set(String::new());
                    reload.update(|n| *n += 1);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    });
    let remove: Callback<String> = Callback::new(move |id: String| {
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let result = api::remove_external_ref(auth, family, &code.get_value(), &id).await;
            busy.set(false);
            match result {
                Ok(()) => reload.update(|n| *n += 1),
                Err(e) => error.set(Some(e)),
            }
        });
    });

    view! {
        <Panel title="External references" caption="documents outside Kairos that this item names">
            <Stack gap="sm" attr:data-testid="external-refs">
                {move || match refs.get() {
                    Some(Ok(refs)) if refs.is_empty() => view! {
                        <Text size="sm" dimmed=true>"This item names no external document."</Text>
                    }.into_any(),
                    Some(Ok(refs)) => refs.into_iter().map(|reference| {
                        let text = link_text(&reference);
                        let href = reference.uri.clone();
                        let shown_uri = reference.uri.clone();
                        let id = reference.id.clone();
                        let row = reference.uri.clone();
                        let has_label = text != shown_uri;
                        view! {
                            <Group gap="sm" justify="between" wrap=true attr:data-external=row>
                                <Stack gap="xs">
                                    // Leaves the app: a new tab, and the
                                    // opener reference severed.
                                    <Anchor href=href attr:target="_blank" attr:rel="noopener noreferrer">
                                        {text}
                                    </Anchor>
                                    {has_label.then(|| view! {
                                        <Text mono=true dimmed=true size="xs">{shown_uri}</Text>
                                    })}
                                </Stack>
                                {move || (!archived && can_edit.get()).then(|| {
                                    let id = id.clone();
                                    let disabled = busy.get();
                                    view! {
                                        <Button variant="default" size="xs" disabled=disabled
                                            on_click=Callback::new(move |_| remove.run(id.clone()))>
                                            "Remove"
                                        </Button>
                                    }
                                })}
                            </Group>
                        }
                    }).collect_view().into_any(),
                    Some(Err(e)) => view! {
                        <Text size="sm" dimmed=true>{api::error_text(&e)}</Text>
                    }.into_any(),
                    None => ().into_any(),
                }}
                {move || (!archived && can_edit.get()).then(|| view! {
                    <Stack gap="xs" attr:data-testid="external-refs-add">
                        <TextInput label="URI" placeholder="https://example.com/wiki/rfc-42" value=uri/>
                        <TextInput label="Label" placeholder="RFC: the title, accepted 2026-10-05" value=label/>
                        <Group justify="end">
                            <Button
                                size="xs"
                                disabled=move || busy.get() || uri.get().trim().is_empty()
                                on_click=add
                            >
                                "Add reference"
                            </Button>
                        </Group>
                    </Stack>
                })}
                <Text size="xs" dimmed=true>
                    "The scheme of the URI is http, https or mailto. Kairos does not read the document."
                </Text>
                {move || error.get().map(|e| view! {
                    <Alert title="Kairos did not change the reference" color=token::BAD>
                        <Text size="sm">{api::error_text(&e)}</Text>
                    </Alert>
                })}
            </Stack>
        </Panel>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(uri: &str, label: Option<&str>) -> ExternalRef {
        ExternalRef {
            id: "r-1".into(),
            uri: uri.into(),
            label: label.map(str::to_string),
        }
    }

    /// The label is the text of the link; with no label, the URI is.
    #[test]
    fn the_link_text_is_the_label_or_the_uri() {
        assert_eq!(
            link_text(&reference("https://example.com/rfc", Some("RFC 42"))),
            "RFC 42"
        );
        assert_eq!(
            link_text(&reference("https://example.com/rfc", None)),
            "https://example.com/rfc"
        );
        assert_eq!(
            link_text(&reference("https://example.com/rfc", Some("  "))),
            "https://example.com/rfc"
        );
    }

    /// The edit rule asks for `manage_<family>`, as each other panel.
    #[test]
    fn the_capability_follows_the_family() {
        assert_eq!(manage_capability(Family::Task), "manage_tasks");
        assert_eq!(manage_capability(Family::Adr), "manage_adrs");
    }
}
