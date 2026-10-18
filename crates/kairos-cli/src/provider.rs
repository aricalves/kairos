//! [`CachedTokenProvider`] — the CLI's implementation of
//! `kairos_client::TokenProvider` (the KAIROS-T-0024 seam): draws the
//! bearer token from the credential cache and transparently refreshes it
//! on (imminent) expiry, persisting rotated tokens back to disk. A refresh
//! that the issuer refuses surfaces as `Error::Token` with a re-login
//! instruction, which `main` maps to exit code 2 (KAIROS-A-0015). An issuer
//! that the CLI cannot reach is `Error::IssuerUnreachable`, exit code 1
//! (KAIROS-T-0588): the login stays.
//!
//! A refresh holds the lock of the cache ([`credentials::lock`],
//! KAIROS-T-0594), so two processes do not send one refresh token twice.
//!
//! A local session (COLLIERY-T-0213) is not refreshed. It is used until it
//! expires, and then the same `Error::Token` gives the command to log in
//! again.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use kairos_client::{Error, TokenProvider};
use tokio::sync::Mutex;

use crate::credentials::{self, DeploymentCredentials, unix_now};
use crate::oidc;

/// A refreshing token provider over one deployment's cache entry.
pub struct CachedTokenProvider {
    /// `credentials.json`.
    path: PathBuf,
    /// Normalized deployment URL (the cache key).
    deployment: String,
    http: reqwest::Client,
    /// Serializes the requests of this process. The lock file of the cache
    /// serializes the refreshes of all processes.
    refresh_lock: Mutex<()>,
}

impl CachedTokenProvider {
    pub fn new(path: PathBuf, deployment: String) -> Self {
        Self {
            path,
            deployment,
            http: reqwest::Client::new(),
            refresh_lock: Mutex::new(()),
        }
    }

    /// The current access token, refreshed (and persisted) when it is
    /// expired or inside the skew window.
    async fn current_token(&self) -> Result<String, Error> {
        let _guard = self.refresh_lock.lock().await;
        // Re-read the cache on every request: another process (or an
        // earlier refresh in this one) may have rotated the tokens.
        let (_, entry) = self.read_entry()?;

        // A local session (COLLIERY-T-0213) has no issuer and no refresh
        // token, so it never reaches the refresh below. When it is over, the
        // request is not sent: the server would refuse the bearer, and the
        // 401 cannot name the command that the person needs.
        if entry.is_local_session() {
            if entry.is_expired(unix_now()) {
                return Err(Error::Token(
                    entry.session_expired_message(&self.deployment),
                ));
            }
            return Ok(entry.access_token);
        }

        if !entry.needs_refresh(unix_now()) {
            return Ok(entry.access_token);
        }

        // KAIROS-T-0594: take the lock of the cache, and read the cache
        // again. Another process can have refreshed the token while this
        // one waited; then its token is good, and its refresh token is the
        // only one that the issuer still accepts.
        let _cache_lock = self.lock_cache().await?;
        let (mut store, entry) = self.read_entry()?;
        if !entry.needs_refresh(unix_now()) {
            return Ok(entry.access_token);
        }

        let Some(refresh_token) = entry.refresh_token.as_deref() else {
            return Err(Error::Token(format!(
                "The access token for {url} expired, and the cache has no refresh token.\n\
                 Run `kairos login --url {url}` to log in again.",
                url = self.deployment
            )));
        };

        let endpoints = oidc::discover_endpoints(&self.http, &entry.issuer)
            .await
            .map_err(|err| self.refresh_error(err))?;
        let refreshed = oidc::refresh_grant(
            &self.http,
            &endpoints.token_endpoint,
            &entry.client_id,
            refresh_token,
        )
        .await
        .map_err(|err| self.refresh_error(err))?;

        // Re-select the same bearer kind the session was established with
        // (T-0054): an `id_token` deployment must keep sending the id_token.
        let mut updated = entry;
        let new_bearer = refreshed
            .bearer_for(updated.api_bearer)
            .ok_or_else(|| {
                Error::Token(format!(
                    "The refresh of the session for {url} gave no id_token. This deployment \
                     must have an id_token.\n\
                     Run `kairos login --url {url}` to log in again.",
                    url = self.deployment
                ))
            })?
            .to_string();
        updated.access_token = new_bearer.clone();
        updated.expires_at = unix_now() + refreshed.expires_in;
        // Issuers may rotate the refresh token; keep the old one when the
        // response omits it.
        if refreshed.refresh_token.is_some() {
            updated.refresh_token = refreshed.refresh_token;
        }
        store.deployments.insert(self.deployment.clone(), updated);
        credentials::save(&self.path, &store).map_err(|err| Error::Token(err.to_string()))?;

        Ok(new_bearer)
    }
}

impl CachedTokenProvider {
    /// The cache, and the entry of this deployment.
    fn read_entry(&self) -> Result<(credentials::CredentialStore, DeploymentCredentials), Error> {
        let store = credentials::load(&self.path).map_err(|err| Error::Token(err.to_string()))?;
        let entry = credentials::entry_for(&store, &self.deployment)
            .map_err(|err| Error::Token(err.to_string()))?;
        Ok((store, entry))
    }

    /// The lock of the cache. The wait for it runs on a blocking thread, so
    /// it does not stop the runtime.
    async fn lock_cache(&self) -> Result<credentials::CacheLock, Error> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || credentials::lock(&path))
            .await
            .map_err(|err| {
                Error::Token(format!("The CLI cannot lock the credential cache: {err}."))
            })?
            .map_err(|err| Error::Token(err.to_string()))
    }

    /// The error of a refresh that gave no token (KAIROS-T-0588). An issuer
    /// that the CLI cannot reach is a transport failure (exit 1): the login
    /// stays, and a new login would not help. A refusal of the issuer is the
    /// end of the session (exit 2): the person must log in again.
    fn refresh_error(&self, err: oidc::IssuerError) -> Error {
        match err {
            oidc::IssuerError::Unreachable(text) => Error::IssuerUnreachable(format!(
                "The CLI cannot refresh the access token for {url}: {text}\n\
                 Make sure that the network connection works. Then run the command again.",
                url = self.deployment
            )),
            oidc::IssuerError::Refused(text) => Error::Token(format!(
                "The session for {url} expired, and the CLI could not refresh it ({text}).\n\
                 Run `kairos login --url {url}` to log in again.",
                url = self.deployment
            )),
        }
    }
}

impl TokenProvider for CachedTokenProvider {
    fn bearer_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
        Box::pin(self.current_token())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::CredentialStore;
    use kairos_client::types_auth::Secret;

    const DEPLOYMENT: &str = "http://one.kairos.test";
    const BEARER: &str =
        "kairos_ss_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn provider_with(name: &str, expires_at: u64) -> (CachedTokenProvider, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("kairos-cli-provider-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("credentials.json");
        let mut store = CredentialStore::default();
        store.deployments.insert(
            DEPLOYMENT.into(),
            DeploymentCredentials::local_session(
                &Secret::new(BEARER),
                expires_at,
                "ada@example.test".into(),
                None,
            ),
        );
        credentials::save(&path, &store).expect("save");
        (CachedTokenProvider::new(path, DEPLOYMENT.to_string()), dir)
    }

    /// COLLIERY-T-0213: a local session that is not over gives its bearer.
    #[tokio::test]
    async fn a_live_local_session_gives_its_bearer() {
        let (provider, dir) = provider_with("live", unix_now() + 3600);
        assert_eq!(provider.current_token().await.expect("bearer"), BEARER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// COLLIERY-T-0213: a local session that is over gives no bearer, so no
    /// request goes out. The error names the login command, and it does not
    /// speak of a refresh: the entry has an empty issuer, and a refresh
    /// would try discovery against the empty string.
    #[tokio::test]
    async fn an_expired_local_session_gives_the_login_command() {
        let (provider, dir) = provider_with("expired", 1);
        let err = provider.current_token().await.expect_err("expired");
        let message = match err {
            Error::Token(message) => message,
            other => panic!("expected a token error, got {other}"),
        };
        assert_eq!(
            message,
            "The session for http://one.kairos.test expired.\n\
             Run `kairos login --url http://one.kairos.test --email ada@example.test` to log \
             in again."
        );
        assert!(!message.contains("refresh"), "{message}");
        assert!(!message.contains(BEARER), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KAIROS-T-0594: a refresh waits for the lock of the cache. When the
    /// holder of the lock wrote a new token, the refresh uses that token and
    /// does not go to the issuer.
    ///
    /// The issuer of the entry is a closed port: a refresh that does not
    /// wait fails at once with `IssuerUnreachable`. With the lock held, the
    /// call cannot end, whatever the timing of the threads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refresh_waits_for_the_lock_and_reads_the_cache_again() {
        let dir =
            std::env::temp_dir().join(format!("kairos-cli-provider-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("credentials.json");
        let expired = DeploymentCredentials {
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: 1,
            issuer: "http://127.0.0.1:9".into(),
            client_id: "kairos-cli".into(),
            tenant: None,
            api_bearer: Default::default(),
            kind: Default::default(),
            email: None,
        };
        let mut store = CredentialStore::default();
        store.deployments.insert(DEPLOYMENT.into(), expired.clone());
        credentials::save(&path, &store).expect("save");

        // Another process holds the lock and refreshes.
        let held = credentials::lock(&path).expect("lock");
        let provider = std::sync::Arc::new(CachedTokenProvider::new(
            path.clone(),
            DEPLOYMENT.to_string(),
        ));
        let task = tokio::spawn({
            let provider = provider.clone();
            async move { provider.current_token().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!task.is_finished(), "the refresh must wait for the lock");

        let mut fresh = expired;
        fresh.access_token = "new-access".into();
        fresh.refresh_token = Some("new-refresh".into());
        fresh.expires_at = unix_now() + 3600;
        store.deployments.insert(DEPLOYMENT.into(), fresh);
        credentials::save(&path, &store).expect("save the refreshed token");
        drop(held);

        let token = task.await.expect("join").expect("the token of the holder");
        assert_eq!(token, "new-access");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KAIROS-T-0588: an issuer that the CLI cannot reach is a transport
    /// failure, not the end of the session. The text does not say to log in
    /// again.
    #[test]
    fn an_unreachable_issuer_is_not_a_login_error() {
        let (provider, dir) = provider_with("unreachable", unix_now() + 3600);
        let err = provider.refresh_error(oidc::IssuerError::Unreachable(
            "The CLI cannot reach the OIDC issuer at http://issuer.test: no route.".into(),
        ));
        let Error::IssuerUnreachable(text) = err else {
            panic!("expected IssuerUnreachable, got {err}");
        };
        assert!(text.contains("http://issuer.test"), "{text}");
        assert!(text.contains("network connection"), "{text}");
        assert!(!text.contains("kairos login"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// KAIROS-T-0588: a refusal of the issuer is the end of the session. The
    /// text gives the login command.
    #[test]
    fn a_refused_refresh_is_a_login_error() {
        let (provider, dir) = provider_with("refused", unix_now() + 3600);
        let err = provider.refresh_error(oidc::IssuerError::Refused(
            "The issuer did not accept the refresh (400 Bad Request): invalid_grant".into(),
        ));
        let Error::Token(text) = err else {
            panic!("expected Token, got {err}");
        };
        assert!(text.contains("could not refresh it"), "{text}");
        assert!(
            text.contains("kairos login --url http://one.kairos.test"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
