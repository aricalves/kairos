//! Shared per-invocation context for the command tree (KAIROS-T-0037): the
//! `--url/--tenant/--json` flags every API-touching leaf command carries,
//! and the `KairosClient` builder over the T-0036 credential cache +
//! refreshing token provider.

use std::sync::Arc;

use serde::Serialize;

use crate::auth::{self, Source};
use crate::credentials;
use crate::error::CliError;
use crate::provider::CachedTokenProvider;
use kairos_client::KairosClient;

/// Flags shared by every command that talks to the API.
#[derive(clap::Args, Debug, Default)]
pub struct Common {
    /// Deployment base URL (defaults to KAIROS_URL, then the only cached deployment)
    #[arg(long)]
    pub url: Option<String>,
    /// Tenant slug override (defaults to the tenant cached at login)
    #[arg(long)]
    pub tenant: Option<String>,
    /// Print JSON instead of the human-readable rendering. A list prints
    /// items, total and next_offset
    #[arg(long)]
    pub json: bool,
}

/// A `KairosClient` for the resolved deployment ([`auth::resolve`]): the
/// key in the environment, else (auto-refreshing) tokens from the
/// credential cache. Missing credentials are an auth error (exit 2) with
/// the `kairos login` instruction. `KAIROS_TIMEOUT` sets the request
/// timeout (KAIROS-T-0607); a value that is not a positive integer is a
/// failure (exit 1) that names the variable.
pub fn client(common: &Common) -> Result<KairosClient, CliError> {
    client_with_credential(common).map(|(client, _)| client)
}

/// [`client`], and the `credential:` line of `kairos whoami`
/// (KAIROS-T-0594). A key that hides a login for the same deployment gives
/// one warning line on stderr, unless `KAIROS_QUIET=1`.
pub fn client_with_credential(common: &Common) -> Result<(KairosClient, String), CliError> {
    let timeout = kairos_client::timeout_from_env()?;
    let path = credentials::credentials_path()?;
    let env = auth::Env::from_process();
    let source = auth::resolve(common.url.as_deref(), &env, || credentials::load(&path))?;
    if let Some(warning) = auth::override_warning(&source, &env, || credentials::load(&path)) {
        eprintln!("{warning}");
    }
    let credential = source.credential_line();
    let (mut client, cached_tenant) = match source {
        Source::Key {
            deployment,
            var,
            key,
        } => {
            auth::note_deployment(&deployment);
            (auth::key_client(&deployment, var, key), None)
        }
        Source::Cache { deployment, store } => {
            let entry = credentials::entry_for(&store, &deployment)?;
            auth::note_deployment(&deployment);
            let provider = Arc::new(CachedTokenProvider::new(path, deployment.clone()));
            (KairosClient::new(&deployment, provider), entry.tenant)
        }
    };
    if let Some(tenant) = common.tenant.clone().or(cached_tenant) {
        client = client.with_tenant(tenant);
    }
    if let Some(timeout) = timeout {
        client = client.with_timeout(timeout);
    }
    Ok((client, credential))
}

/// Pretty-print a DTO as JSON (the `--json` scripting surface).
pub fn print_json<T: Serialize>(value: &T) -> Result<(), CliError> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|err| CliError::Failure(format!("The CLI cannot write the JSON: {err}.")))?
    );
    Ok(())
}
