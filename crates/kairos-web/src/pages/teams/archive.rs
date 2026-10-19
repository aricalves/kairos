//! The archive of the team page and the history of a page (KAIROS-T-0439).
//!
//! - Archiving a folder archives everything in it, at one time. The archive
//!   lists each archived node, newest first; "Restore" brings back the node
//!   and what was archived with it, to its folder, or to the root when the
//!   folder is gone.
//! - The history of a page lists each saved version, newest first, with the
//!   person who saved it, and "(agent)" when an agent saved it with the
//!   agent key of the person. The list has no content: "View" reads that
//!   version (KAIROS-T-0629).

use aurora_dark::components::{Alert, Button, Empty, Group, Loading, Panel, Pill, Stack, Text};
use aurora_dark::tokens::token;
use leptos::prelude::*;

use super::api;
use crate::auth::use_auth;
use crate::pages::item::api::error_text;
use crate::pages::item::markdown;

/// The date and the minute of an RFC 3339 time ("2026-10-14 09:43").
fn short_time(rfc3339: &str) -> String {
    rfc3339.get(..16).unwrap_or(rfc3339).replace('T', " ")
}

/// The editor of a version: "Alice", or "Alice (agent)".
pub(crate) fn editor_label(version: &api::PageVersion) -> String {
    if version.agent_key_id.is_some() {
        format!("{} (agent)", version.edited_by_name)
    } else {
        version.edited_by_name.clone()
    }
}

/// The archived nodes of a team, with "Restore" for a member or an admin.
#[component]
pub(crate) fn ArchivePanel(
    team_id: String,
    manage: bool,
    on_changed: Callback<()>,
) -> impl IntoView {
    let auth = use_auth();
    let team_id = StoredValue::new(team_id);
    let rows = LocalResource::new(move || {
        auth.track_session();
        async move { api::archived_pages(auth, &team_id.get_value()).await }
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let restore = move |page_id: String| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let result = api::restore_page(auth, &team_id.get_value(), &page_id).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    rows.refetch();
                    on_changed.run(());
                }
                Err(e) => error.set(Some(error_text(&e))),
            }
        });
    };
    view! {
        <Panel title="Archived" caption="archived pages and folders — a folder takes what is in it">
            <Stack gap="xs">
                {move || error.get().map(|message| view! {
                    <Alert title="Restore failed" color=token::BAD>
                        <Text size="sm" dimmed=true>{message}</Text>
                    </Alert>
                })}
                {move || match rows.get() {
                    None => view! { <Loading label="Loading the archive…"/> }.into_any(),
                    Some(Err(e)) => view! { <Text dimmed=true size="sm">{error_text(&e)}</Text> }.into_any(),
                    Some(Ok(rows)) if rows.is_empty() => {
                        view! { <Empty message="Nothing is archived."/> }.into_any()
                    }
                    Some(Ok(rows)) => rows
                        .into_iter()
                        .map(|row| {
                            let id = row.id.clone();
                            let kind = if row.kind == "folder" { "folder" } else { "page" };
                            view! {
                                <Group justify="between">
                                    <Group gap="xs">
                                        <Pill color=token::ICE>{kind}</Pill>
                                        <Text bright=true size="sm">{row.title.clone()}</Text>
                                        <Text dimmed=true size="xs">{format!("archived {}", short_time(&row.archived_at))}</Text>
                                    </Group>
                                    {manage.then(|| {
                                        let id = id.clone();
                                        view! {
                                            <Button
                                                variant="default"
                                                size="xs"
                                                disabled=busy
                                                on_click=Callback::new(move |_| restore(id.clone()))
                                            >
                                                "Restore"
                                            </Button>
                                        }
                                    })}
                                </Group>
                            }
                            .into_any()
                        })
                        .collect::<Vec<_>>()
                        .into_any(),
                }}
            </Stack>
        </Panel>
    }
}

/// The saved versions of a page, loaded when the person opens the panel.
#[component]
pub(crate) fn HistoryPanel(team_id: String, page_id: String) -> impl IntoView {
    let auth = use_auth();
    let open = RwSignal::new(false);
    let shown = RwSignal::new(None::<i32>);
    let team_id = StoredValue::new(team_id);
    let page_id = StoredValue::new(page_id);
    let versions = LocalResource::new(move || {
        let wanted = open.get();
        async move {
            if !wanted {
                return Ok(Vec::new());
            }
            api::page_history(auth, &team_id.get_value(), &page_id.get_value()).await
        }
    });
    // KAIROS-T-0629: the list has no content; read the version shown.
    let snapshot = LocalResource::new(move || {
        let wanted = shown.get();
        async move {
            match wanted {
                None => Ok(None),
                Some(version) => {
                    api::page_version(auth, &team_id.get_value(), &page_id.get_value(), version)
                        .await
                        .map(Some)
                }
            }
        }
    });
    view! {
        <Panel title="History" caption="each saved version, newest first">
            {move || if !open.get() {
                view! {
                    <Button variant="default" size="xs" on_click=Callback::new(move |_| open.set(true))>
                        "Show history"
                    </Button>
                }.into_any()
            } else {
                match versions.get() {
                    None => view! { <Loading label="Loading the history…"/> }.into_any(),
                    Some(Err(e)) => view! { <Text dimmed=true size="sm">{error_text(&e)}</Text> }.into_any(),
                    Some(Ok(rows)) => view! {
                        <Stack gap="xs">
                            {rows.into_iter().map(|row| {
                                let number = row.version;
                                let label = format!("v{} · {} · {}", row.version, editor_label(&row), short_time(&row.edited_at));
                                view! {
                                    <Stack gap="xs">
                                        <Group justify="between">
                                            <Text size="sm">{label}</Text>
                                            <Button
                                                variant="subtle"
                                                size="xs"
                                                on_click=Callback::new(move |_| {
                                                    shown.update(|s| *s = if *s == Some(number) { None } else { Some(number) })
                                                })
                                            >
                                                {move || if shown.get() == Some(number) { "Hide" } else { "View" }}
                                            </Button>
                                        </Group>
                                        {move || (shown.get() == Some(number)).then(|| match snapshot.get() {
                                            Some(Ok(Some(version))) if version.version == number => view! {
                                                <Stack gap="xs">
                                                    <Text bright=true size="sm">{version.title.clone()}</Text>
                                                    <div class="kairos-markdown" inner_html=markdown::to_html(&version.content)></div>
                                                </Stack>
                                            }.into_any(),
                                            Some(Err(e)) => view! { <Text dimmed=true size="sm">{error_text(&e)}</Text> }.into_any(),
                                            _ => view! { <Loading label="Loading the version…"/> }.into_any(),
                                        })}
                                    </Stack>
                                }
                            }).collect::<Vec<_>>()}
                        </Stack>
                    }.into_any(),
                }
            }}
        </Panel>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(agent: bool) -> api::PageVersion {
        api::PageVersion {
            version: 2,
            title: "Charter".into(),
            edited_by_name: "Robin".into(),
            edited_at: "2026-10-14T09:43:36.123+00:00".into(),
            agent_key_id: agent.then(|| "k1".into()),
        }
    }

    #[test]
    fn an_agent_save_is_marked() {
        assert_eq!(editor_label(&version(false)), "Robin");
        assert_eq!(editor_label(&version(true)), "Robin (agent)");
    }

    #[test]
    fn the_time_is_the_date_and_the_minute() {
        assert_eq!(
            short_time("2026-10-14T09:43:36.123+00:00"),
            "2026-10-14 09:43"
        );
        assert_eq!(short_time("short"), "short");
    }
}
