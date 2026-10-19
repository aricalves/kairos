//! `/agent-keys` (KAIROS-T-0359, KAIROS-A-0024): the agent keys of the
//! signed-in person. Make a key (the secret shows once), list the keys, and
//! revoke one. An agent key acts as the person, with the capabilities of the
//! person.
//!
//! KAIROS-T-0370: a new key expires after the default life of the deployment
//! (90 days unless the operator changed it), or never when the person asks
//! for no expiry. Rotate gives a key a new secret (shown once); the old secret
//! works until the end of the grace time.
//!
//! KAIROS-T-0536: each new key has a scope. With the two fields empty, the
//! key gets each capability but `purge` and `manage_structure`. A list of
//! capabilities or of boards limits the key. Each key shows its scope.
//!
//! The data layer is here: three calls on `/api/me/agent-keys`, through the
//! `crate::api` helpers (envelope-aware errors, the 401 hook).

use aurora_dark::components::{
    Alert, Button, Code, CopyButton, Divider, Empty, ErrorState, Group, Loading, PageHeader, Panel,
    Pill, Stack, Switch, Text, TextInput,
};
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;
use serde_json::json;

use super::admin::{MutationNotice, MutationOutcome, run_mutation};
use crate::auth::{Auth, use_auth};

/// mirror of: `kairos_client::types_service_accounts::ApiKey`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AgentKey {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    /// The key that a rotation made in place of this key (KAIROS-T-0370).
    #[serde(default)]
    pub replaced_by: Option<String>,
    /// The scope (KAIROS-T-0536): `kairos_client::types_service_accounts::KeyScope`
    /// as JSON, read by [`scope_text`] only. `None`: no scope, no limit.
    #[serde(default)]
    pub scope: Option<serde_json::Value>,
}

/// The scope of a key in words. Pure, host-tested.
pub(crate) fn scope_text(scope: Option<&serde_json::Value>) -> String {
    let Some(scope) = scope else {
        return "Scope: none.".to_string();
    };
    let names = |list: &serde_json::Value, each: fn(&serde_json::Value) -> Option<&str>| {
        list.as_array()
            .map(|items| items.iter().filter_map(each).collect::<Vec<_>>().join(", "))
    };
    let capabilities = names(&scope["capabilities"], |c| c.as_str())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "read-only".to_string());
    let boards = names(&scope["boards"], |b| {
        b["slug"].as_str().or_else(|| b["id"].as_str())
    })
    .unwrap_or_else(|| "each board".to_string());
    format!("Scope: {capabilities} on {boards}.")
}

/// The words of a comma list, trimmed, with no empty word.
fn words(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// mirror of: `kairos_client::types_service_accounts::ApiKeyRotated`
/// (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct AgentKeyRotated {
    name: String,
    key: String,
    replaced_key_expires_at: String,
}

/// mirror of: `kairos_client::types_service_accounts::ApiKeyList`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct AgentKeyList {
    items: Vec<AgentKey>,
}

/// mirror of: `kairos_client::types_service_accounts::ApiKeyCreated` (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct AgentKeyCreated {
    name: String,
    key: String,
}

const PATH: &str = "/api/me/agent-keys";

async fn list_agent_keys(auth: Auth) -> Result<Vec<AgentKey>, ApiError> {
    crate::api::get_json::<AgentKeyList>(auth, PATH)
        .await
        .map(|list| list.items)
}

/// The fields of the form of a new key.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NewKey {
    pub name: String,
    pub expires_at: Option<String>,
    pub no_expiry: bool,
    /// The capabilities, separated by commas. Empty: the default scope.
    pub capabilities: String,
    /// No capabilities: a read-only key.
    pub read_only: bool,
    /// The boards, separated by commas. Empty: each board.
    pub boards: String,
}

/// The body of a new key. Pure, host-tested.
pub(crate) fn create_body(new: &NewKey) -> serde_json::Value {
    let mut body = match (&new.expires_at, new.no_expiry) {
        (_, true) => json!({ "name": new.name, "no_expiry": true }),
        (Some(expires_at), false) => json!({ "name": new.name, "expires_at": expires_at }),
        (None, false) => json!({ "name": new.name }),
    };
    let capabilities = words(&new.capabilities);
    let boards = words(&new.boards);
    if new.read_only || !capabilities.is_empty() {
        body["scope"]["capabilities"] = json!(if new.read_only {
            Vec::new()
        } else {
            capabilities
        });
    }
    if !boards.is_empty() {
        body["scope"]["boards"] = json!(boards);
    }
    body
}

async fn create_agent_key(auth: Auth, new: &NewKey) -> Result<AgentKeyCreated, ApiError> {
    crate::api::post_json(auth, PATH, &create_body(new)).await
}

async fn rotate_agent_key(auth: Auth, key_id: &str) -> Result<AgentKeyRotated, ApiError> {
    crate::api::post_empty(auth, &format!("{PATH}/{key_id}/rotate")).await
}

async fn revoke_agent_key(auth: Auth, key_id: &str) -> Result<serde_json::Value, ApiError> {
    crate::api::delete_json(auth, &format!("{PATH}/{key_id}")).await
}

/// The date part of an RFC 3339 timestamp, or "never".
pub(crate) fn day(value: Option<&str>) -> String {
    value
        .map(|ts| ts.split('T').next().unwrap_or(ts).to_string())
        .unwrap_or_else(|| "never".to_string())
}

/// `2027-01-01T10:30:00.000000Z` → `2027-01-01 10:30 UTC`. Pure,
/// host-tested.
pub(crate) fn day_and_time(value: &str) -> String {
    match value.split_once('T') {
        Some((date, time)) => format!("{date} {} UTC", time.get(..5).unwrap_or(time)),
        None => value.to_string(),
    }
}

/// `/agent-keys`.
#[component]
pub fn AgentKeysPage() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);
    // The secret of the key made last. It shows once, on this page only.
    let made: RwSignal<Option<(String, String)>> = RwSignal::new(None);

    let keys = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        list_agent_keys(auth)
    });

    let name = RwSignal::new(String::new());
    let expires_at = RwSignal::new(String::new());
    let no_expiry = RwSignal::new(false);
    let capabilities = RwSignal::new(String::new());
    let read_only = RwSignal::new(false);
    let boards = RwSignal::new(String::new());
    // The end of the old secret after the last rotation.
    let grace_end: RwSignal<Option<String>> = RwSignal::new(None);

    let on_create = move |_| {
        if busy.get_untracked() {
            return;
        }
        let expiry = expires_at.get_untracked().trim().to_string();
        let new = NewKey {
            name: name.get_untracked().trim().to_string(),
            expires_at: (!expiry.is_empty()).then_some(expiry),
            no_expiry: no_expiry.get_untracked(),
            capabilities: capabilities.get_untracked(),
            read_only: read_only.get_untracked(),
            boards: boards.get_untracked(),
        };
        busy.set(true);
        made.set(None);
        grace_end.set(None);
        leptos::task::spawn_local(async move {
            let result = create_agent_key(auth, &new).await;
            busy.set(false);
            match result {
                Ok(created) => {
                    made.set(Some((created.name, created.key)));
                    outcome.set(None);
                    name.set(String::new());
                    expires_at.set(String::new());
                    no_expiry.set(false);
                    capabilities.set(String::new());
                    read_only.set(false);
                    boards.set(String::new());
                    reload.update(|n| *n += 1);
                }
                Err(error) => outcome.set(Some(Err(error))),
            }
        });
    };

    view! {
        <PageHeader title="Agent keys" sub="An agent key lets an agent act as you. The agent \
                                            gets your capabilities, and the history shows \
                                            your name."/>
        <Stack gap="md">
            <MutationNotice outcome/>
            {move || made.get().map(|(key_name, key)| {
                let copied = key.clone();
                view! {
                    <Alert title=format!("The key {key_name}") color=token::OK>
                        <Stack gap="xs">
                            <Text size="sm">
                                "Copy the key now. Kairos does not show it again."
                            </Text>
                            {move || grace_end.get().map(|end| view! {
                                <Text size="sm">
                                    {format!(
                                        "The old secret works until {}. Put the new key in \
                                         the settings of your agent before that time.",
                                        day_and_time(&end)
                                    )}
                                </Text>
                            })}
                            <Group gap="sm" wrap=true>
                                <Code>{key}</Code>
                                <CopyButton value=copied label="Copy key"/>
                            </Group>
                            <Text size="sm" dimmed=true>
                                "Put the key in the settings of your agent. For Claude Code, \
                                 set the environment variable KAIROS_MCP_KEY. Do not put the \
                                 key in a repository."
                            </Text>
                            <Group justify="end">
                                <Button variant="subtle" size="xs"
                                    on_click=Callback::new(move |_| made.set(None))>
                                    "Hide the key"
                                </Button>
                            </Group>
                        </Stack>
                    </Alert>
                }
            })}
            <Panel title="Make an agent key"
                caption="Give each agent its own key. Then you can revoke one key and keep \
                         the others. With no date, the key expires after the default time of \
                         this server (90 days unless the operator changed it).">
                <Group align="end" gap="sm" wrap=true>
                    <TextInput label="Name" value=name placeholder="laptop-claude-code"/>
                    <TextInput label="Expires at (optional)" value=expires_at
                        placeholder="2027-01-01T00:00:00Z"/>
                    <Switch checked=no_expiry label="No expiry"/>
                </Group>
                <Group align="end" gap="sm" wrap=true>
                    <TextInput label="Capabilities (optional)" value=capabilities
                        placeholder="manage_tasks, transition_items"/>
                    <TextInput label="Boards (optional)" value=boards placeholder="web, ops"/>
                    <Switch checked=read_only label="Read-only"/>
                    <Button on_click=Callback::new(on_create)>"Make key"</Button>
                </Group>

            </Panel>
            <Panel title="Your agent keys"
                caption="Kairos keeps the start of each key only. A revoked key stays in the \
                         list.">
                {move || match keys.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(list)) if list.is_empty() => view! {
                        <Empty message="You have no agent keys."/>
                    }.into_any(),
                    Some(Ok(list)) => list.into_iter().map(|key| {
                        let key_id = StoredValue::new(key.id.clone());
                        let revoked = key.revoked_at.is_some();
                        let replaced = key.replaced_by.is_some();
                        let revoked_name = key.name.clone();
                        let on_rotate = move |_| {
                            if busy.get_untracked() {
                                return;
                            }
                            let key_id = key_id.get_value();
                            busy.set(true);
                            made.set(None);
                            leptos::task::spawn_local(async move {
                                let result = rotate_agent_key(auth, &key_id).await;
                                busy.set(false);
                                match result {
                                    Ok(rotated) => {
                                        made.set(Some((rotated.name, rotated.key)));
                                        grace_end.set(Some(rotated.replaced_key_expires_at));
                                        outcome.set(None);
                                        reload.update(|n| *n += 1);
                                    }
                                    Err(error) => outcome.set(Some(Err(error))),
                                }
                            });
                        };
                        let on_revoke = move |_| {
                            let key_id = key_id.get_value();
                            let revoked_name = revoked_name.clone();
                            run_mutation(
                                busy, outcome, reload,
                                format!("Kairos revoked the key {revoked_name}."),
                                async move { revoke_agent_key(auth, &key_id).await.map(|_| ()) },
                            );
                        };
                        view! {
                            <div data-testid="agent-key">
                            <Stack gap="xs">
                                <Group justify="between" wrap=true>
                                    <Group gap="sm" wrap=true>
                                        <Text bright=true>{key.name.clone()}</Text>
                                        <Code>{key.prefix.clone()}</Code>
                                        {if revoked {
                                            view! { <Pill color=token::BAD>"revoked"</Pill> }.into_any()
                                        } else if replaced {
                                            view! { <Pill color=token::GOLD>"rotated"</Pill> }.into_any()
                                        } else {
                                            view! { <Pill color=token::OK>"active"</Pill> }.into_any()
                                        }}
                                    </Group>
                                    {(!revoked).then(|| view! {
                                        <Group gap="sm">
                                            {(!replaced).then(|| view! {
                                                <Button variant="default" size="xs"
                                                    on_click=Callback::new(on_rotate)>
                                                    "Rotate"
                                                </Button>
                                            })}
                                            <Button variant="default" size="xs" bad=true
                                                on_click=Callback::new(on_revoke)>
                                                "Revoke"
                                            </Button>
                                        </Group>
                                    })}
                                </Group>
                                <Text dimmed=true size="sm">
                                    {format!(
                                        "Made {}. Expires {}. Last used {}. {}",
                                        day(Some(&key.created_at)),
                                        day(key.expires_at.as_deref()),
                                        day(key.last_used_at.as_deref()),
                                        scope_text(key.scope.as_ref()),
                                    )}
                                </Text>
                                <Divider/>
                            </Stack>
                            </div>
                        }
                    }).collect_view().into_any(),
                }}
            </Panel>
        </Stack>
    }
}

#[cfg(test)]
mod tests {
    use super::{NewKey, create_body, day, day_and_time, scope_text};

    fn key(name: &str) -> NewKey {
        NewKey {
            name: name.into(),
            ..NewKey::default()
        }
    }

    #[test]
    fn the_body_asks_for_no_expiry_or_a_date() {
        assert_eq!(
            create_body(&NewKey {
                no_expiry: true,
                ..key("k")
            }),
            serde_json::json!({"name": "k", "no_expiry": true})
        );
        assert_eq!(
            create_body(&NewKey {
                expires_at: Some("2027-01-01T00:00:00Z".into()),
                ..key("k")
            }),
            serde_json::json!({"name": "k", "expires_at": "2027-01-01T00:00:00Z"})
        );
        assert_eq!(create_body(&key("k")), serde_json::json!({"name": "k"}));
    }

    #[test]
    fn the_body_has_the_scope_only_when_the_form_names_one() {
        assert_eq!(
            create_body(&NewKey {
                capabilities: " manage_tasks, ,transition_items ".into(),
                boards: "web".into(),
                ..key("k")
            }),
            serde_json::json!({"name": "k", "scope": {
                "capabilities": ["manage_tasks", "transition_items"],
                "boards": ["web"],
            }})
        );
        assert_eq!(
            create_body(&NewKey {
                capabilities: "purge".into(),
                read_only: true,
                ..key("k")
            }),
            serde_json::json!({"name": "k", "scope": {"capabilities": []}})
        );
    }

    #[test]
    fn the_scope_reads_as_words() {
        assert_eq!(scope_text(None), "Scope: none.");
        let scope = serde_json::json!({
            "capabilities": ["manage_tasks"],
            "boards": [{"id": "b1", "slug": "web"}],
        });
        assert_eq!(scope_text(Some(&scope)), "Scope: manage_tasks on web.");
        let read_only = serde_json::json!({"capabilities": [], "boards": null});
        assert_eq!(
            scope_text(Some(&read_only)),
            "Scope: read-only on each board."
        );
    }

    #[test]
    fn the_end_of_the_grace_time_reads_as_a_day_and_a_time() {
        assert_eq!(
            day_and_time("2027-01-01T10:30:00.000000Z"),
            "2027-01-01 10:30 UTC"
        );
    }

    #[test]
    fn day_shows_the_date_or_never() {
        assert_eq!(day(Some("2027-01-01T00:00:00.000000Z")), "2027-01-01");
        assert_eq!(day(None), "never");
    }
}
