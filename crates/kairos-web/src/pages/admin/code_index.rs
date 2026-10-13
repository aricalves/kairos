//! `/admin/code-index` — where the code index summaries and vectors of the
//! organization are made (KAIROS-T-0339, COLLIERY-I-0611): the embedded
//! model, Ollama Cloud (an OpenAI-compatible endpoint) or Bedrock for the
//! summaries; the embedded model or a remote endpoint for the vectors.
//!
//! A secret field that is empty keeps the stored secret, as the read token
//! form does; the button "Remove" sends an empty secret, which removes it.
//! The page never shows a secret: the server gives a status (set by whom,
//! when).
//!
//! The panel "Breakers" lists the repositories whose code index breaker is
//! tripped (KAIROS-T-0399): a build job stopped its server or stalled, and
//! the builder starts no build of the repository until a person pushes
//! "Build again" in the panel Code index of the repository (on the page
//! Repositories, or on the team page).

use aurora_dark::components::{
    Anchor, Button, ErrorState, Group, Loading, PageHeader, Panel, Select, Stack, Text, TextInput,
};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use super::{MutationNotice, MutationOutcome, run_mutation};
use crate::api::{get_json, put_json};
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
}

/// mirror of: `kairos_client::types_code_index::PutCodeIndexSettings`.
#[derive(Debug, Serialize)]
struct PutSettings {
    summary: PutSummary,
    vectors: PutVectors,
    concurrency: i32,
    default_summaries: String,
}

fn embedded() -> String {
    "embedded".to_string()
}

/// The setting `default_summaries` (KAIROS-T-0358).
const DEFAULT_SUMMARIES_HINT: &str = "The summarizer of each repository that follows the \
                                      organization: embedded or hosted. A repository can set \
                                      its own value on the page Repositories. Hosted needs a \
                                      hosted provider of the summaries.";

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
struct PutVectors {
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

async fn load_settings(auth: Auth) -> Result<CodeIndexSettings, aurora_dark::tokens::ApiError> {
    get_json(auth, "/api/org/code-index-settings").await
}

/// The text of a secret status: who set it and when, or "none". Pure,
/// host-tested.
pub(crate) fn secret_text(status: &SecretStatus) -> String {
    match (&status.set_by, &status.set_at) {
        (Some(by), Some(at)) => format!("Secret: set by {by} at {at}."),
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

const SUMMARY_HINT: &str = "ollama-cloud needs the base URL (for example https://ollama.com/v1), \
                            the model (for example gemma4:31b) and the API key. bedrock needs \
                            the region, the model id and the credentials as <access key \
                            id>:<secret access key>. An empty secret field keeps the stored \
                            secret.";
const VECTOR_HINT: &str = "remote needs the base URL of an OpenAI-compatible embeddings endpoint \
                           and the model. A local Ollama needs no secret. A change of the vector \
                           model gives the code index new vectors over time.";
const OPT_IN_HINT: &str = "A repository uses the hosted provider when it is set to hosted on the \
                           page Repositories, or when it follows the organization and the default \
                           summarizer of the repositories is hosted.";

/// `/admin/code-index`.
#[component]
pub fn AdminCodeIndexPage() -> impl IntoView {
    let auth = use_auth();
    let reload = RwSignal::new(0u32);
    let outcome: RwSignal<MutationOutcome> = RwSignal::new(None);
    let busy = RwSignal::new(false);
    let settings = LocalResource::new(move || {
        let _ = auth.token();
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
                Some(Ok(current)) => view! {
                    <SettingsForm current busy outcome reload/>
                }.into_any(),
            }}
            <Breakers/>
        </Stack>
    }
}

#[component]
fn SettingsForm(
    current: CodeIndexSettings,
    busy: RwSignal<bool>,
    outcome: RwSignal<MutationOutcome>,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let auth = use_auth();
    let summary_provider = RwSignal::new(current.summary.provider.clone());
    let summary_url = RwSignal::new(current.summary.base_url.clone().unwrap_or_default());
    let summary_model = RwSignal::new(current.summary.model.clone().unwrap_or_default());
    let summary_region = RwSignal::new(current.summary.region.clone().unwrap_or_default());
    let summary_secret = RwSignal::new(String::new());
    let vector_provider = RwSignal::new(current.vectors.provider.clone());
    let vector_url = RwSignal::new(current.vectors.base_url.clone().unwrap_or_default());
    let vector_model = RwSignal::new(current.vectors.model.clone().unwrap_or_default());
    let vector_secret = RwSignal::new(String::new());
    let concurrency = RwSignal::new(current.concurrency.to_string());
    let default_summaries = RwSignal::new(current.default_summaries.clone());
    let summary_secret_text = secret_text(&current.summary.secret);
    let vector_secret_text = secret_text(&current.vectors.secret);
    let summary_secret_set = current.summary.secret.set;
    let vector_secret_set = current.vectors.secret.set;
    let state_line = match (&current.updated_by, &current.updated_at) {
        (Some(by), Some(at)) => format!("Set by {by} at {at}."),
        _ => "Not set: the organization uses the embedded model for both.".to_string(),
    };

    // `remove`: which secret the save removes (an empty secret in the body).
    let save = move |remove_summary: bool, remove_vectors: bool| {
        let body = PutSettings {
            summary: PutSummary {
                provider: summary_provider.get_untracked(),
                base_url: given(&summary_url.get_untracked()),
                model: given(&summary_model.get_untracked()),
                region: given(&summary_region.get_untracked()),
                secret: if remove_summary {
                    Some(String::new())
                } else {
                    given(&summary_secret.get_untracked())
                },
            },
            vectors: PutVectors {
                provider: vector_provider.get_untracked(),
                base_url: given(&vector_url.get_untracked()),
                model: given(&vector_model.get_untracked()),
                secret: if remove_vectors {
                    Some(String::new())
                } else {
                    given(&vector_secret.get_untracked())
                },
            },
            concurrency: concurrency
                .get_untracked()
                .trim()
                .parse()
                .unwrap_or(current.concurrency),
            default_summaries: default_summaries.get_untracked(),
        };
        run_mutation(
            busy,
            outcome,
            reload,
            "Kairos set the providers of the code index.".to_string(),
            async move {
                let _: serde_json::Value =
                    put_json(auth, "/api/org/code-index-settings", &body).await?;
                summary_secret.set(String::new());
                vector_secret.set(String::new());
                Ok(())
            },
        );
    };

    view! {
        <Panel title="Summaries" caption="the model that writes the summaries of the code">
            <Stack gap="sm" attr:data-testid="code-index-summary-settings">
                <Group gap="sm" wrap=true top=true>
                    <Select label="Provider" value=summary_provider
                        options=vec!["embedded".to_string(), "ollama-cloud".to_string(), "bedrock".to_string()]/>
                    <TextInput label="Base URL" value=summary_url placeholder="https://ollama.com/v1"/>
                    <TextInput label="Model" value=summary_model placeholder="gemma4:31b"/>
                    <TextInput label="Region (bedrock)" value=summary_region placeholder="us-east-1"/>
                </Group>
                <Group gap="sm" wrap=true align="end">
                    <TextInput label="Secret" value=summary_secret input_type="password"
                        autocomplete="new-password" spellcheck=false name="summary-secret"/>
                    {summary_secret_set.then(|| view! {
                        <Button variant="default" size="xs" bad=true
                            on_click=Callback::new(move |_| save(true, false))>
                            "Remove the secret"
                        </Button>
                    })}
                </Group>
                <Text dimmed=true size="xs" attr:data-testid="summary-secret-status">{summary_secret_text}</Text>
                <Text dimmed=true size="xs">{SUMMARY_HINT}</Text>
                <Text dimmed=true size="xs">{OPT_IN_HINT}</Text>
            </Stack>
        </Panel>
        <Panel title="Vectors" caption="the model that makes the vectors of the summaries">
            <Stack gap="sm" attr:data-testid="code-index-vector-settings">
                <Group gap="sm" wrap=true top=true>
                    <Select label="Provider" value=vector_provider
                        options=vec!["embedded".to_string(), "remote".to_string()]/>
                    <TextInput label="Base URL" value=vector_url placeholder="http://ollama:11434/v1"/>
                    <TextInput label="Model" value=vector_model placeholder="nomic-embed-text"/>
                </Group>
                <Group gap="sm" wrap=true align="end">
                    <TextInput label="Secret" value=vector_secret input_type="password"
                        autocomplete="new-password" spellcheck=false name="vector-secret"/>
                    {vector_secret_set.then(|| view! {
                        <Button variant="default" size="xs" bad=true
                            on_click=Callback::new(move |_| save(false, true))>
                            "Remove the secret"
                        </Button>
                    })}
                </Group>
                <Text dimmed=true size="xs" attr:data-testid="vector-secret-status">{vector_secret_text}</Text>
                <Text dimmed=true size="xs">{VECTOR_HINT}</Text>
            </Stack>
        </Panel>
        <Panel title="Builder" caption="how the builder uses a hosted provider">
            <Stack gap="sm">
                <Group gap="sm" wrap=true align="end">
                    <TextInput label="Requests at a time (1 to 32)" value=concurrency/>
                    <Select label="Default summarizer of the repositories" value=default_summaries
                        options=vec!["embedded".to_string(), "hosted".to_string()]
                        attr:data-testid="code-index-default-summaries"/>
                    <Button on_click=Callback::new(move |_| save(false, false))
                        attr:data-testid="code-index-settings-save">
                        "Save"
                    </Button>
                </Group>
                <Text dimmed=true size="xs">{DEFAULT_SUMMARIES_HINT}</Text>
                <Text dimmed=true size="xs" attr:data-testid="code-index-settings-state">{state_line}</Text>
            </Stack>
        </Panel>
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
        let _ = auth.token();
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
            }),
            "Secret: set by u1 at 2026-10-07T08:00:00+00:00."
        );
    }

    #[test]
    fn an_empty_field_is_not_given() {
        assert_eq!(given("  "), None);
        assert_eq!(given(" gemma4:31b "), Some("gemma4:31b".to_string()));
    }

    #[test]
    fn the_settings_decode() {
        let settings: CodeIndexSettings = serde_json::from_str(
            r#"{"summary":{"provider":"ollama-cloud","base_url":"https://ollama.com/v1",
                "model":"gemma4:31b","secret":{"set":true,"set_by":"u1","set_at":"t"}},
               "vectors":{"provider":"embedded","secret":{"set":false}},
               "concurrency":4,"updated_by":"u1","updated_at":"t"}"#,
        )
        .expect("decodes");
        assert_eq!(settings.summary.model.as_deref(), Some("gemma4:31b"));
        assert!(settings.summary.secret.set);
        assert!(!settings.vectors.secret.set);
        assert_eq!(settings.default_summaries, "embedded", "an older server");
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
