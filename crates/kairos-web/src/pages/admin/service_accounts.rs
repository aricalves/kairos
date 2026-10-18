//! The section "Service accounts" of `/admin/members` (KAIROS-T-0587): an
//! organization admin makes and deletes a service account, and makes,
//! rotates and revokes its keys. A key acts as the service account, not as
//! a person (KAIROS-A-0017).
//!
//! A made or rotated key shows once, with a copy button and the warning
//! that Kairos does not show it again. A delete and a revoke ask first.
//!
//! The data layer is here: `/api/service-accounts`, `/{id}`, `/{id}/keys`,
//! `/{id}/keys/{key_id}` and `/{id}/keys/{key_id}/rotate`, through the
//! `crate::api` helpers. The list routes have no pages (`{items, total}`).

use aurora_dark::components::{
    Alert, Button, Code, CopyButton, Divider, Empty, ErrorState, Group, Loading, Panel, Pill,
    Stack, Switch, Text, TextInput,
};
use aurora_dark::frame::ConfirmDialog;
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;
use serde_json::json;

use super::{MutationNotice, MutationOutcome, run_mutation};
use crate::auth::{Auth, use_auth};
use crate::pages::agent_keys::{AgentKey as ApiKey, create_body, day, day_and_time};

/// mirror of: `kairos_client::types_service_accounts::ServiceAccount`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ServiceAccount {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// mirror of: `kairos_client::types_service_accounts::ApiKeyCreated`
/// (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct KeyCreated {
    name: String,
    key: String,
}

/// mirror of: `kairos_client::types_service_accounts::ApiKeyRotated`
/// (partial).
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct KeyRotated {
    name: String,
    key: String,
    replaced_key_expires_at: String,
}

const PATH: &str = "/api/service-accounts";

async fn list_accounts(auth: Auth) -> Result<Vec<ServiceAccount>, ApiError> {
    crate::api::get_json::<crate::api::Page<ServiceAccount>>(auth, PATH)
        .await
        .map(|page| page.items)
}

async fn create_account(auth: Auth, name: &str) -> Result<ServiceAccount, ApiError> {
    crate::api::post_json(auth, PATH, &json!({ "name": name })).await
}

async fn delete_account(auth: Auth, account_id: &str) -> Result<serde_json::Value, ApiError> {
    crate::api::delete_json(auth, &format!("{PATH}/{account_id}")).await
}

async fn list_keys(auth: Auth, account_id: &str) -> Result<Vec<ApiKey>, ApiError> {
    crate::api::get_json::<crate::api::Page<ApiKey>>(auth, &format!("{PATH}/{account_id}/keys"))
        .await
        .map(|page| page.items)
}

async fn create_key(
    auth: Auth,
    account_id: &str,
    name: &str,
    expires_at: Option<&str>,
    no_expiry: bool,
) -> Result<KeyCreated, ApiError> {
    crate::api::post_json(
        auth,
        &format!("{PATH}/{account_id}/keys"),
        &create_body(name, expires_at, no_expiry),
    )
    .await
}

async fn rotate_key(auth: Auth, account_id: &str, key_id: &str) -> Result<KeyRotated, ApiError> {
    crate::api::post_empty(auth, &format!("{PATH}/{account_id}/keys/{key_id}/rotate")).await
}

async fn revoke_key(
    auth: Auth,
    account_id: &str,
    key_id: &str,
) -> Result<serde_json::Value, ApiError> {
    crate::api::delete_json(auth, &format!("{PATH}/{account_id}/keys/{key_id}")).await
}

/// A key that shows once: after a create or a rotation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShownKey {
    account: String,
    name: String,
    key: String,
    /// The end of the old secret, after a rotation.
    grace_end: Option<String>,
}

/// What a delete or a revoke asks first.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ask {
    DeleteAccount {
        account_id: String,
        account: String,
    },
    RevokeKey {
        account_id: String,
        account: String,
        key_id: String,
        key: String,
    },
}

/// The title and the text of the confirmation. Pure, host-tested.
pub(crate) fn ask_words(ask: &Ask) -> (String, String) {
    match ask {
        Ask::DeleteAccount { account, .. } => (
            format!("Delete the service account {account}?"),
            format!(
                "Kairos deletes {account} and each key of it. An agent that uses a key of \
                 {account} gets 401. You cannot undo a delete."
            ),
        ),
        Ask::RevokeKey { account, key, .. } => (
            format!("Revoke the key {key}?"),
            format!(
                "The key {key} of {account} stops now. An agent that uses the key gets 401. \
                 You cannot undo a revoke."
            ),
        ),
    }
}

/// What the page says after a delete or a revoke. Pure, host-tested.
pub(crate) fn ask_done(ask: &Ask) -> String {
    match ask {
        Ask::DeleteAccount { account, .. } => {
            format!("Kairos deleted the service account {account} and each key of it.")
        }
        Ask::RevokeKey { account, key, .. } => {
            format!("Kairos revoked the key {key} of {account}.")
        }
    }
}

/// The state of a key: `revoked`, `rotated` or `active`. Pure, host-tested.
pub(crate) fn key_state(key: &ApiKey) -> &'static str {
    if key.revoked_at.is_some() {
        "revoked"
    } else if key.replaced_by.is_some() {
        "rotated"
    } else {
        "active"
    }
}

/// The line of dates of a key. Pure, host-tested.
pub(crate) fn key_dates(key: &ApiKey) -> String {
    let mut line = format!(
        "Made {}. Expires {}. Last used {}.",
        day(Some(&key.created_at)),
        day(key.expires_at.as_deref()),
        day(key.last_used_at.as_deref()),
    );
    if let Some(revoked) = key.revoked_at.as_deref() {
        line.push_str(&format!(" Revoked {}.", day(Some(revoked))));
    }
    line
}

/// The warning of a key that shows once. Pure, host-tested.
pub(crate) fn shown_warning(grace_end: Option<&str>) -> Vec<String> {
    let mut lines = vec!["Copy the key now. Kairos does not show it again.".to_string()];
    if let Some(end) = grace_end {
        lines.push(format!(
            "The old secret works until {}. Put the new key in the agent before that time.",
            day_and_time(end)
        ));
    }
    lines
}

/// The panel of the service accounts. `reload` is the reload of the page.
#[component]
pub fn ServiceAccountsPanel(reload: RwSignal<u32>) -> impl IntoView {
    let auth = use_auth();
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);
    let shown: RwSignal<Option<ShownKey>> = RwSignal::new(None);
    let confirm_open = RwSignal::new(false);
    let ask: RwSignal<Option<Ask>> = RwSignal::new(None);

    let accounts = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        list_accounts(auth)
    });

    let new_name = RwSignal::new(String::new());
    let on_create = Callback::new(move |_| {
        let name = new_name.get_untracked().trim().to_string();
        new_name.set(String::new());
        run_mutation(
            busy,
            outcome,
            reload,
            format!("Kairos made the service account {name}. Make a key for it below."),
            async move { create_account(auth, &name).await.map(|_| ()) },
        );
    });

    let on_confirm = Callback::new(move |()| {
        let Some(target) = ask.get_untracked() else {
            return;
        };
        confirm_open.set(false);
        let done = ask_done(&target);
        run_mutation(busy, outcome, reload, done, async move {
            match target {
                Ask::DeleteAccount { account_id, .. } => {
                    delete_account(auth, &account_id).await.map(|_| ())
                }
                Ask::RevokeKey {
                    account_id, key_id, ..
                } => revoke_key(auth, &account_id, &key_id).await.map(|_| ()),
            }
        });
    });

    view! {
        <Panel title="Service accounts"
            caption="A service account is an identity for a machine, not for a person. Its \
                     keys act as the service account. Give each machine its own key.">
            <Stack gap="sm">
                <MutationNotice outcome/>
                {move || shown.get().map(|made| {
                    let copied = made.key.clone();
                    let warning = shown_warning(made.grace_end.as_deref());
                    view! {
                        <Alert title=format!("The key {} of {}", made.name, made.account)
                            color=token::OK>
                            <Stack gap="xs" attr:data-testid="service-key-secret">
                                {warning.into_iter().map(|line| view! {
                                    <Text size="sm">{line}</Text>
                                }).collect_view()}
                                <Group gap="sm" wrap=true>
                                    <Code>{made.key}</Code>
                                    <CopyButton value=copied label="Copy key"/>
                                </Group>
                                <Text size="sm" dimmed=true>
                                    "Do not put the key in a repository."
                                </Text>
                                <Group justify="end">
                                    <Button variant="subtle" size="xs"
                                        on_click=Callback::new(move |_| shown.set(None))>
                                        "Hide the key"
                                    </Button>
                                </Group>
                            </Stack>
                        </Alert>
                    }
                })}
                <Group align="end" gap="sm" wrap=true>
                    <TextInput label="Name" value=new_name placeholder="ci-runner"/>
                    <Button disabled=busy on_click=on_create>"Make service account"</Button>
                </Group>
                <Divider/>
                {move || match accounts.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(list)) if list.is_empty() => view! {
                        <Empty message="The organization has no service accounts."/>
                    }.into_any(),
                    Some(Ok(list)) => list.into_iter().map(|account| view! {
                        <ServiceAccountRow account busy outcome reload shown ask confirm_open/>
                    }).collect_view().into_any(),
                }}
            </Stack>
        </Panel>
        <ConfirmDialog
            open=confirm_open
            title=Signal::derive(move || {
                ask.get().map(|target| ask_words(&target).0).unwrap_or_default()
            })
            confirm_label="Confirm"
            busy=busy
            on_confirm=on_confirm
        >
            <Text size="sm">
                {move || ask.get().map(|target| ask_words(&target).1)}
            </Text>
        </ConfirmDialog>
    }
}

/// One service account: its keys, the form of a new key, and Delete.
#[component]
fn ServiceAccountRow(
    account: ServiceAccount,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
    shown: RwSignal<Option<ShownKey>>,
    ask: RwSignal<Option<Ask>>,
    confirm_open: RwSignal<bool>,
) -> impl IntoView {
    let auth = use_auth();
    let account_id = StoredValue::new(account.id.clone());
    let account_name = StoredValue::new(account.name.clone());

    let keys = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        let id = account_id.get_value();
        async move { list_keys(auth, &id).await }
    });

    let key_name = RwSignal::new(String::new());
    let expires_at = RwSignal::new(String::new());
    let no_expiry = RwSignal::new(false);

    let on_make_key = Callback::new(move |_| {
        if busy.get_untracked() {
            return;
        }
        let id = account_id.get_value();
        let name = key_name.get_untracked().trim().to_string();
        let expiry = expires_at.get_untracked().trim().to_string();
        let forever = no_expiry.get_untracked();
        busy.set(true);
        shown.set(None);
        leptos::task::spawn_local(async move {
            let expiry = (!expiry.is_empty()).then_some(expiry);
            let result = create_key(auth, &id, &name, expiry.as_deref(), forever).await;
            busy.set(false);
            match result {
                Ok(created) => {
                    shown.set(Some(ShownKey {
                        account: account_name.get_value(),
                        name: created.name,
                        key: created.key,
                        grace_end: None,
                    }));
                    outcome.set(None);
                    key_name.set(String::new());
                    expires_at.set(String::new());
                    no_expiry.set(false);
                    reload.update(|n| *n += 1);
                }
                Err(error) => outcome.set(Some(Err(error))),
            }
        });
    });

    let on_delete = Callback::new(move |_| {
        ask.set(Some(Ask::DeleteAccount {
            account_id: account_id.get_value(),
            account: account_name.get_value(),
        }));
        confirm_open.set(true);
    });

    let row_name = account.name.clone();
    view! {
        <div data-testid="service-account" data-account=row_name>
        <Stack gap="xs">
            <Group justify="between" wrap=true>
                <Group gap="sm">
                    <Text bright=true>{account.name.clone()}</Text>
                    <Text dimmed=true size="sm">
                        {format!("Made {}.", day(Some(&account.created_at)))}
                    </Text>
                </Group>
                <Button variant="default" size="xs" bad=true disabled=busy on_click=on_delete>
                    "Delete"
                </Button>
            </Group>
            {move || match keys.get() {
                None => view! { <Loading/> }.into_any(),
                Some(Err(error)) => view! { <ErrorState error/> }.into_any(),
                Some(Ok(list)) if list.is_empty() => view! {
                    <Text dimmed=true size="sm">"The service account has no keys."</Text>
                }.into_any(),
                Some(Ok(list)) => list.into_iter().map(|key| {
                    let state = key_state(&key);
                    let key_attr = key.name.clone();
                    let dates = key_dates(&key);
                    let key_id = StoredValue::new(key.id.clone());
                    let name = StoredValue::new(key.name.clone());
                    let on_rotate = Callback::new(move |_| {
                        if busy.get_untracked() {
                            return;
                        }
                        let id = account_id.get_value();
                        let key_id = key_id.get_value();
                        busy.set(true);
                        shown.set(None);
                        leptos::task::spawn_local(async move {
                            let result = rotate_key(auth, &id, &key_id).await;
                            busy.set(false);
                            match result {
                                Ok(rotated) => {
                                    shown.set(Some(ShownKey {
                                        account: account_name.get_value(),
                                        name: rotated.name,
                                        key: rotated.key,
                                        grace_end: Some(rotated.replaced_key_expires_at),
                                    }));
                                    outcome.set(None);
                                    reload.update(|n| *n += 1);
                                }
                                Err(error) => outcome.set(Some(Err(error))),
                            }
                        });
                    });
                    let on_revoke = Callback::new(move |_| {
                        ask.set(Some(Ask::RevokeKey {
                            account_id: account_id.get_value(),
                            account: account_name.get_value(),
                            key_id: key_id.get_value(),
                            key: name.get_value(),
                        }));
                        confirm_open.set(true);
                    });
                    let color = match state {
                        "revoked" => token::BAD,
                        "rotated" => token::GOLD,
                        _ => token::OK,
                    };
                    view! {
                        <div data-testid="service-key" data-key=key_attr>
                        <Stack gap="xs">
                            <Group justify="between" wrap=true>
                                <Group gap="sm" wrap=true>
                                    <Text>{key.name.clone()}</Text>
                                    <Code>{key.prefix.clone()}</Code>
                                    <Pill color=color>{state}</Pill>
                                </Group>
                                <Group gap="xs">
                                    {(state == "active").then(|| view! {
                                        <Button variant="default" size="xs" disabled=busy
                                            on_click=on_rotate>
                                            "Rotate"
                                        </Button>
                                    })}
                                    {(state != "revoked").then(|| view! {
                                        <Button variant="default" size="xs" bad=true
                                            disabled=busy on_click=on_revoke>
                                            "Revoke"
                                        </Button>
                                    })}
                                </Group>
                            </Group>
                            <Text dimmed=true size="sm">{dates}</Text>
                        </Stack>
                        </div>
                    }
                }).collect_view().into_any(),
            }}
            <Group align="end" gap="sm" wrap=true>
                <TextInput label="Key name" value=key_name placeholder="nightly-import"/>
                <TextInput label="Expires at (optional)" value=expires_at
                    placeholder="2027-01-01T00:00:00Z"/>
                <Switch checked=no_expiry label="No expiry"/>
                <Button size="xs" disabled=busy on_click=on_make_key>"Make key"</Button>
            </Group>
            <Text dimmed=true size="xs">
                "With no date, the key expires after the default time of this server."
            </Text>
            <Divider/>
        </Stack>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(revoked_at: Option<&str>, replaced_by: Option<&str>) -> ApiKey {
        ApiKey {
            id: "k".into(),
            name: "nightly".into(),
            prefix: "kairos_sk_acme_ab12".into(),
            created_at: "2026-10-01T09:00:00.000000Z".into(),
            expires_at: None,
            last_used_at: None,
            revoked_at: revoked_at.map(str::to_string),
            replaced_by: replaced_by.map(str::to_string),
        }
    }

    #[test]
    fn a_key_is_active_rotated_or_revoked() {
        assert_eq!(key_state(&key(None, None)), "active");
        assert_eq!(key_state(&key(None, Some("k2"))), "rotated");
        assert_eq!(
            key_state(&key(Some("2026-10-05T10:00:00Z"), Some("k2"))),
            "revoked"
        );
    }

    #[test]
    fn the_dates_name_the_revoke() {
        assert_eq!(
            key_dates(&key(None, None)),
            "Made 2026-10-01. Expires never. Last used never."
        );
        assert_eq!(
            key_dates(&key(Some("2026-10-05T10:00:00.000000Z"), None)),
            "Made 2026-10-01. Expires never. Last used never. Revoked 2026-10-05."
        );
    }

    #[test]
    fn a_shown_key_warns_once_and_names_the_grace_time() {
        assert_eq!(
            shown_warning(None),
            vec!["Copy the key now. Kairos does not show it again.".to_string()]
        );
        let rotated = shown_warning(Some("2027-01-01T10:30:00.000000Z"));
        assert_eq!(rotated.len(), 2);
        assert_eq!(
            rotated[1],
            "The old secret works until 2027-01-01 10:30 UTC. Put the new key in the agent \
             before that time."
        );
    }

    #[test]
    fn each_ask_names_what_it_changes() {
        let delete = Ask::DeleteAccount {
            account_id: "sa".into(),
            account: "ci-runner".into(),
        };
        let (title, text) = ask_words(&delete);
        assert_eq!(title, "Delete the service account ci-runner?");
        assert!(text.contains("each key of it"));
        assert_eq!(
            ask_done(&delete),
            "Kairos deleted the service account ci-runner and each key of it."
        );
        let revoke = Ask::RevokeKey {
            account_id: "sa".into(),
            account: "ci-runner".into(),
            key_id: "k".into(),
            key: "nightly".into(),
        };
        assert_eq!(ask_words(&revoke).0, "Revoke the key nightly?");
        assert_eq!(
            ask_done(&revoke),
            "Kairos revoked the key nightly of ci-runner."
        );
    }

    #[test]
    fn the_mirrors_read_the_wire_shapes() {
        let list: crate::api::Page<ServiceAccount> = serde_json::from_value(json!({
            "items": [{ "id": "sa", "name": "ci-runner", "created_at": "2026-10-01T09:00:00Z" }],
            "total": 1
        }))
        .expect("parses");
        assert_eq!(list.items[0].name, "ci-runner");
        let rotated: KeyRotated = serde_json::from_value(json!({
            "id": "k2", "name": "nightly", "key": "kairos_sk_acme_secret",
            "prefix": "kairos_sk_acme_se", "created_at": "2026-10-02T09:00:00Z",
            "expires_at": null, "replaced_key_id": "k",
            "replaced_key_expires_at": "2026-10-03T09:00:00Z"
        }))
        .expect("parses");
        assert_eq!(rotated.replaced_key_expires_at, "2026-10-03T09:00:00Z");
        let created: KeyCreated = serde_json::from_value(json!({
            "id": "k", "name": "nightly", "key": "kairos_sk_acme_secret",
            "prefix": "kairos_sk_acme_se", "created_at": "2026-10-02T09:00:00Z",
            "expires_at": null
        }))
        .expect("parses");
        assert_eq!(created.key, "kairos_sk_acme_secret");
    }
}
