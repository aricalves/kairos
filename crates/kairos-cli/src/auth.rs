//! Where the bearer of a command comes from: a key in the environment, or
//! the credential cache of `kairos login`. One resolver for the commands
//! that talk to the API ([`crate::context::client`]) and for the base index
//! of `kairos index update`.
//!
//! - **The key.** `KAIROS_KEY`, else `KAIROS_MCP_KEY`: an agent key of a
//!   person (it acts as that person) or an API key of a service account.
//!   With a key, the CLI does not read the credential cache, and the key
//!   wins over a login. This is the route on a deployment whose issuer
//!   gives the CLI no device grant, for example Google.
//! - **The deployment.** `--url`, else `KAIROS_URL`, else the only
//!   deployment in the credential cache.
//! - **The warning.** A key that hides a login for the same deployment
//!   gives one line on stderr, unless `KAIROS_QUIET=1` (KAIROS-T-0594).
//!   `kairos whoami` names the source of the credential.

use std::fmt;
use std::sync::OnceLock;

use kairos_client::KairosClient;
use kairos_client::types_auth::REDACTED;

use crate::credentials::{self, CredentialStore};
use crate::error::CliError;

/// The variables that can hold the key, in order.
pub const KEY_VARS: [&str; 2] = ["KAIROS_KEY", "KAIROS_MCP_KEY"];
/// The variable that can hold the deployment URL.
pub const URL_VAR: &str = "KAIROS_URL";
/// The variable that, set to `1`, stops the warning of a key that hides a
/// login (KAIROS-T-0594).
pub const QUIET_VAR: &str = "KAIROS_QUIET";

/// The variable of the key that this process sends, when it sends one. The
/// message of a 401 names it in place of `kairos login`.
static KEY_IN_USE: OnceLock<&'static str> = OnceLock::new();

/// The variable of the key that this process sends, if any.
pub fn key_in_use() -> Option<&'static str> {
    KEY_IN_USE.get().copied()
}

/// The deployment that this process talks to, once a client is made. The
/// message of a 401 names it in the `kairos login` command (KAIROS-T-0588).
static DEPLOYMENT_IN_USE: OnceLock<String> = OnceLock::new();

/// Record the deployment of this process. The first call wins.
pub fn note_deployment(deployment: &str) {
    let _ = DEPLOYMENT_IN_USE.set(deployment.to_string());
}

/// The deployment that this process talks to, if a client was made.
pub fn deployment_in_use() -> Option<&'static str> {
    DEPLOYMENT_IN_USE.get().map(String::as_str)
}

/// The values of the environment that choose the deployment and the bearer.
#[derive(Clone, Default)]
pub struct Env {
    pub url: Option<String>,
    /// The name of the variable, and the key.
    pub key: Option<(&'static str, String)>,
    /// True when `KAIROS_QUIET` is `1`.
    pub quiet: bool,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("url", &self.url)
            .field("key", &self.key.as_ref().map(|(var, _)| (var, REDACTED)))
            .field("quiet", &self.quiet)
            .finish()
    }
}

impl Env {
    /// The values of the environment of this process.
    pub fn from_process() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The values from `lookup`. An empty or blank value counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        Self {
            url: get(URL_VAR),
            key: KEY_VARS
                .iter()
                .find_map(|var| get(var).map(|key| (*var, key.trim().to_string()))),
            quiet: get(QUIET_VAR).is_some_and(|value| value.trim() == "1"),
        }
    }
}

/// Where the bearer of a command comes from.
pub enum Source {
    /// A key from the environment.
    Key {
        deployment: String,
        var: &'static str,
        key: String,
    },
    /// The credential cache of `kairos login`.
    Cache {
        deployment: String,
        store: CredentialStore,
    },
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Key {
                deployment, var, ..
            } => f
                .debug_struct("Key")
                .field("deployment", deployment)
                .field("var", var)
                .field("key", &REDACTED)
                .finish(),
            Source::Cache { deployment, .. } => f
                .debug_struct("Cache")
                .field("deployment", deployment)
                .finish_non_exhaustive(),
        }
    }
}

impl Source {
    /// The `credential:` line of `kairos whoami` (KAIROS-T-0594). An agent
    /// key and a service-account key have one format, so the line says
    /// "key".
    pub fn credential_line(&self) -> String {
        match self {
            Source::Key { var, .. } => format!("credential: key from {var}"),
            Source::Cache { deployment, .. } => {
                format!("credential: login cached for {deployment}")
            }
        }
    }
}

/// The warning of a key that hides a login (KAIROS-T-0594). `None` when
/// the source is the cache, when `KAIROS_QUIET` is `1`, or when the cache
/// has no login for the deployment of the key. A cache that the CLI cannot
/// read gives no warning: the key does not need the cache.
pub fn override_warning(
    source: &Source,
    env: &Env,
    load: impl FnOnce() -> Result<CredentialStore, CliError>,
) -> Option<String> {
    let Source::Key {
        deployment, var, ..
    } = source
    else {
        return None;
    };
    if env.quiet {
        return None;
    }
    let store = load().ok()?;
    store.deployments.contains_key(deployment).then(|| {
        format!(
            "warning: {var} is set, so the CLI sends that key and not the login for \
             {deployment}. Set {QUIET_VAR}=1 to hide this warning."
        )
    })
}

/// Choose the deployment and the bearer. `url` is the `--url` of the
/// command; it wins over `KAIROS_URL`. `load` reads the credential cache.
/// With a key and a URL, the CLI does not read the cache.
pub fn resolve(
    url: Option<&str>,
    env: &Env,
    load: impl FnOnce() -> Result<CredentialStore, CliError>,
) -> Result<Source, CliError> {
    let url = url.map(str::to_string).or_else(|| env.url.clone());
    match &env.key {
        Some((var, key)) => {
            let deployment = match url {
                Some(url) => credentials::normalize_url(&url),
                None => key_deployment(var, load)?,
            };
            Ok(Source::Key {
                deployment,
                var,
                key: key.clone(),
            })
        }
        None => {
            let store = load()?;
            let deployment = credentials::resolve_deployment(&store, url.as_deref())?;
            Ok(Source::Cache { deployment, store })
        }
    }
}

/// The deployment of a key with no URL: the only login in the cache. The
/// error says what is wrong with the cache (KAIROS-T-0594).
fn key_deployment(
    var: &str,
    load: impl FnOnce() -> Result<CredentialStore, CliError>,
) -> Result<String, CliError> {
    let head =
        format!("{var} is set, but no URL names the deployment. Set {URL_VAR} or use --url.");
    let store = load().map_err(|err| {
        CliError::Auth(format!(
            "{head}\nThe CLI cannot find the deployment in the credential cache. {err}"
        ))
    })?;
    let mut deployments = store.deployments.keys();
    match (deployments.next(), deployments.next()) {
        (Some(only), None) => Ok(only.clone()),
        (None, _) => Err(CliError::Auth(format!(
            "{head}\nThe credential cache has no login."
        ))),
        (Some(_), Some(_)) => Err(CliError::Auth(format!(
            "{head}\nThe credential cache has logins for {} deployments.",
            store.deployments.len()
        ))),
    }
}

/// A client that sends the key from `var` as its bearer.
pub fn key_client(deployment: &str, var: &'static str, key: String) -> KairosClient {
    let _ = KEY_IN_USE.set(var);
    KairosClient::with_static_token(deployment, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::DeploymentCredentials;
    use crate::error::EXIT_AUTH;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> Env {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Env::from_lookup(|name| map.get(name).cloned())
    }

    fn store(urls: &[&str]) -> CredentialStore {
        let mut store = CredentialStore::default();
        for url in urls {
            store.deployments.insert(
                url.to_string(),
                DeploymentCredentials {
                    access_token: "cached".into(),
                    refresh_token: None,
                    expires_at: u64::MAX,
                    issuer: String::new(),
                    client_id: String::new(),
                    tenant: None,
                    api_bearer: Default::default(),
                    kind: Default::default(),
                    email: None,
                },
            );
        }
        store
    }

    fn no_cache() -> Result<CredentialStore, CliError> {
        panic!("the CLI must not read the cache here")
    }

    #[test]
    fn kairos_key_wins_over_kairos_mcp_key_and_blank_is_unset() {
        let both = env(&[("KAIROS_KEY", "a"), ("KAIROS_MCP_KEY", "b")]);
        assert_eq!(
            both.key.as_ref().map(|(v, k)| (*v, k.as_str())),
            Some(("KAIROS_KEY", "a"))
        );
        let blank = env(&[("KAIROS_KEY", "  "), ("KAIROS_MCP_KEY", "b")]);
        assert_eq!(blank.key.as_ref().map(|(v, _)| *v), Some("KAIROS_MCP_KEY"));
        assert!(env(&[]).key.is_none());
    }

    #[test]
    fn a_key_with_a_url_does_not_read_the_cache() {
        let env = env(&[
            ("KAIROS_KEY", "kairos_sk_x"),
            ("KAIROS_URL", "https://env/"),
        ]);
        match resolve(None, &env, no_cache).unwrap() {
            Source::Key {
                deployment,
                var,
                key,
            } => {
                assert_eq!(deployment, "https://env");
                assert_eq!(var, "KAIROS_KEY");
                assert_eq!(key, "kairos_sk_x");
            }
            other => panic!("expected a key, got {other:?}"),
        }
    }

    #[test]
    fn the_url_flag_wins_over_kairos_url() {
        let env = env(&[("KAIROS_MCP_KEY", "k"), ("KAIROS_URL", "https://env")]);
        let Source::Key { deployment, .. } =
            resolve(Some("https://flag/"), &env, no_cache).unwrap()
        else {
            panic!("expected a key");
        };
        assert_eq!(deployment, "https://flag");
    }

    #[test]
    fn a_key_with_no_url_uses_the_only_cached_deployment() {
        let env = env(&[("KAIROS_KEY", "k")]);
        let Source::Key { deployment, .. } =
            resolve(None, &env, || Ok(store(&["https://cached"]))).unwrap()
        else {
            panic!("expected a key");
        };
        assert_eq!(deployment, "https://cached");
    }

    #[test]
    fn a_key_with_no_url_and_no_single_deployment_is_an_auth_error() {
        let env = env(&[("KAIROS_KEY", "k")]);
        for cached in [vec![], vec!["https://a", "https://b"]] {
            let err = resolve(None, &env, || Ok(store(&cached))).unwrap_err();
            assert_eq!(err.exit_code(), EXIT_AUTH);
            let message = err.to_string();
            assert!(message.contains("KAIROS_KEY is set"), "{message}");
            assert!(message.contains("KAIROS_URL"), "{message}");
        }
    }

    /// KAIROS-T-0594: with a key and no URL, the error says what is wrong
    /// with the cache.
    #[test]
    fn a_key_with_no_url_says_what_is_wrong_with_the_cache() {
        let env = env(&[("KAIROS_MCP_KEY", "k")]);
        let corrupted = || {
            Err(CliError::Auth(
                "The credential cache at /c/credentials.json is corrupted. The file is not \
                 valid JSON: x."
                    .into(),
            ))
        };
        let message = resolve(None, &env, corrupted).unwrap_err().to_string();
        assert!(message.contains("KAIROS_MCP_KEY is set"), "{message}");
        assert!(
            message.contains("is corrupted. The file is not valid JSON"),
            "{message}"
        );
        assert!(!message.contains("does not know"), "{message}");

        let message = resolve(None, &env, || Ok(store(&[])))
            .unwrap_err()
            .to_string();
        assert!(message.contains("has no login"), "{message}");
        let message = resolve(None, &env, || Ok(store(&["https://a", "https://b"])))
            .unwrap_err()
            .to_string();
        assert!(message.contains("logins for 2 deployments"), "{message}");
    }

    /// KAIROS-T-0594: `whoami` names the source of the credential.
    #[test]
    fn the_credential_line_names_the_source() {
        let key = resolve(
            None,
            &env(&[("KAIROS_MCP_KEY", "k"), ("KAIROS_URL", "https://x")]),
            no_cache,
        )
        .unwrap();
        assert_eq!(key.credential_line(), "credential: key from KAIROS_MCP_KEY");
        let cache = resolve(None, &Env::default(), || Ok(store(&["https://a"]))).unwrap();
        assert_eq!(
            cache.credential_line(),
            "credential: login cached for https://a"
        );
    }

    /// KAIROS-T-0594: a key that hides a login for its deployment gives one
    /// warning line, unless KAIROS_QUIET is 1.
    #[test]
    fn a_key_that_hides_a_login_gives_a_warning() {
        let loud = env(&[("KAIROS_MCP_KEY", "k"), ("KAIROS_URL", "https://a/")]);
        let source = resolve(None, &loud, no_cache).unwrap();
        let warning =
            override_warning(&source, &loud, || Ok(store(&["https://a"]))).expect("a warning");
        assert_eq!(
            warning,
            "warning: KAIROS_MCP_KEY is set, so the CLI sends that key and not the login for \
             https://a. Set KAIROS_QUIET=1 to hide this warning."
        );
        assert!(!warning.contains('\n'));

        // No login for this deployment, no cache, a cache that cannot be read.
        assert!(override_warning(&source, &loud, || Ok(store(&["https://b"]))).is_none());
        assert!(override_warning(&source, &loud, || Ok(store(&[]))).is_none());
        assert!(override_warning(&source, &loud, || Err(CliError::Auth("bad".into()))).is_none());

        // KAIROS_QUIET=1 hides it; another value does not.
        let quiet = env(&[
            ("KAIROS_MCP_KEY", "k"),
            ("KAIROS_URL", "https://a"),
            ("KAIROS_QUIET", "1"),
        ]);
        assert!(quiet.quiet);
        assert!(override_warning(&source, &quiet, || Ok(store(&["https://a"]))).is_none());
        assert!(!env(&[("KAIROS_QUIET", "0")]).quiet);

        // The cache is the source: no warning, and the cache is not read.
        let cache = resolve(None, &Env::default(), || Ok(store(&["https://a"]))).unwrap();
        assert!(override_warning(&cache, &loud, no_cache).is_none());
    }

    #[test]
    fn no_key_uses_the_cache_and_kairos_url_selects_the_deployment() {
        let env = env(&[("KAIROS_URL", "https://b/")]);
        match resolve(None, &env, || Ok(store(&["https://a", "https://b"]))).unwrap() {
            Source::Cache { deployment, .. } => assert_eq!(deployment, "https://b"),
            other => panic!("expected the cache, got {other:?}"),
        }
    }

    #[test]
    fn no_key_and_no_login_says_to_log_in() {
        let err = resolve(None, &Env::default(), || Ok(store(&[]))).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_AUTH);
        assert!(err.to_string().contains("kairos login"));
    }

    #[test]
    fn debug_never_prints_the_key() {
        let env = env(&[("KAIROS_KEY", "kairos_sk_secret")]);
        assert!(!format!("{env:?}").contains("kairos_sk_secret"));
        let source = resolve(Some("https://x"), &env, no_cache).unwrap();
        assert!(!format!("{source:?}").contains("kairos_sk_secret"));
    }
}
