//! The section "Agent keys of the people" of `/admin/members`
//! (KAIROS-T-0388): an organization admin reads the agent keys of each
//! person (the owner, the name, the prefix, the dates, revoked or not) and
//! revokes one, after a confirmation. Kairos tells the owner.
//!
//! The data layer is here: `GET /api/org/agent-keys` (each page) and
//! `DELETE /api/org/agent-keys/{key_id}`, through the `crate::api` helpers.

use aurora_dark::components::{
    Button, Code, Divider, Empty, ErrorState, Group, Loading, Panel, Pill, Stack, Text,
};
use aurora_dark::frame::ConfirmDialog;
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::Deserialize;

use super::{MutationNotice, MutationOutcome, run_mutation};
use crate::auth::{Auth, use_auth};

/// mirror of: `kairos_client::types_service_accounts::OrgAgentKey`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct OrgAgentKey {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub owner_id: String,
    pub owner_email: String,
    pub owner_name: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    #[serde(default)]
    pub replaced_by: Option<String>,
}

const PATH: &str = "/api/org/agent-keys";

async fn list_org_agent_keys(auth: Auth) -> Result<Vec<OrgAgentKey>, ApiError> {
    crate::api::get_all(auth, PATH).await
}

async fn revoke_org_agent_key(auth: Auth, key_id: &str) -> Result<serde_json::Value, ApiError> {
    crate::api::delete_json(auth, &format!("{PATH}/{key_id}")).await
}

/// The date part of an RFC 3339 timestamp, or `none`. Pure, host-tested.
fn day(value: Option<&str>, none: &str) -> String {
    value
        .map(|ts| ts.split('T').next().unwrap_or(ts).to_string())
        .unwrap_or_else(|| none.to_string())
}

/// The line of dates of a key. Pure, host-tested.
fn dates_line(key: &OrgAgentKey) -> String {
    let mut line = format!(
        "Made {}. Expires {}. Last used {}.",
        day(Some(&key.created_at), "never"),
        day(key.expires_at.as_deref(), "never"),
        day(key.last_used_at.as_deref(), "never"),
    );
    if let Some(revoked) = key.revoked_at.as_deref() {
        line.push_str(&format!(" Revoked {}.", day(Some(revoked), "")));
    }
    line
}

/// The panel of the agent keys of each person, with Revoke. `reload` is
/// the reload of the page: a removal of a member reloads the keys too.
#[component]
pub fn OrgAgentKeysPanel(reload: RwSignal<u32>) -> impl IntoView {
    let auth = use_auth();
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);
    let confirm_open = RwSignal::new(false);
    // The key to revoke: (id, name, owner email).
    let target: RwSignal<Option<(String, String, String)>> = RwSignal::new(None);

    let keys = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        list_org_agent_keys(auth)
    });

    let on_confirm = Callback::new(move |()| {
        let Some((key_id, name, owner)) = target.get_untracked() else {
            return;
        };
        confirm_open.set(false);
        run_mutation(
            busy,
            outcome,
            reload,
            format!("Kairos revoked the key {name} of {owner}, and told {owner}."),
            async move { revoke_org_agent_key(auth, &key_id).await.map(|_| ()) },
        );
    });

    view! {
        <Panel title="Agent keys of the people"
            caption="An agent key acts as its owner. A key can have no expiry. Revoke a key \
                     that must stop now. Kairos tells the owner. When you remove a person \
                     from the organization, Kairos revokes each key of the person.">
            <Stack gap="sm">
                <MutationNotice outcome/>
                {move || match keys.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(list)) if list.is_empty() => view! {
                        <Empty message="No person has an agent key."/>
                    }.into_any(),
                    Some(Ok(list)) => list.into_iter().map(|key| {
                        let revoked = key.revoked_at.is_some();
                        let ask = (key.id.clone(), key.name.clone(), key.owner_email.clone());
                        let on_revoke = move |_| {
                            target.set(Some(ask.clone()));
                            confirm_open.set(true);
                        };
                        let dates = dates_line(&key);
                        view! {
                            <div data-testid="org-agent-key">
                            <Stack gap="xs">
                                <Group justify="between" wrap=true>
                                    <Group gap="sm" wrap=true>
                                        <Text bright=true>{key.owner_name.clone()}</Text>
                                        <Text dimmed=true size="sm">{key.owner_email.clone()}</Text>
                                        <Text>{key.name.clone()}</Text>
                                        <Code>{key.prefix.clone()}</Code>
                                        {if revoked {
                                            view! { <Pill color=token::BAD>"revoked"</Pill> }.into_any()
                                        } else {
                                            view! { <Pill color=token::OK>"active"</Pill> }.into_any()
                                        }}
                                    </Group>
                                    {(!revoked).then(|| view! {
                                        <Button variant="default" size="xs" bad=true
                                            disabled=busy
                                            on_click=Callback::new(on_revoke)>
                                            "Revoke"
                                        </Button>
                                    })}
                                </Group>
                                <Text dimmed=true size="sm">{dates}</Text>
                                <Divider/>
                            </Stack>
                            </div>
                        }
                    }).collect_view().into_any(),
                }}
            </Stack>
        </Panel>
        <ConfirmDialog
            open=confirm_open
            title="Revoke this agent key?"
            confirm_label="Revoke"
            busy=busy
            on_confirm=on_confirm
        >
            <Text size="sm">
                {move || target.get().map(|(_, name, owner)| format!(
                    "The key {name} of {owner} stops now. An agent that uses the key gets \
                     401. Kairos tells {owner}. You cannot undo a revoke."
                ))}
            </Text>
        </ConfirmDialog>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(revoked_at: Option<&str>, expires_at: Option<&str>) -> OrgAgentKey {
        OrgAgentKey {
            id: "k".into(),
            name: "laptop".into(),
            prefix: "kairos_sk_acme_ab12".into(),
            owner_id: "u".into(),
            owner_email: "robin@example.com".into(),
            owner_name: "Robin".into(),
            created_at: "2026-10-01T09:00:00.000000Z".into(),
            expires_at: expires_at.map(str::to_string),
            last_used_at: None,
            revoked_at: revoked_at.map(str::to_string),
            replaced_by: None,
        }
    }

    #[test]
    fn the_dates_say_never_for_no_expiry_and_name_the_revoke() {
        assert_eq!(
            dates_line(&key(None, None)),
            "Made 2026-10-01. Expires never. Last used never."
        );
        assert_eq!(
            dates_line(&key(
                Some("2026-10-05T10:00:00.000000Z"),
                Some("2026-12-30T09:00:00.000000Z")
            )),
            "Made 2026-10-01. Expires 2026-12-30. Last used never. Revoked 2026-10-05."
        );
    }

    #[test]
    fn the_mirror_reads_the_wire_shape() {
        let wire = serde_json::json!({
            "id": "k", "name": "laptop", "prefix": "kairos_sk_acme_ab12",
            "owner_id": "u", "owner_email": "robin@example.com", "owner_name": "Robin",
            "created_at": "2026-10-01T09:00:00.000000Z", "expires_at": null,
            "last_used_at": null, "revoked_at": null, "replaced_by": null
        });
        let parsed: OrgAgentKey = serde_json::from_value(wire).expect("parses");
        assert_eq!(parsed, key(None, None));
    }
}
