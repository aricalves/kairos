//! `/admin/code-index` — where the code index summaries and vectors of the
//! organization are made (KAIROS-T-0339, COLLIERY-I-0611): the embedded
//! model, Ollama Cloud (an OpenAI-compatible endpoint) or Bedrock for the
//! summaries; the embedded model or a remote endpoint for the vectors.
//!
//! KAIROS-T-0415: the page first shows what is in effect. Each section
//! (Summaries, Vectors, Builder) has its own Edit, then Save and Cancel; a
//! save sends that section only, with the version that the page read, after
//! a confirmation that lists what changes. A save based on an old read is
//! refused by the server (409): the page says who changed the settings and
//! reads them again. "Remove the secret" is its own action, with a
//! confirmation.
//!
//! A secret field that is empty keeps the stored secret. A change of the
//! provider with no new secret removes the stored secret of that section:
//! it was the secret of the old provider. The page never shows a secret:
//! the server gives a status (set by whom, when).
//!
//! The panel "Breakers" lists the repositories whose code index breaker is
//! tripped (KAIROS-T-0399): a build job stopped its server or stalled, and
//! the builder starts no build of the repository until a person pushes
//! "Build again" in the panel Code index of the repository (on the page
//! Repositories, or on the team page).
//!
//! The section "Read tokens by host" (KAIROS-T-0431) lists the read tokens
//! of the organization: one for each forge host, with who set it and when,
//! never the token. The builder uses the token of the host of a repository
//! before the token of the repository. "Set a token" sets or replaces the
//! token of a host, after a confirmation; "Remove" is its own action, with a
//! confirmation.

use aurora_dark::components::{
    Anchor, Button, ErrorState, Group, Loading, PageHeader, Panel, Pill, Select, Stack, Text,
    TextInput,
};
use aurora_dark::frame::ConfirmDialog;
use aurora_dark::tokens::{ApiError, token};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use super::{MutationNotice, MutationOutcome};
use crate::api::{delete_json, encode_component, get_json, put_json};
use crate::auth::{Auth, use_auth};
use crate::pages::repositories::api::{Repository, list_repositories};

/// mirror of: `kairos_client::types_code_index::SecretStatus`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SecretStatus {
    pub set: bool,
    #[serde(default)]
    pub set_by: Option<String>,
    #[serde(default)]
    pub set_at: Option<String>,
    #[serde(default)]
    pub set_by_name: Option<String>,
}

/// mirror of: `kairos_client::types_code_index::SummaryProviderSettings`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SummaryProvider {
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub secret: SecretStatus,
}

/// mirror of: `kairos_client::types_code_index::VectorProviderSettings`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct VectorProvider {
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub secret: SecretStatus,
}

/// mirror of: `kairos_client::types_code_index::CodeIndexSettings`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CodeIndexSettings {
    pub summary: SummaryProvider,
    pub vectors: VectorProvider,
    pub concurrency: i32,
    /// `embedded` or `hosted` (KAIROS-T-0358). An older server sends none.
    #[serde(default = "embedded")]
    pub default_summaries: String,
    #[serde(default)]
    pub updated_by: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub updated_by_name: Option<String>,
    /// The version that a save sends back (KAIROS-T-0415).
    #[serde(default)]
    pub version: i32,
    /// The repositories whose summaries the hosted provider makes now.
    #[serde(default)]
    pub hosted_repositories: Vec<String>,
}

fn embedded() -> String {
    "embedded".to_string()
}

/// mirror of: `kairos_client::types_code_index::PutCodeIndexSettings`. A
/// section that is `None` is not sent, and stays as stored.
#[derive(Debug, Default, Serialize)]
struct PutSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<PutSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vectors: Option<PutVectors>,
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_summaries: Option<String>,
    version: i32,
}

#[derive(Debug, Clone, Serialize)]
struct PutSummary {
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PutVectors {
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

async fn load_settings(auth: Auth) -> Result<CodeIndexSettings, ApiError> {
    get_json(auth, "/api/org/code-index-settings").await
}

/// A time for a person: the date and the time to the minute. Pure,
/// host-tested.
pub(crate) fn when(rfc3339: &str) -> String {
    match rfc3339.split_once('T') {
        Some((date, time)) => format!("{date} {}", time.get(..5).unwrap_or(time)),
        None => rfc3339.to_string(),
    }
}

/// The text of a secret status: who set it and when, or "none". Pure,
/// host-tested.
pub(crate) fn secret_text(status: &SecretStatus) -> String {
    let by = status.set_by_name.as_ref().or(status.set_by.as_ref());
    match (by, &status.set_at) {
        (Some(by), Some(at)) => format!("Secret: set by {by} at {} (UTC).", when(at)),
        _ if status.set => "Secret: set.".to_string(),
        _ => "Secret: none.".to_string(),
    }
}

/// An empty field is `None`: the server then takes the field as not given.
/// Pure, host-tested.
pub(crate) fn given(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The concurrency of the field, or the refusal to show next to it. Pure,
/// host-tested.
pub(crate) fn concurrency_of(value: &str) -> Result<i32, String> {
    match value.trim().parse::<i32>() {
        Ok(n) if (1..=32).contains(&n) => Ok(n),
        _ => Err(format!(
            "\"{}\" is not a number from 1 to 32.",
            value.trim()
        )),
    }
}

/// The lines of a confirmation: each value that a save changes, as
/// "name: old → new". Pure, host-tested.
pub(crate) fn changes(pairs: &[(&str, Option<&str>, Option<&str>)]) -> Vec<String> {
    let show = |v: Option<&str>| v.filter(|s| !s.is_empty()).unwrap_or("none").to_string();
    pairs
        .iter()
        .filter(|(_, old, new)| show(*old) != show(*new))
        .map(|(name, old, new)| format!("{name}: {} → {}", show(*old), show(*new)))
        .collect()
}

/// The confirmation line about a secret, after a save. Pure, host-tested.
pub(crate) fn secret_change(
    stored: bool,
    old_provider: &str,
    new_provider: &str,
    new_secret: bool,
) -> Option<String> {
    if new_secret {
        return Some("Secret: the new value replaces the stored one.".to_string());
    }
    if stored && old_provider != new_provider {
        return Some(format!(
            "Secret: Kairos removes the stored secret: it is the secret of {old_provider}."
        ));
    }
    None
}

const SUMMARY_HINT: &str = "ollama-cloud needs the base URL (for example https://ollama.com/v1), \
                            the model (for example gemma4:31b) and the API key. bedrock needs \
                            the region, the model id and the credentials as <access key \
                            id>:<secret access key>. An empty secret field keeps the stored \
                            secret. A new provider needs its own secret.";
const VECTOR_HINT: &str = "remote needs the base URL of an OpenAI-compatible embeddings endpoint \
                           and the model. A local Ollama needs no secret. A change of the vector \
                           model gives the code index new vectors over time.";
const DEFAULT_SUMMARIES_HINT: &str = "The summarizer of each repository that follows the \
                                      organization: embedded or hosted. A repository can set \
                                      its own value on the page Repositories. Hosted needs a \
                                      hosted provider of the summaries.";

/// Send `body`. Success: the notice, `done` (the form closes) and a new read.
/// A refusal because the settings changed after the read (409): the
/// server's message, `done`, and a new read so the page shows the current
/// values. Any other refusal: the message, and the form stays open to fix
/// the field.
// The signals of the page and the two endings of a save: a struct for them
// would only name the same eight things again.
#[allow(clippy::too_many_arguments)]
fn send(
    auth: Auth,
    body: PutSettings,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
    success: &'static str,
    done: Callback<()>,
    refused: Callback<()>,
) {
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    leptos::task::spawn_local(async move {
        let result: Result<serde_json::Value, ApiError> =
            put_json(auth, "/api/org/code-index-settings", &body).await;
        busy.set(false);
        match result {
            Ok(_) => {
                done.run(());
                outcome.set(Some(Ok(success.to_string())));
                reload.update(|n| *n += 1);
            }
            Err(error) => {
                let stale = matches!(error, ApiError::Http { status: 409, .. });
                outcome.set(Some(Err(error)));
                if stale {
                    done.run(());
                    reload.update(|n| *n += 1);
                } else {
                    // The confirmation closes; the form stays.
                    refused.run(());
                }
            }
        }
    });
}

/// `/admin/code-index`.
#[component]
pub fn AdminCodeIndexPage() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);
    let settings = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        load_settings(auth)
    });

    view! {
        <PageHeader title="Code index" sub="Where the summaries and the vectors of the code \
                                            index of this organization are made."/>
        <Stack gap="md">
            <MutationNotice outcome/>
            {move || match settings.get() {
                None => view! { <Loading/> }.into_any(),
                Some(Err(error)) => view! {
                    <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                }.into_any(),
                Some(Ok(current)) => {
                    let summary = current.clone();
                    let vectors = current.clone();
                    let builder = current.clone();
                    view! {
                        <SummarySection current=summary busy outcome reload/>
                        <VectorSection current=vectors busy outcome reload/>
                        <BuilderSection current=builder busy outcome reload/>
                        <Text dimmed=true size="xs" attr:data-testid="code-index-settings-state">
                            {state_line(&current)}
                        </Text>
                    }.into_any()
                }
            }}
            <ReadTokens outcome/>
            <Breakers/>
        </Stack>
    }
}

fn state_line(current: &CodeIndexSettings) -> String {
    let by = current
        .updated_by_name
        .as_ref()
        .or(current.updated_by.as_ref());
    match (by, &current.updated_at) {
        (Some(by), Some(at)) => format!(
            "Last change by {by} at {} (UTC). Version {}.",
            when(at),
            current.version
        ),
        _ => "Not set: the organization uses the embedded model for both.".to_string(),
    }
}

/// One "label: value" line of a read view.
#[component]
fn Fact(label: &'static str, value: Option<String>) -> impl IntoView {
    view! {
        <Group gap="sm">
            <Text dimmed=true size="sm">{format!("{label}:")}</Text>
            <Text size="sm">{value.filter(|v| !v.is_empty()).unwrap_or_else(|| "none".to_string())}</Text>
        </Group>
    }
}

#[component]
fn SummarySection(
    current: CodeIndexSettings,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let editing = RwSignal::new(false);
    let confirm = RwSignal::new(false);
    let confirm_remove = RwSignal::new(false);
    let stored = StoredValue::new(current.summary.clone());
    let version = current.version;
    let provider = RwSignal::new(current.summary.provider.clone());
    let url = RwSignal::new(current.summary.base_url.clone().unwrap_or_default());
    let model = RwSignal::new(current.summary.model.clone().unwrap_or_default());
    let region = RwSignal::new(current.summary.region.clone().unwrap_or_default());
    let secret = RwSignal::new(String::new());
    let lines = Signal::derive(move || {
        let old = stored.get_value();
        let mut lines = changes(&[
            (
                "Provider",
                Some(old.provider.as_str()),
                Some(provider.get().as_str()),
            ),
            (
                "Base URL",
                old.base_url.as_deref(),
                given(&url.get()).as_deref(),
            ),
            (
                "Model",
                old.model.as_deref(),
                given(&model.get()).as_deref(),
            ),
            (
                "Region",
                old.region.as_deref(),
                given(&region.get()).as_deref(),
            ),
        ]);
        lines.extend(secret_change(
            old.secret.set,
            &old.provider,
            &provider.get(),
            given(&secret.get()).is_some(),
        ));
        lines
    });
    let close = Callback::new(move |()| {
        confirm.set(false);
        confirm_remove.set(false);
        editing.set(false);
        secret.set(String::new());
    });
    let refused = Callback::new(move |()| {
        confirm.set(false);
        confirm_remove.set(false);
    });
    let save = move |remove: bool| {
        let old = stored.get_value();
        let body = PutSettings {
            summary: Some(if remove {
                PutSummary {
                    provider: old.provider.clone(),
                    base_url: old.base_url.clone(),
                    model: old.model.clone(),
                    region: old.region.clone(),
                    secret: Some(String::new()),
                }
            } else {
                PutSummary {
                    provider: provider.get_untracked(),
                    base_url: given(&url.get_untracked()),
                    model: given(&model.get_untracked()),
                    region: given(&region.get_untracked()),
                    secret: given(&secret.get_untracked()),
                }
            }),
            version,
            ..Default::default()
        };
        send(
            auth,
            body,
            busy,
            outcome,
            reload,
            "Kairos changed the summaries.",
            close,
            refused,
        );
    };
    let s = StoredValue::new(current.summary.clone());
    view! {
        <Panel title="Summaries" caption="the model that writes the summaries of the code">
            <Stack gap="sm" attr:data-testid="code-index-summary-settings">
                <Show when=move || !editing.get()>
                    <Stack gap="xs" attr:data-testid="code-index-summary-view">
                        <Fact label="Provider" value=Some(s.get_value().provider)/>
                        <Fact label="Base URL" value=s.get_value().base_url/>
                        <Fact label="Model" value=s.get_value().model/>
                        <Fact label="Region" value=s.get_value().region/>
                        <Text dimmed=true size="xs" attr:data-testid="summary-secret-status">
                            {secret_text(&s.get_value().secret)}
                        </Text>
                        <Group gap="sm">
                            <Button size="xs" on_click=Callback::new(move |_| editing.set(true))
                                attr:data-testid="code-index-summary-edit">"Edit"</Button>
                            // A hosted provider needs its secret: to drop it, change
                            // the provider (the confirmation says the secret goes).
                            {(s.get_value().secret.set && s.get_value().provider == "embedded").then(|| view! {
                                <Button variant="default" size="xs" bad=true
                                    on_click=Callback::new(move |_| confirm_remove.set(true))>
                                    "Remove the secret"
                                </Button>
                            })}
                        </Group>
                    </Stack>
                </Show>
                <Show when=move || editing.get()>
                    <Group gap="sm" wrap=true top=true>
                        <Select label="Provider" value=provider
                            options=vec!["embedded".to_string(), "ollama-cloud".to_string(), "bedrock".to_string()]/>
                        <TextInput label="Base URL" value=url placeholder="https://ollama.com/v1"/>
                        <TextInput label="Model" value=model placeholder="gemma4:31b"/>
                        <TextInput label="Region (bedrock)" value=region placeholder="us-east-1"/>
                        <TextInput label="Secret" value=secret input_type="password"
                            autocomplete="new-password" spellcheck=false name="summary-secret"/>
                    </Group>
                    <Text dimmed=true size="xs">{SUMMARY_HINT}</Text>
                    <Group gap="sm">
                        <Button size="xs" on_click=Callback::new(move |_| confirm.set(true))
                            attr:data-testid="code-index-summary-save">"Save"</Button>
                        <Button variant="default" size="xs" on_click=Callback::new(move |_| close.run(()))>
                            "Cancel"
                        </Button>
                    </Group>
                </Show>
            </Stack>
        </Panel>
        <ConfirmDialog open=confirm title="Change the summaries?" confirm_label="Save" danger=false
            busy=busy impacts=lines impacts_label="This save changes:"
            on_confirm=Callback::new(move |()| save(false))/>
        <ConfirmDialog open=confirm_remove title="Remove the secret of the summaries?"
            confirm_label="Remove" busy=busy
            message="A hosted provider stops working until it gets a new secret."
            on_confirm=Callback::new(move |()| save(true))/>
    }
}

#[component]
fn VectorSection(
    current: CodeIndexSettings,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let editing = RwSignal::new(false);
    let confirm = RwSignal::new(false);
    let confirm_remove = RwSignal::new(false);
    let stored = StoredValue::new(current.vectors.clone());
    let version = current.version;
    let provider = RwSignal::new(current.vectors.provider.clone());
    let url = RwSignal::new(current.vectors.base_url.clone().unwrap_or_default());
    let model = RwSignal::new(current.vectors.model.clone().unwrap_or_default());
    let secret = RwSignal::new(String::new());
    let lines = Signal::derive(move || {
        let old = stored.get_value();
        let mut lines = changes(&[
            (
                "Provider",
                Some(old.provider.as_str()),
                Some(provider.get().as_str()),
            ),
            (
                "Base URL",
                old.base_url.as_deref(),
                given(&url.get()).as_deref(),
            ),
            (
                "Model",
                old.model.as_deref(),
                given(&model.get()).as_deref(),
            ),
        ]);
        lines.extend(secret_change(
            old.secret.set,
            &old.provider,
            &provider.get(),
            given(&secret.get()).is_some(),
        ));
        lines
    });
    let close = Callback::new(move |()| {
        confirm.set(false);
        confirm_remove.set(false);
        editing.set(false);
        secret.set(String::new());
    });
    let refused = Callback::new(move |()| {
        confirm.set(false);
        confirm_remove.set(false);
    });
    let save = move |remove: bool| {
        let old = stored.get_value();
        let body = PutSettings {
            vectors: Some(if remove {
                PutVectors {
                    provider: old.provider.clone(),
                    base_url: old.base_url.clone(),
                    model: old.model.clone(),
                    secret: Some(String::new()),
                }
            } else {
                PutVectors {
                    provider: provider.get_untracked(),
                    base_url: given(&url.get_untracked()),
                    model: given(&model.get_untracked()),
                    secret: given(&secret.get_untracked()),
                }
            }),
            version,
            ..Default::default()
        };
        send(
            auth,
            body,
            busy,
            outcome,
            reload,
            "Kairos changed the vectors.",
            close,
            refused,
        );
    };
    let v = StoredValue::new(current.vectors.clone());
    view! {
        <Panel title="Vectors" caption="the model that makes the vectors of the summaries">
            <Stack gap="sm" attr:data-testid="code-index-vector-settings">
                <Show when=move || !editing.get()>
                    <Stack gap="xs" attr:data-testid="code-index-vector-view">
                        <Fact label="Provider" value=Some(v.get_value().provider)/>
                        <Fact label="Base URL" value=v.get_value().base_url/>
                        <Fact label="Model" value=v.get_value().model/>
                        <Text dimmed=true size="xs" attr:data-testid="vector-secret-status">
                            {secret_text(&v.get_value().secret)}
                        </Text>
                        <Group gap="sm">
                            <Button size="xs" on_click=Callback::new(move |_| editing.set(true))
                                attr:data-testid="code-index-vector-edit">"Edit"</Button>
                            {v.get_value().secret.set.then(|| view! {
                                <Button variant="default" size="xs" bad=true
                                    on_click=Callback::new(move |_| confirm_remove.set(true))>
                                    "Remove the secret"
                                </Button>
                            })}
                        </Group>
                    </Stack>
                </Show>
                <Show when=move || editing.get()>
                    <Group gap="sm" wrap=true top=true>
                        <Select label="Provider" value=provider
                            options=vec!["embedded".to_string(), "remote".to_string()]/>
                        <TextInput label="Base URL" value=url placeholder="http://ollama:11434/v1"/>
                        <TextInput label="Model" value=model placeholder="nomic-embed-text"/>
                        <TextInput label="Secret" value=secret input_type="password"
                            autocomplete="new-password" spellcheck=false name="vector-secret"/>
                    </Group>
                    <Text dimmed=true size="xs">{VECTOR_HINT}</Text>
                    <Group gap="sm">
                        <Button size="xs" on_click=Callback::new(move |_| confirm.set(true))
                            attr:data-testid="code-index-vector-save">"Save"</Button>
                        <Button variant="default" size="xs" on_click=Callback::new(move |_| close.run(()))>
                            "Cancel"
                        </Button>
                    </Group>
                </Show>
            </Stack>
        </Panel>
        <ConfirmDialog open=confirm title="Change the vectors?" confirm_label="Save" danger=false
            busy=busy impacts=lines impacts_label="This save changes:"
            on_confirm=Callback::new(move |()| save(false))/>
        <ConfirmDialog open=confirm_remove title="Remove the secret of the vectors?"
            confirm_label="Remove" busy=busy
            on_confirm=Callback::new(move |()| save(true))/>
    }
}

#[component]
fn BuilderSection(
    current: CodeIndexSettings,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let editing = RwSignal::new(false);
    let confirm = RwSignal::new(false);
    let version = current.version;
    let old_concurrency = current.concurrency;
    let old_default = StoredValue::new(current.default_summaries.clone());
    let concurrency = RwSignal::new(current.concurrency.to_string());
    let default_summaries = RwSignal::new(current.default_summaries.clone());
    let concurrency_bad = Signal::derive(move || concurrency_of(&concurrency.get()).is_err());
    // The field shows the refusal next to it; empty when the value is good.
    let concurrency_error =
        Signal::derive(move || concurrency_of(&concurrency.get()).err().unwrap_or_default());
    let lines = Signal::derive(move || {
        let old_default = old_default.get_value();
        changes(&[
            (
                "Requests at a time",
                Some(old_concurrency.to_string().as_str()),
                Some(concurrency.get().trim()),
            ),
            (
                "Default summarizer of the repositories",
                Some(old_default.as_str()),
                Some(default_summaries.get().as_str()),
            ),
        ])
    });
    let close = Callback::new(move |()| {
        confirm.set(false);
        editing.set(false);
    });
    let refused = Callback::new(move |()| confirm.set(false));
    let save = move || {
        let Ok(n) = concurrency_of(&concurrency.get_untracked()) else {
            confirm.set(false);
            return;
        };
        let body = PutSettings {
            concurrency: Some(n),
            default_summaries: Some(default_summaries.get_untracked()),
            version,
            ..Default::default()
        };
        send(
            auth,
            body,
            busy,
            outcome,
            reload,
            "Kairos changed the builder.",
            close,
            refused,
        );
    };
    let hosted = StoredValue::new(current.hosted_repositories.clone());
    let shown_default = StoredValue::new(current.default_summaries.clone());
    view! {
        <Panel title="Builder" caption="how the builder uses a hosted provider">
            <Stack gap="sm">
                <Show when=move || !editing.get()>
                    <Stack gap="xs" attr:data-testid="code-index-builder-view">
                        <Fact label="Requests at a time" value=Some(old_concurrency.to_string())/>
                        <Fact label="Default summarizer of the repositories"
                            value=Some(shown_default.get_value())/>
                        <Group gap="xs" wrap=true attr:data-testid="code-index-hosted-repositories">
                            <Text dimmed=true size="sm">"Repositories with hosted summaries:"</Text>
                            {move || {
                                let hosted = hosted.get_value();
                                if hosted.is_empty() {
                                    view! { <Text size="sm">"none"</Text> }.into_any()
                                } else {
                                    hosted.into_iter().map(|slug| view! {
                                        <Pill color=token::GOLD>{slug}</Pill>
                                    }).collect_view().into_any()
                                }
                            }}
                        </Group>
                        <Group gap="sm">
                            <Button size="xs" on_click=Callback::new(move |_| editing.set(true))
                                attr:data-testid="code-index-builder-edit">"Edit"</Button>
                        </Group>
                    </Stack>
                </Show>
                <Show when=move || editing.get()>
                    <Group gap="sm" wrap=true align="end">
                        <TextInput label="Requests at a time (1 to 32)" value=concurrency
                            error=concurrency_error/>
                        <Select label="Default summarizer of the repositories" value=default_summaries
                            options=vec!["embedded".to_string(), "hosted".to_string()]
                            attr:data-testid="code-index-default-summaries"/>
                    </Group>
                    <Text dimmed=true size="xs">{DEFAULT_SUMMARIES_HINT}</Text>
                    <Group gap="sm">
                        <Button size="xs"
                            disabled=concurrency_bad
                            on_click=Callback::new(move |_| confirm.set(true))
                            attr:data-testid="code-index-settings-save">"Save"</Button>
                        <Button variant="default" size="xs" on_click=Callback::new(move |_| close.run(()))>
                            "Cancel"
                        </Button>
                    </Group>
                </Show>
            </Stack>
        </Panel>
        <ConfirmDialog open=confirm title="Change the builder?" confirm_label="Save" danger=false
            busy=busy impacts=lines impacts_label="This save changes:"
            on_confirm=Callback::new(move |()| save())/>
    }
}

/// mirror of: `kairos_client::types_code_index::OrgReadToken`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct OrgReadToken {
    pub host: String,
    pub set_by: String,
    #[serde(default)]
    pub set_by_name: Option<String>,
    pub set_at: String,
}

/// mirror of: `kairos_client::types_code_index::SetOrgReadTokenRequest`.
#[derive(Clone, Debug, Serialize)]
struct SetOrgReadToken {
    token: String,
}

/// The line of a read token of a host: who set it and when. Pure,
/// host-tested.
pub(crate) fn read_token_line(token: &OrgReadToken) -> String {
    let by = token.set_by_name.as_ref().unwrap_or(&token.set_by);
    format!("Set by {by} at {} (UTC).", when(&token.set_at))
}

const READ_TOKENS_HINT: &str = "The builder fetches each repository on a host with the token of \
                                the host. The token of a repository is used only for a host \
                                with no token here. Use a token that can only read the code. \
                                The host is the host only, for example github.com, or \
                                git.example.org:8443 for a port that is not 443.";

/// The read tokens of the organization by host (KAIROS-T-0431).
#[component]
fn ReadTokens(outcome: RwSignal<MutationOutcome>) -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let busy = RwSignal::new(false);
    let editing = RwSignal::new(false);
    let confirm = RwSignal::new(false);
    let confirm_remove = RwSignal::new(false);
    let host = RwSignal::new(String::new());
    let secret = RwSignal::new(String::new());
    let removing = RwSignal::new(String::new());
    let tokens = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        get_json::<Vec<OrgReadToken>>(auth, "/api/org/read-tokens")
    });
    let close = move || {
        confirm.set(false);
        confirm_remove.set(false);
        editing.set(false);
        host.set(String::new());
        secret.set(String::new());
    };
    let save = move || {
        if busy.get_untracked() {
            return;
        }
        let Some(target) = given(&host.get_untracked()) else {
            confirm.set(false);
            outcome.set(Some(Err(ApiError::Unknown(
                "Write the host, for example github.com.".to_string(),
            ))));
            return;
        };
        busy.set(true);
        let body = SetOrgReadToken {
            token: secret.get_untracked(),
        };
        leptos::task::spawn_local(async move {
            let path = format!("/api/org/read-tokens/{}", encode_component(&target));
            let result: Result<OrgReadToken, ApiError> = put_json(auth, &path, &body).await;
            busy.set(false);
            match result {
                Ok(token) => {
                    close();
                    outcome.set(Some(Ok(format!(
                        "Kairos keeps the read token of {}.",
                        token.host
                    ))));
                    reload.update(|n| *n += 1);
                }
                Err(error) => {
                    // The confirmation closes; the form stays.
                    confirm.set(false);
                    outcome.set(Some(Err(error)));
                }
            }
        });
    };
    let remove = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let target = removing.get_untracked();
        leptos::task::spawn_local(async move {
            let path = format!("/api/org/read-tokens/{}", encode_component(&target));
            let result: Result<Vec<OrgReadToken>, ApiError> = delete_json(auth, &path).await;
            busy.set(false);
            confirm_remove.set(false);
            match result {
                Ok(_) => outcome.set(Some(Ok(format!(
                    "Kairos removed the read token of {target}."
                )))),
                Err(error) => outcome.set(Some(Err(error))),
            }
            reload.update(|n| *n += 1);
        });
    };
    let confirm_lines = Signal::derive(move || {
        let target = given(&host.get()).unwrap_or_else(|| "none".to_string());
        vec![
            format!("Host: {target}"),
            "Token: the new value replaces a stored token of this host.".to_string(),
        ]
    });
    let remove_title =
        Signal::derive(move || format!("Remove the read token of {}?", removing.get()));
    view! {
        <Panel title="Read tokens by host" caption="the tokens that the builder uses for each repository on a host">
            <Stack gap="sm" attr:data-testid="code-index-read-tokens">
                {move || match tokens.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(list)) if list.is_empty() => view! {
                        <Text size="sm">"No host has a token."</Text>
                    }.into_any(),
                    Some(Ok(list)) => list
                        .into_iter()
                        .map(|token| {
                            let line = read_token_line(&token);
                            let target = token.host.clone();
                            view! {
                                <Group gap="sm">
                                    <Text size="sm">{token.host.clone()}</Text>
                                    <Text dimmed=true size="xs">{line}</Text>
                                    <Button variant="default" size="xs" bad=true
                                        on_click=Callback::new(move |_| {
                                            removing.set(target.clone());
                                            confirm_remove.set(true);
                                        })>
                                        "Remove"
                                    </Button>
                                </Group>
                            }
                        })
                        .collect_view()
                        .into_any(),
                }}
                <Show when=move || !editing.get()>
                    <Group gap="sm">
                        <Button size="xs" on_click=Callback::new(move |_| editing.set(true))
                            attr:data-testid="code-index-read-token-add">"Set a token"</Button>
                    </Group>
                </Show>
                <Show when=move || editing.get()>
                    <Group gap="sm" wrap=true top=true>
                        <TextInput label="Host" value=host placeholder="github.com"/>
                        <TextInput label="Token" value=secret input_type="password"
                            autocomplete="new-password" spellcheck=false name="org-read-token"/>
                    </Group>
                    <Group gap="sm">
                        <Button size="xs" on_click=Callback::new(move |_| confirm.set(true))
                            attr:data-testid="code-index-read-token-save">"Save"</Button>
                        <Button variant="default" size="xs" on_click=Callback::new(move |_| close())>
                            "Cancel"
                        </Button>
                    </Group>
                </Show>
                <Text dimmed=true size="xs">{READ_TOKENS_HINT}</Text>
            </Stack>
        </Panel>
        <ConfirmDialog open=confirm title="Set the read token of this host?" confirm_label="Save"
            danger=false busy=busy impacts=confirm_lines impacts_label="This save changes:"
            on_confirm=Callback::new(move |()| save())/>
        <ConfirmDialog open=confirm_remove title=remove_title confirm_label="Remove" busy=busy
            message="The builder then fetches each repository on this host with the token of \
                     the repository, or with no token."
            on_confirm=Callback::new(move |()| remove())/>
    }
}

/// The repositories whose code index breaker is tripped (KAIROS-T-0399),
/// by slug. Pure, host-tested.
pub(crate) fn tripped(repos: &[Repository]) -> Vec<&Repository> {
    let mut out: Vec<&Repository> = repos
        .iter()
        .filter(|repo| repo.code_index_breaker.tripped)
        .collect();
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

const BREAKERS_HINT: &str = "A build job that stops its server (for example, it runs out of \
                             memory) or stalls trips the breaker of its repository. Kairos then \
                             starts no build of the repository: no update, no first build. To \
                             reset the breaker, push \"Build again\" in the panel Code index of \
                             the repository on the page Repositories.";

/// The panel of the tripped breakers (KAIROS-T-0399).
#[component]
fn Breakers() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let repos = LocalResource::new(move || {
        auth.track_session();
        reload.get();
        list_repositories(auth, None)
    });
    view! {
        <Panel title="Breakers" caption="the repositories that the builder does not build now">
            <Stack gap="sm" attr:data-testid="code-index-breakers">
                {move || match repos.get() {
                    None => view! { <Loading/> }.into_any(),
                    Some(Err(error)) => view! {
                        <ErrorState error on_retry=Callback::new(move |_| reload.update(|n| *n += 1))/>
                    }.into_any(),
                    Some(Ok(repos)) => {
                        let tripped: Vec<Repository> = tripped(&repos).into_iter().cloned().collect();
                        if tripped.is_empty() {
                            view! { <Text size="sm">"No breaker is tripped."</Text> }.into_any()
                        } else {
                            tripped
                                .into_iter()
                                .map(|repo| {
                                    let href = "/admin/repositories".to_string();
                                    let line = repo.code_index_breaker.summary();
                                    view! {
                                        <Stack gap="xs">
                                            <Anchor href=href>{repo.slug.clone()}</Anchor>
                                            <Text dimmed=true size="xs">{line}</Text>
                                        </Stack>
                                    }
                                })
                                .collect_view()
                                .into_any()
                        }
                    }
                }}
                <Text dimmed=true size="xs">{BREAKERS_HINT}</Text>
            </Stack>
        </Panel>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_secret_status_reads_as_a_line() {
        assert_eq!(secret_text(&SecretStatus::default()), "Secret: none.");
        assert_eq!(
            secret_text(&SecretStatus {
                set: true,
                set_by: Some("u1".into()),
                set_at: Some("2026-10-07T08:00:00+00:00".into()),
                set_by_name: Some("Robin".into()),
            }),
            "Secret: set by Robin at 2026-10-07 08:00 (UTC)."
        );
    }

    #[test]
    fn an_empty_field_is_not_given() {
        assert_eq!(given("  "), None);
        assert_eq!(given(" gemma4:31b "), Some("gemma4:31b".to_string()));
    }

    #[test]
    fn the_concurrency_must_be_a_number_from_1_to_32() {
        assert_eq!(concurrency_of(" 8 "), Ok(8));
        assert!(concurrency_of("eight").unwrap_err().contains("\"eight\""));
        assert!(concurrency_of("0").is_err());
        assert!(concurrency_of("33").is_err());
    }

    #[test]
    fn the_confirmation_lists_each_change_only() {
        assert_eq!(
            changes(&[
                ("Provider", Some("ollama-cloud"), Some("bedrock")),
                ("Model", Some("gemma4:31b"), Some("gemma4:31b")),
                ("Region", None, Some("us-east-1")),
                ("Base URL", Some("https://ollama.com/v1"), None),
            ]),
            vec![
                "Provider: ollama-cloud → bedrock".to_string(),
                "Region: none → us-east-1".to_string(),
                "Base URL: https://ollama.com/v1 → none".to_string(),
            ]
        );
    }

    #[test]
    fn a_new_provider_without_a_secret_removes_the_old_secret() {
        assert_eq!(
            secret_change(true, "ollama-cloud", "bedrock", false).as_deref(),
            Some("Secret: Kairos removes the stored secret: it is the secret of ollama-cloud.")
        );
        assert_eq!(
            secret_change(true, "ollama-cloud", "ollama-cloud", false),
            None
        );
        assert!(secret_change(false, "embedded", "remote", true).is_some());
    }

    #[test]
    fn the_settings_decode() {
        let settings: CodeIndexSettings = serde_json::from_str(
            r#"{"summary":{"provider":"ollama-cloud","base_url":"https://ollama.com/v1",
                "model":"gemma4:31b","secret":{"set":true,"set_by":"u1","set_at":"t",
                "set_by_name":"Robin"}},
               "vectors":{"provider":"embedded","secret":{"set":false}},
               "concurrency":4,"updated_by":"u1","updated_at":"t","updated_by_name":"Robin",
               "version":3,"hosted_repositories":["payments"]}"#,
        )
        .expect("decodes");
        assert_eq!(settings.summary.model.as_deref(), Some("gemma4:31b"));
        assert!(settings.summary.secret.set);
        assert_eq!(settings.version, 3);
        assert_eq!(settings.hosted_repositories, vec!["payments".to_string()]);
        assert_eq!(settings.default_summaries, "embedded", "an older server");
    }

    #[test]
    fn a_save_of_one_section_sends_only_that_section_and_the_version() {
        let body = PutSettings {
            concurrency: Some(8),
            version: 3,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"concurrency": 8, "version": 3})
        );
    }

    #[test]
    fn a_read_token_reads_as_who_and_when_and_never_a_value() {
        let token: OrgReadToken = serde_json::from_str(
            r#"{"host":"github.com","set_by":"u1","set_by_name":"Robin",
                "set_at":"2026-10-14T08:30:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!(
            read_token_line(&token),
            "Set by Robin at 2026-10-14 08:30 (UTC)."
        );
        let unnamed = OrgReadToken {
            set_by_name: None,
            ..token
        };
        assert_eq!(
            read_token_line(&unnamed),
            "Set by u1 at 2026-10-14 08:30 (UTC)."
        );
    }

    fn repo(slug: &str, breaker: &str) -> Repository {
        serde_json::from_str(&format!(
            r#"{{"id":"r-{slug}","slug":"{slug}","forge":"github","repo_full_name":"o/{slug}",
                "repo_url":"https://example.com/o/{slug}","default_branch":"main",
                "team":{{"id":"t1","slug":"platform","name":"Platform"}}{breaker}}}"#
        ))
        .expect("decodes")
    }

    #[test]
    fn the_panel_lists_the_tripped_breakers_by_slug() {
        let tripped_breaker = r#","code_index_breaker":{"tripped":true,
            "tripped_at":"2026-10-12T00:00:00+00:00","reason":"The lease ran out."}"#;
        let repos = vec![
            repo("zeta", tripped_breaker),
            repo("plain", ""),
            repo("alpha", tripped_breaker),
        ];
        let slugs: Vec<&str> = tripped(&repos).iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(slugs, ["alpha", "zeta"]);
        assert_eq!(
            repos[1].code_index_breaker.summary(),
            "Code index breaker: not tripped.",
            "an older server sends no breaker"
        );
        let line = repos[0].code_index_breaker.summary();
        assert!(
            line.starts_with(
                "Code index breaker: tripped at 2026-10-12T00:00:00+00:00. The lease ran out."
            ),
            "{line}"
        );
    }
}
