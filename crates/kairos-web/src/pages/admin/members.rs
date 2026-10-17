//! `/admin/members` — organization membership (KAIROS-T-0043): list,
//! add-by-email, role changes, removal. The server guards the org's last
//! admin (422 `LAST_ADMIN`) and unknown emails (users are JIT-provisioned
//! at first login, so adding one that never logged in is a 404 with that
//! guidance) — both render through [`super::MutationNotice`].
//!
//! KAIROS-T-0388: the section of the agent keys of each person
//! ([`super::agent_keys::OrgAgentKeysPanel`]). A removal revokes the keys
//! of the person.
//!
//! KAIROS-T-0413: the name of each member links to the grants page of the
//! person (`/admin/members/:user`). A holder of `administer_members` on a
//! board who is not an org admin also opens this page: it shows the list
//! and the links only. The controls of the org admin (role, removal, add,
//! agent keys) are hidden for that viewer; the server refuses them anyway.

use aurora_dark::components::{
    Anchor, Button, Divider, Empty, ErrorState, Group, Loading, PageHeader, Panel, Pill, Select,
    Stack, Text, TextInput,
};
use aurora_dark::tokens::token;
use leptos::prelude::*;

use super::api;
use super::gating;
use super::{MutationNotice, MutationOutcome, run_mutation};
use crate::auth::use_auth;

/// `/admin/members`.
#[component]
pub fn AdminMembersPage() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);

    let members = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        api::list_org_members(auth)
    });
    let whoami = LocalResource::new(move || {
        auth.track_session();
        crate::api::whoami(auth)
    });
    // The org-admin controls show only when whoami says so.
    let viewer_is_admin = move || matches!(whoami.get(), Some(Ok(me)) if gating::is_org_admin(&me));

    let add_email = RwSignal::new(String::new());
    let add_role = RwSignal::new("member".to_string());

    let on_add = move |_| {
        let email = add_email.get_untracked();
        let role = add_role.get_untracked();
        run_mutation(
            busy,
            outcome,
            reload,
            format!("Kairos added {email} as {role}."),
            async move { api::add_org_member(auth, &email, &role).await.map(|_| ()) },
        );
    };

    view! {
        <PageHeader title="Organization members" sub="The members of the organization, \
                                                      and its admins."/>
        <Stack gap="md">
            <MutationNotice outcome/>
            <Panel title="Members"
                caption="Click a name to set the grants of the person on many boards. An \
                         organization must have one admin or more (LAST_ADMIN).">
                {move || match members.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(list)) if list.is_empty() => view! {
                        <Empty message="The list of members is empty. This is a fault, \
                                        because you are a member."/>
                    }.into_any(),
                    Some(Ok(list)) => list.into_iter().map(|member| {
                        let grants_href = format!("/admin/members/{}", member.user_id);
                        let user_id = StoredValue::new(member.user_id);
                        let is_admin = member.role == "admin";
                        let role_color = if is_admin { token::GOLD } else { token::ICE };
                        let email = member.email.clone();
                        let toggled_email = email.clone();
                        let removed_email = email.clone();
                        let on_toggle_role = move |_| {
                            let user_id = user_id.get_value();
                            let new_role = if is_admin { "member" } else { "admin" };
                            let toggled_email = toggled_email.clone();
                            run_mutation(
                                busy, outcome, reload,
                                format!("The role of {toggled_email} is {new_role}."),
                                async move {
                                    api::set_org_member_role(auth, &user_id, new_role)
                                        .await
                                        .map(|_| ())
                                },
                            );
                        };
                        let on_remove = move |_| {
                            let user_id = user_id.get_value();
                            let removed_email = removed_email.clone();
                            run_mutation(
                                busy, outcome, reload,
                                format!(
                                    "Kairos removed {removed_email} from the organization, \
                                     and revoked each agent key of {removed_email}."
                                ),
                                async move {
                                    api::remove_org_member(auth, &user_id).await.map(|_| ())
                                },
                            );
                        };
                        let on_toggle_role = Callback::new(on_toggle_role);
                        let on_remove = Callback::new(on_remove);
                        view! {
                            <Stack gap="xs">
                                <Group justify="between" wrap=true>
                                    <Group gap="sm">
                                        <Anchor href=grants_href>
                                            {member.display_name.clone()}
                                        </Anchor>
                                        <Text dimmed=true size="sm">{email.clone()}</Text>
                                        <Pill color=role_color>
                                            {member.role.clone()}
                                        </Pill>
                                    </Group>
                                    <Show when=viewer_is_admin>
                                    <Group gap="xs">
                                        <Button variant="default" size="xs"
                                            on_click=on_toggle_role>
                                            {if is_admin { "Make member" } else { "Make admin" }}
                                        </Button>
                                        <Button variant="default" size="xs" bad=true
                                            on_click=on_remove>
                                            "Remove"
                                        </Button>
                                    </Group>
                                    </Show>
                                </Group>
                                <Divider/>
                            </Stack>
                        }
                    }).collect_view().into_any(),
                }}
            </Panel>
            <Show when=viewer_is_admin>
            <Panel title="Add member by email"
                caption="Kairos makes a user at the first login. You cannot add an email \
                         before its first login.">
                <Group align="end" gap="sm" wrap=true>
                    <TextInput label="Email" value=add_email placeholder="someone@example.com"/>
                    <Select label="Role"
                        options=vec!["member".to_string(), "admin".to_string()]
                        value=add_role/>
                    <Button on_click=Callback::new(on_add)>"Add member"</Button>
                </Group>
            </Panel>
            <super::agent_keys::OrgAgentKeysPanel reload/>
            // KAIROS-T-0587: the service accounts and their keys.
            <super::service_accounts::ServiceAccountsPanel reload/>
            </Show>
        </Stack>
    }
}
