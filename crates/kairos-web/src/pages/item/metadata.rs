//! The typed metadata panel (KAIROS-T-0041, per KAIROS-A-0003; scoping per
//! KAIROS-T-0065): the panel shows editors only for fields the item
//! ACTUALLY CARRIES — values stamped at creation from its template's
//! declared fields, or added here explicitly. The full definition catalog
//! never renders wholesale (that put a "Document Type" editor on every
//! task); unstamped definitions sit behind an "add a field" picker
//! instead. Editors match the field type — enum → dropdown of the
//! definition's options, date → a date input, string → text. Clearing a
//! field sends `null` (the A-0003 "null clears" contract); saving PATCHes
//! only the fields that changed. Metadata is not versioned (A-0004):
//! last-write-wins, no conflict UI here.
//!
//! KAIROS-T-0581: a person who may not edit sees the values read-only,
//! with the reason ([`super::access`]). A value with no definition in
//! scope shows read-only too, so no value disappears. The text for no
//! definition follows the role: only an org admin can add one.

use std::collections::BTreeMap;

use aurora_dark::components::{
    Alert, Button, Empty, ErrorState, Group, Loading, Panel, Pill, Select, Text, TextInput,
};
use aurora_dark::tokens::token;
use leptos::prelude::*;

use super::access;
use super::api::{self, Family, MetadataDefinition, MetadataValue};
use crate::auth::use_auth;

/// One field's editing state: its definition, the live draft, and the
/// value the server had when the panel loaded.
#[derive(Clone)]
struct FieldRow {
    definition: MetadataDefinition,
    draft: RwSignal<String>,
    original: String,
}

/// The metadata panel: definitions + values fetched together (one `join!`,
/// KAIROS-T-0618), typed editors, one save for all changed fields.
#[component]
pub fn MetadataPanel(
    family: Family,
    #[prop(into)] code: String,
    /// The item is archived (KAIROS-T-0164, ADR-20): its values are still
    /// shown — they are part of what the record said, and KAIROS-T-0152's
    /// `DEFINITION_IN_USE` refusal sends admins here to read them — but
    /// the metadata PATCH resolves live-only, so the editors are disabled
    /// rather than left to fail. KAIROS-T-0581: also when the person may
    /// not edit the item; a signal, because whoami resolves later.
    #[prop(into)]
    read_only: Signal<bool>,
    /// Why the values are read-only; empty while the page does not know.
    #[prop(into)]
    read_only_reason: Signal<String>,
    /// The page bumps this to read the values again: Reload of the banner
    /// "Changed on the server" (KAIROS-T-0578).
    #[prop(optional, into)]
    reload: Signal<u32>,
    /// The panel writes here the values that it shows, so the live page
    /// can compare them with the values on the server (KAIROS-T-0578).
    #[prop(optional)]
    seen: Option<RwSignal<Option<Vec<MetadataValue>>>>,
) -> impl IntoView {
    let page_reload = reload;
    let auth = use_auth();
    let whoami =
        use_context::<LocalResource<Result<crate::api::Whoami, aurora_dark::tokens::ApiError>>>();
    let is_admin = Signal::derive(move || {
        whoami
            .and_then(|resource| resource.get())
            .and_then(Result::ok)
            .is_some_and(|me| me.organization.role == "admin")
    });
    let code = StoredValue::new(code);
    let reload = RwSignal::new(0u32);

    let data = LocalResource::new(move || {
        auth.track_session();
        let _ = reload.get();
        let _ = page_reload.get();
        // KAIROS-T-0618: the two reads go at the same time.
        let code = code.get_value();
        async move {
            let (definitions, values) = futures_util::join!(
                api::fetch_definitions(auth, family),
                api::fetch_metadata(auth, family, code),
            );
            Ok::<_, aurora_dark::tokens::ApiError>((definitions?, values?))
        }
    });
    let retry = Callback::new(move |_| reload.update(|n| *n += 1));
    if let Some(seen) = seen {
        Effect::new(move |_| {
            if let Some(Ok((_, values))) = data.get() {
                seen.set(Some(values));
            }
        });
    }

    view! {
        <Panel title="Metadata" caption="typed fields">
            {move || match data.get() {
                None => view! { <Loading label="Loading metadata…"/> }.into_any(),
                Some(Err(error)) => view! { <ErrorState error on_retry=retry/> }.into_any(),
                Some(Ok((definitions, values))) if definitions.is_empty() => {
                    let orphans = access::orphan_values(&definitions, &values);
                    view! {
                        <span data-testid="metadata-no-definitions">
                            <Empty message=access::no_definitions_text(is_admin.get())/>
                        </span>
                        <OrphanValues orphans/>
                    }.into_any()
                }
                Some(Ok((definitions, values))) => view! {
                    <MetadataForm
                        family
                        code=code.get_value()
                        definitions
                        values
                        read_only
                        read_only_reason
                        on_saved=Callback::new(move |_| reload.update(|n| *n += 1))
                    />
                }.into_any(),
            }}
        </Panel>
    }
}

/// The "add a field" picker's no-choice option.
const ADD_PLACEHOLDER: &str = "(add a field…)";

/// The editors + save button, built fresh per fetch (drafts start at the
/// server's values). Only STAMPED fields render editors (KAIROS-T-0065);
/// the rest of the catalog waits behind the add-a-field picker.
#[component]
fn MetadataForm(
    family: Family,
    #[prop(into)] code: String,
    definitions: Vec<MetadataDefinition>,
    values: Vec<MetadataValue>,
    /// Show the values, refuse the writes (KAIROS-T-0164, KAIROS-T-0581).
    read_only: Signal<bool>,
    /// Why the values are read-only.
    read_only_reason: Signal<String>,
    on_saved: Callback<()>,
) -> impl IntoView {
    // KAIROS-T-0581: the values with no definition in scope; read-only.
    let orphans = access::orphan_values(&definitions, &values);
    let has_orphans = !orphans.is_empty();
    let auth = use_auth();
    let code = StoredValue::new(code);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    // EVERY definition gets its row (and its draft signal) up front, at
    // render time; `visible` decides which rows show. The picker only
    // pushes a slug into `visible` — it never creates signals. Two prior
    // shapes of this code broke on the add path (T-0078 review): an
    // Effect resetting the signal it watched re-entered itself and
    // panicked the reactive runtime, and creating the draft signal inside
    // the change handler (no reactive owner) produced a dead editor whose
    // writes never propagated.
    let all_rows: StoredValue<Vec<FieldRow>> = StoredValue::new(
        definitions
            .into_iter()
            .map(|definition| {
                let original = values
                    .iter()
                    .find(|value| value.slug == definition.slug)
                    .map(|value| value.value.clone())
                    .unwrap_or_default();
                FieldRow {
                    definition,
                    draft: RwSignal::new(original.clone()),
                    original,
                }
            })
            .collect(),
    );
    let visible: RwSignal<Vec<String>> =
        RwSignal::new(values.iter().map(|value| value.slug.clone()).collect());

    // Choosing a definition in the picker reveals its (empty) editor row;
    // the value only reaches the server on save. The picker is an Aurora
    // `Select` (COLLIERY-T-1836) bound to `pick`; the change handler puts
    // it back on its placeholder, so the next add is a fresh change. A
    // handler, not an Effect: no reactive re-entry (T-0078 review).
    let pick = RwSignal::new(ADD_PLACEHOLDER.to_string());
    let on_pick = Callback::new(move |choice: String| {
        if choice == ADD_PLACEHOLDER {
            return;
        }
        let slug = all_rows.with_value(|rows| {
            rows.iter()
                .find(|row| row.definition.name == choice)
                .map(|row| row.definition.slug.clone())
        });
        if let Some(slug) = slug {
            visible.update(|shown| {
                if !shown.contains(&slug) {
                    shown.push(slug);
                }
            });
        }
        pick.set(ADD_PLACEHOLDER.to_string());
    });

    // Hidden rows are never dirty (draft == original == server value).
    let dirty =
        move || all_rows.with_value(|rows| rows.iter().any(|row| row.draft.get() != row.original));

    let save = move |_: ()| {
        if saving.get_untracked() {
            return;
        }
        // Changed fields only; "" means clear (A-0003 null-clears).
        let changed: BTreeMap<String, Option<String>> = all_rows.with_value(|rows| {
            rows.iter()
                .filter(|row| row.draft.get_untracked() != row.original)
                .map(|row| {
                    let draft = row.draft.get_untracked();
                    let value = (!draft.is_empty()).then_some(draft);
                    (row.definition.slug.clone(), value)
                })
                .collect()
        });
        if changed.is_empty() {
            return;
        }
        saving.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let result = api::update_metadata(auth, family, &code.get_value(), changed).await;
            saving.set(false);
            match result {
                Ok(_) => on_saved.run(()),
                Err(e) => error.set(Some(api::error_text(&e))),
            }
        });
    };

    view! {
        <div class="kairos-metadata">
            {move || error.get().map(|message| view! {
                <Alert title="Metadata save failed" color=token::BAD>
                    <Text size="sm" dimmed=true>{message}</Text>
                </Alert>
            })}
            {move || {
                // Render in VISIBLE order (stamped first, then picks in
                // the order made), so a freshly added row lands at the
                // bottom, right above the picker — in catalog order an
                // add could insert above existing rows, out of view of
                // the picker the user just used.
                let shown = visible.get();
                let current: Vec<FieldRow> = all_rows.with_value(|rows| {
                    shown
                        .iter()
                        .filter_map(|slug| {
                            rows.iter().find(|row| &row.definition.slug == slug)
                        })
                        .cloned()
                        .collect()
                });
                if current.is_empty() && !has_orphans {
                    view! {
                        <Text size="sm" dimmed=true>
                            {access::no_values_text(read_only.get())}
                        </Text>
                    }.into_any()
                } else {
                    current
                        .into_iter()
                        .map(|row| view! { <FieldEditor row read_only/> })
                        .collect_view()
                        .into_any()
                }
            }}
            <OrphanValues orphans=orphans.clone()/>
            {move || (!read_only.get()).then(|| view! {{move || {
                let shown = visible.get();
                let available: Vec<String> = all_rows.with_value(|rows| {
                    rows.iter()
                        .filter(|row| !shown.contains(&row.definition.slug))
                        .map(|row| row.definition.name.clone())
                        .collect()
                });
                (!available.is_empty()).then(|| {
                    let mut options = vec![ADD_PLACEHOLDER.to_string()];
                    options.extend(available);
                    view! {
                        <Select options value=pick on_change=on_pick/>
                    }
                })
            }}})}
            <Group justify="between">
                <span data-testid="metadata-read-only-reason">
                    <Text size="xs" dimmed=true>
                        {move || metadata_hint(read_only.get(), read_only_reason.get())}
                    </Text>
                </span>
                <Button
                    size="xs"
                    disabled=move || read_only.get() || !dirty()
                    loading=saving
                    loading_label="Saving…"
                    on_click=Callback::new(save)
                >
                    "Save metadata"
                </Button>
            </Group>
        </div>
    }
}

/// The line under the editors: the reason while read-only, else the rules
/// of a save. Pure, host-tested.
fn metadata_hint(read_only: bool, reason: String) -> String {
    if !read_only {
        "Blank clears a field. Last write wins (not versioned).".to_string()
    } else {
        reason
    }
}

/// The values with no definition in scope (KAIROS-T-0581): the slug and
/// the value, read-only. Nothing renders when there is none.
#[component]
fn OrphanValues(orphans: Vec<MetadataValue>) -> impl IntoView {
    (!orphans.is_empty()).then(|| {
        view! {
            <div class="kairos-metadata__orphans" data-testid="metadata-orphans">
                <Text size="xs" dimmed=true>
                    "These values have no definition for this item type. They are read-only."
                </Text>
                {orphans.into_iter().map(|MetadataValue { slug, value }| {
                    let label = slug.clone();
                    view! {
                        <div class="kairos-metadata__field" data-slug=slug>
                            <span class="kairos-metadata__type">
                                <Pill color=token::GOLD>"no definition"</Pill>
                            </span>
                            <TextInput label value=RwSignal::new(value) disabled=true/>
                        </div>
                    }
                }).collect_view()}
            </div>
        }
    })
}

/// One typed editor row: label + the editor its `field_type` calls for.
/// `read_only` disables the control itself (KAIROS-T-0164) — a field that
/// accepts typing and then cannot save is worse than one that says no.
#[component]
fn FieldEditor(row: FieldRow, read_only: Signal<bool>) -> impl IntoView {
    let FieldRow {
        definition, draft, ..
    } = row;
    let type_pill = match definition.field_type.as_str() {
        "enum" => ("enum", token::VIOLET),
        "date" => ("date", token::TEAL),
        _ => ("string", token::ICE),
    };
    // COLLIERY-T-1836: the Aurora fields, each with its label pointing to
    // its control. The type pill sits at the right of the label row.
    let name = definition.name.clone();
    let editor = match definition.field_type.as_str() {
        "enum" => {
            // "" is the explicit unset choice; the rest come from the
            // definition, in its display order.
            let options: Vec<(String, String)> = std::iter::once((String::new(), "—".to_string()))
                .chain(
                    definition
                        .enum_options
                        .iter()
                        .map(|o| (o.clone(), o.clone())),
                )
                .collect();
            view! {
                <Select label=name option_pairs=options value=draft disabled=read_only/>
            }
            .into_any()
        }
        "date" => view! {
            <TextInput label=name input_type="date" value=draft disabled=read_only/>
        }
        .into_any(),
        _ => view! {
            <TextInput label=name placeholder="—" value=draft disabled=read_only/>
        }
        .into_any(),
    };
    view! {
        <div class="kairos-metadata__field">
            <span class="kairos-metadata__type">
                <Pill color=type_pill.1>{type_pill.0}</Pill>
            </span>
            {editor}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KAIROS-T-0581: read-only shows the reason; else the save rules.
    #[test]
    fn the_hint_is_the_reason_while_read_only() {
        assert_eq!(
            metadata_hint(true, "You cannot edit this item.".to_string()),
            "You cannot edit this item."
        );
        assert!(metadata_hint(false, "ignored".to_string()).starts_with("Blank clears"));
    }
}
