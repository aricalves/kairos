//! The open branches and pull requests of a repository that no item is
//! linked to (KAIROS-T-0480, COLLIERY-I-0623). The forge webhook records
//! each one that the forge filter admits, with or without a short code.
//! The admin directory and the team page show the list for each
//! repository; it is read when the user opens it.

use aurora_dark::components::{
    Anchor, Button, Empty, ErrorState, Group, Loading, Pill, Stack, Text,
};
use leptos::prelude::*;

use super::api::{self, ForgeRef};
use crate::auth::use_auth;
use crate::pages::teams::link_state_color;

/// One row: state, label (a link to the forge) and author.
#[component]
pub fn ForgeRefRow(forge_ref: ForgeRef) -> impl IntoView {
    let label = forge_ref.label();
    let author = if forge_ref.author.is_empty() {
        String::new()
    } else {
        format!("by {}", forge_ref.author)
    };
    let repo = format!("{} · {}", forge_ref.forge, forge_ref.repo_full_name);
    view! {
        <Group justify="between" wrap=true attr:data-ref=forge_ref.external_id.clone()>
            <Group gap="sm" wrap=true>
                <Pill color=link_state_color(&forge_ref.state)>{forge_ref.state.clone()}</Pill>
                <Anchor href=forge_ref.url.clone() attr:target="_blank" attr:rel="noopener noreferrer">
                    {label}
                </Anchor>
                <Text dimmed=true size="xs">{author}</Text>
            </Group>
            <Text mono=true dimmed=true size="xs">{repo}</Text>
        </Group>
    }
}

/// The open branches and pull requests of one repository with no item.
#[component]
pub fn UnlinkedRefs(
    /// The slug of the repository.
    #[prop(into)]
    slug: String,
) -> impl IntoView {
    let auth = use_auth();
    let slug = StoredValue::new(slug);
    let open = RwSignal::new(false);
    let reload = RwSignal::new(0u32);
    let refs = LocalResource::new(move || {
        auth.track_session();
        let _ = reload.get();
        let wanted = open.get();
        async move {
            if wanted {
                api::unlinked_refs(auth, &slug.get_value()).await.map(Some)
            } else {
                Ok(None)
            }
        }
    });
    let retry = Callback::new(move |_| reload.update(|n| *n += 1));
    view! {
        <Stack gap="xs" attr:data-testid="repository-unlinked-refs">
            <Group gap="xs">
                <Button variant="default" size="xs"
                    on_click=Callback::new(move |_| open.update(|open| *open = !*open))>
                    {move || if open.get() { "Hide not linked" } else { "Show not linked" }}
                </Button>
            </Group>
            <Show when=move || open.get()>
                {move || match refs.get() {
                    None | Some(Ok(None)) => view! { <Loading label="Loading branches and pull requests…"/> }.into_any(),
                    Some(Err(error)) => view! { <ErrorState error on_retry=retry/> }.into_any(),
                    Some(Ok(Some(refs))) if refs.is_empty() => view! {
                        <Empty message="Each open branch and pull request of this repository is linked to an item."/>
                    }.into_any(),
                    Some(Ok(Some(refs))) => view! {
                        <Stack gap="xs">
                            {refs.into_iter().map(|forge_ref| view! { <ForgeRefRow forge_ref/> }).collect_view()}
                        </Stack>
                    }.into_any(),
                }}
            </Show>
        </Stack>
    }
}
