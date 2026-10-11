//! `kairos admin tenants` — deployment-admin tenant provisioning (S-0005
//! `/api/admin/tenants`; the caller's OIDC `sub` must be listed in the
//! server's `KAIROS_DEPLOYMENT_ADMINS`, otherwise 403 with that guidance).
//!
//! `kairos admin agent-keys` (KAIROS-T-0388): an organization admin lists
//! the agent keys of each person and revokes any one.

use kairos_client::types_auth::Secret;
use kairos_client::types_code_index::{
    CodeIndexSettings, PutCodeIndexSettings, PutSummaryProvider, PutVectorProvider,
};
use kairos_client::types_org::CreateTenantRequest;
use kairos_client::types_service_accounts::OrgAgentKeysQuery;

use crate::commands::entities::{ListArgs, require_confirm};
use crate::context::{Common, client, print_json};
use crate::error::CliError;
use crate::table::Table;

/// Deployment-administration operations.
#[derive(clap::Subcommand, Debug)]
pub enum AdminCommand {
    /// Tenant provisioning (deployment-admin only, cross-tenant)
    #[command(subcommand)]
    Tenants(TenantsCommand),
    /// The providers of the code index summaries and vectors of this
    /// organization (an organization admin sets them)
    #[command(subcommand, name = "code-index-settings")]
    CodeIndexSettings(CodeIndexSettingsCommand),
    /// The agent keys of each person of the organization (an organization
    /// admin lists and revokes them)
    #[command(subcommand, name = "agent-keys")]
    AgentKeys(AgentKeysCommand),
}

/// List and revoke the agent keys of the persons of the organization.
#[derive(clap::Subcommand, Debug)]
pub enum AgentKeysCommand {
    /// List the agent keys of each person, newest first (prefixes only;
    /// never the secret). A revoked key stays in the list
    List {
        /// Only the keys of this person: a user id (UUID) or an email
        #[arg(long)]
        user: Option<String>,
        #[command(flatten)]
        list: ListArgs,
    },
    /// Revoke the agent key of a person (requires --confirm). Kairos tells
    /// the owner
    Revoke {
        /// Key id (UUID; see `kairos admin agent-keys list`)
        key_id: String,
        /// Actually revoke it
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

impl AgentKeysCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List { user, list } => {
                let client = client(&list.common)?;
                let page = list.page();
                let envelope = client
                    .list_org_agent_keys(&OrgAgentKeysQuery {
                        user,
                        limit: page.limit,
                        offset: page.offset,
                    })
                    .await?;
                if list.common.json {
                    return print_json(&envelope);
                }
                if envelope.items.is_empty() {
                    println!("(none)");
                } else {
                    let mut table = Table::new(&[
                        "ID",
                        "OWNER",
                        "NAME",
                        "PREFIX",
                        "CREATED",
                        "EXPIRES",
                        "LAST_USED",
                        "REVOKED",
                    ]);
                    let or_dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
                    for key in &envelope.items {
                        table.row(vec![
                            key.id.clone(),
                            key.owner_email.clone(),
                            key.name.clone(),
                            key.prefix.clone(),
                            key.created_at.clone(),
                            key.expires_at.clone().unwrap_or_else(|| "never".into()),
                            or_dash(&key.last_used_at),
                            or_dash(&key.revoked_at),
                        ]);
                    }
                    print!("{}", table.render());
                }
                println!(
                    "total: {} (limit {}, offset {})",
                    envelope.total, envelope.limit, envelope.offset
                );
                Ok(())
            }
            Self::Revoke {
                key_id,
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the agent key {key_id}"))?;
                let client = client(&common)?;
                let deleted = client.revoke_org_agent_key(&key_id).await?;
                if common.json {
                    return print_json(&deleted);
                }
                println!(
                    "Kairos revoked the agent key {}. Kairos told the owner.",
                    deleted.id
                );
                Ok(())
            }
        }
    }
}

/// Show or set where the code index summaries and vectors of the
/// organization are made.
#[derive(clap::Subcommand, Debug)]
// `Set` has a field for each flag; clap builds the value once (KAIROS-T-0358).
#[allow(clippy::large_enum_variant)]
pub enum CodeIndexSettingsCommand {
    /// Show the providers and whether each secret is set
    Show {
        #[command(flatten)]
        common: Common,
    },
    /// Set the providers. A flag that is not given keeps its value. A
    /// secret comes from standard input (--summary-secret-stdin,
    /// --vector-secret-stdin), never from an argument
    Set {
        /// embedded, ollama-cloud or bedrock
        #[arg(long)]
        summary_provider: Option<String>,
        /// The base URL of the OpenAI-compatible endpoint (ollama-cloud)
        #[arg(long)]
        summary_url: Option<String>,
        /// The model name (ollama-cloud) or the model id (bedrock)
        #[arg(long)]
        summary_model: Option<String>,
        /// The AWS region (bedrock)
        #[arg(long)]
        summary_region: Option<String>,
        /// Read the API key (ollama-cloud), or the AWS credentials as
        /// <access key id>:<secret access key>[:<session token>] (bedrock),
        /// from standard input
        #[arg(long)]
        summary_secret_stdin: bool,
        /// Remove the stored secret of the summaries
        #[arg(long, conflicts_with = "summary_secret_stdin")]
        clear_summary_secret: bool,
        /// embedded or remote
        #[arg(long)]
        vector_provider: Option<String>,
        /// The base URL of the OpenAI-compatible embeddings endpoint (remote)
        #[arg(long)]
        vector_url: Option<String>,
        /// The model name of the embeddings (remote)
        #[arg(long)]
        vector_model: Option<String>,
        /// Read the API key of the embeddings endpoint from standard input
        #[arg(long)]
        vector_secret_stdin: bool,
        /// Remove the stored secret of the vectors
        #[arg(long, conflicts_with = "vector_secret_stdin")]
        clear_vector_secret: bool,
        /// The requests that a hosted summarizer sends at a time, 1 to 32
        #[arg(long)]
        concurrency: Option<i32>,
        /// The summarizer of each repository that follows the organization:
        /// embedded or hosted (the provider of the summaries; the code of
        /// each changed symbol leaves the host)
        #[arg(long, value_name = "EMBEDDED|HOSTED", value_parser = ["embedded", "hosted"])]
        default_summaries: Option<String>,
        #[command(flatten)]
        common: Common,
    },
}

/// The body of a `set`: the current settings with the flags over them.
/// Pure, host-tested.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settings_body(
    current: &CodeIndexSettings,
    summary_provider: Option<String>,
    summary_url: Option<String>,
    summary_model: Option<String>,
    summary_region: Option<String>,
    summary_secret: Option<Secret>,
    vector_provider: Option<String>,
    vector_url: Option<String>,
    vector_model: Option<String>,
    vector_secret: Option<Secret>,
    concurrency: Option<i32>,
    default_summaries: Option<String>,
) -> PutCodeIndexSettings {
    PutCodeIndexSettings {
        summary: PutSummaryProvider {
            provider: summary_provider.unwrap_or_else(|| current.summary.provider.clone()),
            base_url: summary_url.or_else(|| current.summary.base_url.clone()),
            model: summary_model.or_else(|| current.summary.model.clone()),
            region: summary_region.or_else(|| current.summary.region.clone()),
            secret: summary_secret,
        },
        vectors: PutVectorProvider {
            provider: vector_provider.unwrap_or_else(|| current.vectors.provider.clone()),
            base_url: vector_url.or_else(|| current.vectors.base_url.clone()),
            model: vector_model.or_else(|| current.vectors.model.clone()),
            secret: vector_secret,
        },
        concurrency: Some(concurrency.unwrap_or(current.concurrency)),
        // clap accepts only embedded and hosted. Not given: the value stays.
        default_summaries: default_summaries
            .as_deref()
            .and_then(kairos_client::types_repositories::CodeIndexSummaries::parse),
    }
}

fn print_settings(settings: &CodeIndexSettings) {
    let secret = |status: &kairos_client::types_code_index::SecretStatus| match (
        &status.set_by,
        &status.set_at,
    ) {
        (Some(by), Some(at)) => format!("set by {by} at {at}"),
        _ if status.set => "set".to_string(),
        _ => "none".to_string(),
    };
    println!("Summaries: {}", settings.summary.provider);
    if let Some(url) = &settings.summary.base_url {
        println!("  base URL: {url}");
    }
    if let Some(model) = &settings.summary.model {
        println!("  model: {model}");
    }
    if let Some(region) = &settings.summary.region {
        println!("  region: {region}");
    }
    println!("  secret: {}", secret(&settings.summary.secret));
    println!("Vectors: {}", settings.vectors.provider);
    if let Some(url) = &settings.vectors.base_url {
        println!("  base URL: {url}");
    }
    if let Some(model) = &settings.vectors.model {
        println!("  model: {model}");
    }
    println!("  secret: {}", secret(&settings.vectors.secret));
    println!("Concurrency: {}", settings.concurrency);
    println!(
        "Default summarizer of the repositories: {}",
        settings.default_summaries
    );
    match (&settings.updated_by, &settings.updated_at) {
        (Some(by), Some(at)) => println!("Set by {by} at {at}."),
        _ => println!("Not set: the organization uses the embedded model."),
    }
}

impl CodeIndexSettingsCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::Show { common } => {
                let client = client(&common)?;
                let settings = client.code_index_settings().await?;
                if common.json {
                    return print_json(&settings);
                }
                print_settings(&settings);
                Ok(())
            }
            Self::Set {
                summary_provider,
                summary_url,
                summary_model,
                summary_region,
                summary_secret_stdin,
                clear_summary_secret,
                vector_provider,
                vector_url,
                vector_model,
                vector_secret_stdin,
                clear_vector_secret,
                concurrency,
                default_summaries,
                common,
            } => {
                let summary_secret = if clear_summary_secret {
                    Some(Secret::new(""))
                } else if summary_secret_stdin {
                    Some(crate::password::read_secret(
                        "Secret of the summaries provider: ",
                        "secret",
                    )?)
                } else {
                    None
                };
                let vector_secret = if clear_vector_secret {
                    Some(Secret::new(""))
                } else if vector_secret_stdin {
                    Some(crate::password::read_secret(
                        "Secret of the vectors provider: ",
                        "secret",
                    )?)
                } else {
                    None
                };
                let client = client(&common)?;
                let current = client.code_index_settings().await?;
                let body = settings_body(
                    &current,
                    summary_provider,
                    summary_url,
                    summary_model,
                    summary_region,
                    summary_secret,
                    vector_provider,
                    vector_url,
                    vector_model,
                    vector_secret,
                    concurrency,
                    default_summaries,
                );
                let settings = client.put_code_index_settings(&body).await?;
                if common.json {
                    return print_json(&settings);
                }
                println!("Kairos set the providers of the code index.");
                print_settings(&settings);
                Ok(())
            }
        }
    }
}

/// Provision, list, and drop tenants.
#[derive(clap::Subcommand, Debug)]
pub enum TenantsCommand {
    /// List provisioned tenants (paginated)
    List(ListArgs),
    /// Provision a tenant: organization + schema + default boards
    Create {
        /// Organization slug (^[a-z][a-z0-9_-]{1,62}$)
        #[arg(long)]
        slug: String,
        /// Organization display name
        #[arg(long)]
        name: String,
        /// OIDC sub of the initial org admin (defaults to you; the user
        /// must have logged in once)
        #[arg(long, value_name = "OIDC_SUB")]
        initial_admin: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// Drop a tenant — destructive and unrecoverable (requires --confirm)
    Delete {
        /// The tenant's slug
        slug: String,
        /// Actually drop the tenant's organization and schema
        #[arg(long)]
        confirm: bool,
        #[command(flatten)]
        common: Common,
    },
}

impl AdminCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::Tenants(command) => command.run().await,
            Self::CodeIndexSettings(command) => command.run().await,
            Self::AgentKeys(command) => command.run().await,
        }
    }
}

impl TenantsCommand {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            Self::List(args) => {
                let client = client(&args.common)?;
                let envelope = client.list_tenants(args.page()).await?;
                if args.common.json {
                    return print_json(&envelope);
                }
                if envelope.items.is_empty() {
                    println!("(none)");
                } else {
                    let mut table = Table::new(&["SLUG", "NAME", "SCHEMA"]);
                    for tenant in &envelope.items {
                        table.row(vec![
                            tenant.slug.clone(),
                            tenant.name.clone(),
                            if tenant.schema_exists {
                                "ok".to_string()
                            } else {
                                "MISSING".to_string()
                            },
                        ]);
                    }
                    print!("{}", table.render());
                }
                println!(
                    "total: {} (limit {}, offset {})",
                    envelope.total, envelope.limit, envelope.offset
                );
                Ok(())
            }
            Self::Create {
                slug,
                name,
                initial_admin,
                common,
            } => {
                let client = client(&common)?;
                let report = client
                    .create_tenant(&CreateTenantRequest {
                        slug,
                        name,
                        initial_admin_external_id: initial_admin,
                    })
                    .await?;
                if common.json {
                    return print_json(&report);
                }
                println!(
                    "Kairos made the tenant {} (schema {}).",
                    report.slug, report.schema
                );
                println!("  boards created: {}", report.boards_created.join(", "));
                println!(
                    "  templates copied: {}, metadata definitions copied: {}",
                    report.templates_copied, report.metadata_definitions_copied
                );
                println!(
                    "  initial admin: {} ({})",
                    report.initial_admin.email, report.initial_admin.external_id
                );
                Ok(())
            }
            Self::Delete {
                slug,
                confirm,
                common,
            } => {
                require_confirm(confirm, &format!("the tenant {slug}"))?;
                let client = client(&common)?;
                let response = client.delete_tenant(&slug, Some(true)).await?;
                if common.json {
                    return print_json(&response);
                }
                println!("Kairos deleted the tenant {}.", response.slug);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod code_index_settings_tests {
    use super::*;
    use kairos_client::types_code_index::{
        SecretStatus, SummaryProviderSettings, VectorProviderSettings,
    };

    fn current() -> CodeIndexSettings {
        CodeIndexSettings {
            summary: SummaryProviderSettings {
                provider: "ollama-cloud".into(),
                base_url: Some("https://ollama.com/v1".into()),
                model: Some("gemma4:31b".into()),
                region: None,
                secret: SecretStatus::default(),
            },
            vectors: VectorProviderSettings {
                provider: "embedded".into(),
                base_url: None,
                model: None,
                secret: SecretStatus::default(),
            },
            concurrency: 8,
            default_summaries: Default::default(),
            updated_by: None,
            updated_at: None,
        }
    }

    /// KAIROS-T-0339: a `set` keeps each value that no flag names, and a
    /// secret goes only when a flag gives it.
    #[test]
    fn a_set_puts_the_flags_over_the_current_settings() {
        let body = settings_body(
            &current(),
            None,
            None,
            Some("kimi-k2.7-code".into()),
            None,
            None,
            Some("remote".into()),
            Some("http://ollama:11434/v1".into()),
            Some("nomic-embed-text".into()),
            None,
            None,
            None,
        );
        assert_eq!(body.default_summaries, None, "no flag: the value stays");
        assert_eq!(body.summary.provider, "ollama-cloud");
        assert_eq!(
            body.summary.base_url.as_deref(),
            Some("https://ollama.com/v1")
        );
        assert_eq!(body.summary.model.as_deref(), Some("kimi-k2.7-code"));
        assert_eq!(body.summary.secret, None, "no flag, no secret in the body");
        assert_eq!(body.vectors.provider, "remote");
        assert_eq!(body.vectors.model.as_deref(), Some("nomic-embed-text"));
        assert_eq!(body.concurrency, Some(8));
    }

    #[test]
    fn a_cleared_secret_is_an_empty_secret() {
        let body = settings_body(
            &current(),
            Some("embedded".into()),
            None,
            None,
            None,
            Some(Secret::new("")),
            None,
            None,
            None,
            None,
            Some(2),
            Some("hosted".into()),
        );
        assert_eq!(body.summary.provider, "embedded");
        assert!(body.summary.secret.as_ref().is_some_and(|s| s.is_empty()));
        assert_eq!(body.concurrency, Some(2));
        assert_eq!(
            body.default_summaries,
            Some(kairos_client::types_repositories::CodeIndexSummaries::Hosted),
            "KAIROS-T-0358: the flag goes into the body"
        );
    }
}
