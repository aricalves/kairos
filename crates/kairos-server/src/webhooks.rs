//! Signed webhook deliveries (KAIROS-T-0384, COLLIERY-I-0614).
//!
//! A delivery is a job of the job queue (kind `webhook_delivery`,
//! [`kairos_db::webhooks`]). The [`WebhookService`] runs those jobs: it
//! sends the body of the delivery with a `POST` to the URL of the target,
//! signed with the secret of the target, and writes each try in the log.
//!
//! # The request
//!
//! - `Content-Type: application/json`, and the body of the delivery.
//! - [`SIGNATURE_HEADER`] `X-Kairos-Signature: sha256=<hex>`: the
//!   HMAC-SHA256 of the body bytes, with the secret of the target as the
//!   key. A receiver computes the same HMAC and compares the 2 values in
//!   constant time.
//! - [`DELIVERY_HEADER`] `X-Kairos-Delivery`: the id of the delivery. It is
//!   the same on each try, so a receiver can drop a second copy.
//! - [`EVENT_HEADER`] `X-Kairos-Event`: the event.
//!
//! A response with a 2xx code is a good try. Any other code, a time-out, or
//! an error of the connection is a failed try. Kairos does not follow a
//! redirect.
//!
//! # Tries
//!
//! A failed try is tried again after a wait that doubles at each try
//! ([`WebhookSettings::wait_after`]: 30 s, 1 min, 2 min, … to at most 12
//! h), until [`WebhookSettings::max_tries`] (12: about 17 h of tries). Then
//! the delivery is `failed`.
//!
//! Each failed try adds one to the failures in a row of the target; a good
//! try sets them to 0. At [`WebhookSettings::disable_after`] the target is
//! turned off, and its owners get a notification (`delivery_failing`).
//!
//! # The guard of the addresses
//!
//! Only `http` and `https` URLs, with no user name or password. Kairos
//! resolves the host before each try, and refuses the try when an address
//! is loopback, private, link-local, shared (100.64.0.0/10), multicast or
//! unspecified ([`is_blocked`]), unless
//! [`WebhookSettings::allow_private`] (`KAIROS_WEBHOOK_ALLOW_PRIVATE`) is
//! on. The request then goes to the address that was checked, so a second
//! answer of the DNS cannot send it elsewhere.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use kairos_db::webhooks::{self as db, NewTarget, SealedSecret, TryResult};
use sha2::Sha256;
use uuid::Uuid;

use crate::blocking::BlockingTenantPool;
use crate::error::ApiError;
use crate::jobs::{JobError, JobFuture, JobHandle, JobHandler, UnitFuture, Worker, kinds};
use crate::secrets::SecretsKey;

/// The header of the signature: `sha256=<hex of the HMAC-SHA256 of the
/// body>`.
pub const SIGNATURE_HEADER: &str = "X-Kairos-Signature";
/// The header of the id of the delivery.
pub const DELIVERY_HEADER: &str = "X-Kairos-Delivery";
/// The header of the event.
pub const EVENT_HEADER: &str = "X-Kairos-Event";

/// The start of each secret that Kairos makes.
pub const SECRET_PREFIX: &str = "kairos_whsec_";

/// The settings of the webhook deliveries (`KAIROS_WEBHOOK_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookSettings {
    /// `KAIROS_WEBHOOK_MAX_TRIES` (12): the tries of a delivery.
    pub max_tries: i32,
    /// `KAIROS_WEBHOOK_RETRY_BASE_SECS` (30): the wait after the first
    /// failed try. Each wait is 2 times the wait before it.
    pub retry_base: Duration,
    /// The longest wait between 2 tries (12 h).
    pub retry_cap: Duration,
    /// `KAIROS_WEBHOOK_TIMEOUT_SECS` (10): the time of one try.
    pub timeout: Duration,
    /// `KAIROS_WEBHOOK_DISABLE_AFTER` (24): the failed tries in a row that
    /// turn a target off. 0: never.
    pub disable_after: i32,
    /// `KAIROS_WEBHOOK_ALLOW_PRIVATE` (false): allow the addresses that
    /// [`is_blocked`] refuses (a receiver on the same host or network).
    pub allow_private: bool,
}

impl Default for WebhookSettings {
    fn default() -> Self {
        WebhookSettings {
            max_tries: db::DEFAULT_MAX_TRIES,
            retry_base: Duration::from_secs(30),
            retry_cap: Duration::from_secs(12 * 60 * 60),
            timeout: Duration::from_secs(10),
            disable_after: 24,
            allow_private: false,
        }
    }
}

impl WebhookSettings {
    /// The wait after the failed try `try_number` (1 is the first try):
    /// [`Self::retry_base`] times 2 to the power `try_number - 1`, at most
    /// [`Self::retry_cap`].
    pub fn wait_after(&self, try_number: i32) -> Duration {
        let exponent = u32::try_from(try_number.saturating_sub(1))
            .unwrap_or(0)
            .min(30);
        self.retry_base
            .checked_mul(1u32 << exponent)
            .unwrap_or(self.retry_cap)
            .min(self.retry_cap)
    }
}

/// A new secret: [`SECRET_PREFIX`] and 32 random bytes in hex.
pub fn generate_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{SECRET_PREFIX}{hex}")
}

/// The value of [`SIGNATURE_HEADER`] for `body`: `sha256=` and the hex of
/// the HMAC-SHA256 of the body, with `secret` (its UTF-8 bytes) as the key.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

/// True when the guard refuses `ip`: loopback, private (10/8, 172.16/12,
/// 192.168/16, fc00::/7), link-local (169.254/16, fe80::/10, which holds the
/// metadata service of the clouds), shared (100.64/10), multicast,
/// broadcast, unspecified, and the IPv4 addresses inside IPv6.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                || a >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                // The IPv4-compatible addresses (::a.b.c.d).
                || v6.segments()[..6].iter().all(|s| *s == 0)
        }
    }
}

/// Check the form of the URL of a target: `http` or `https`, a host, no
/// user name or password. A host that is an address, or `localhost`, is
/// checked with [`is_blocked`] unless `allow_private`. The error is a
/// sentence for the person who sent the URL.
pub fn check_url(raw: &str, allow_private: bool) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|e| format!("The URL {raw:?} is not a URL: {e}."))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "The URL must start with http:// or https://, and it starts with {}://.",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The URL must not have a user name or a password.".into());
    }
    let host = match url.host() {
        Some(host) => host,
        None => return Err("The URL has no host.".into()),
    };
    if !allow_private {
        let blocked = match host {
            url::Host::Ipv4(ip) => is_blocked(IpAddr::V4(ip)),
            url::Host::Ipv6(ip) => is_blocked(IpAddr::V6(ip)),
            url::Host::Domain(name) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                name == "localhost" || name.ends_with(".localhost")
            }
        };
        if blocked {
            return Err(private_refusal(&host.to_string()));
        }
    }
    Ok(url)
}

fn private_refusal(host: &str) -> String {
    format!(
        "The host {host} is a loopback, private, link-local or shared address. Kairos does not \
         send a delivery there unless the deployment sets KAIROS_WEBHOOK_ALLOW_PRIVATE=true."
    )
}

/// Resolve the host of `url`, and check each address with [`is_blocked`]
/// unless `allow_private`. The first address, for the request.
async fn resolve(url: &reqwest::Url, allow_private: bool) -> Result<SocketAddr, String> {
    let host = url.host_str().ok_or("The URL has no host.")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url.port_or_known_default().ok_or("The URL has no port.")?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("The host {host} does not resolve: {e}."))?
        .collect();
    let first = *addrs
        .first()
        .ok_or_else(|| format!("The host {host} has no address."))?;
    if !allow_private && addrs.iter().any(|a| is_blocked(a.ip())) {
        return Err(private_refusal(host));
    }
    Ok(first)
}

/// Seal a new secret for a new target, and the row to add. The plain
/// secret is the second value: the caller shows it one time.
pub fn new_target(
    key: &SecretsKey,
    tenant: &str,
    board_id: Uuid,
    name: &str,
    url: &str,
    created_by: Uuid,
) -> (NewTarget, String) {
    let id = Uuid::new_v4();
    let secret = generate_secret();
    let sealed = key.seal(
        &crate::secrets::webhook_target_aad(tenant, id),
        secret.as_bytes(),
    );
    (
        NewTarget {
            id,
            board_id,
            name: name.trim().to_string(),
            url: url.trim().to_string(),
            secret: SealedSecret {
                ciphertext: sealed.ciphertext,
                nonce: sealed.nonce,
                key_id: sealed.key_id,
            },
            created_by,
        },
        secret,
    )
}

/// The body with each `data.link` that is a path (`/items/<code>`) made a
/// URL with `public_url`. `None`: the body does not change (no link that is
/// a path, or no `public_url`).
pub fn absolute_links(body: &str, public_url: Option<&str>) -> Option<String> {
    let base = public_url?;
    let mut value: serde_json::Value = serde_json::from_str(body).ok()?;
    let link = value.get_mut("data")?.get_mut("link")?;
    let path = link.as_str()?;
    if !path.starts_with('/') {
        return None;
    }
    *link = serde_json::Value::String(format!("{base}{path}"));
    Some(value.to_string())
}

/// What one try gave.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    ok: bool,
    code: Option<u16>,
    error: Option<String>,
}

/// The runner of the delivery jobs.
#[derive(Clone)]
pub struct WebhookService {
    blocking: BlockingTenantPool,
    secrets: Option<SecretsKey>,
    settings: WebhookSettings,
    /// `KAIROS_PUBLIC_URL`: a `link` of the data that is a path becomes a
    /// URL ([`absolute_links`]).
    public_url: Option<String>,
}

impl std::fmt::Debug for WebhookService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookService")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl WebhookService {
    /// A runner that opens the secrets with `secrets`.
    pub fn new(
        blocking: BlockingTenantPool,
        secrets: Option<SecretsKey>,
        settings: WebhookSettings,
    ) -> Self {
        WebhookService {
            blocking,
            secrets,
            settings,
            public_url: None,
        }
    }

    /// The base URL of the links (`KAIROS_PUBLIC_URL`, KAIROS-T-0385).
    pub fn with_public_url(mut self, public_url: Option<String>) -> Self {
        self.public_url = public_url.map(|url| url.trim_end_matches('/').to_string());
        self
    }

    /// Run the deliveries on `worker`.
    pub fn register(self, worker: &Worker) {
        worker.register(kinds::WEBHOOK_DELIVERY, Arc::new(self));
    }

    /// The settings.
    pub fn settings(&self) -> &WebhookSettings {
        &self.settings
    }

    /// Send `body` to `url` once.
    async fn send(&self, url: &str, secret: &str, delivery: &db::Delivery) -> Sent {
        let failed = |error: String| Sent {
            ok: false,
            code: None,
            error: Some(error),
        };
        let url = match check_url(url, self.settings.allow_private) {
            Ok(url) => url,
            Err(e) => return failed(e),
        };
        let addr = match resolve(&url, self.settings.allow_private).await {
            Ok(addr) => addr,
            Err(e) => return failed(e),
        };
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.settings.timeout)
            .user_agent(concat!("Kairos-Webhook/", env!("CARGO_PKG_VERSION")));
        if let Some(url::Host::Domain(domain)) = url.host() {
            builder = builder.resolve(domain, addr);
        }
        let client = match builder.build() {
            Ok(client) => client,
            Err(e) => return failed(format!("The HTTP client did not start: {e}.")),
        };
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                SIGNATURE_HEADER,
                signature(secret, delivery.body.as_bytes()),
            )
            .header(DELIVERY_HEADER, delivery.id.to_string())
            .header(EVENT_HEADER, &delivery.event)
            .body(delivery.body.clone())
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status();
                Sent {
                    ok: status.is_success(),
                    code: Some(status.as_u16()),
                    error: (!status.is_success()).then(|| format!("The target answered {status}.")),
                }
            }
            // The URL can hold a token of the receiver: the error has no URL.
            Err(e) if e.is_timeout() => failed(format!(
                "The target did not answer in {} s.",
                self.settings.timeout.as_secs()
            )),
            Err(e) => failed(format!("The request failed: {}.", e.without_url())),
        }
    }

    async fn deliver(self, handle: JobHandle) -> Result<(), JobError> {
        let job = handle.job().clone();
        let tenant = job
            .tenant
            .clone()
            .ok_or_else(|| JobError::fail("The delivery has no organization."))?;
        let delivery_id = job.payload_json()["delivery_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| JobError::fail("The payload of the delivery has no delivery_id."))?;
        let (id, instance, max_tries) = (
            job.id,
            job.leased_by.unwrap_or_default(),
            self.settings.max_tries,
        );
        // The tries of this server's setting.
        let _ = self
            .blocking
            .run_public(move |conn| {
                kairos_db::jobs::set_max_tries(conn, id, instance, max_tries)
                    .map_err(ApiError::internal)
            })
            .await;
        let loaded = self
            .blocking
            .run(&tenant, move |conn| {
                let Some(delivery) =
                    db::get_delivery(conn, delivery_id).map_err(ApiError::internal)?
                else {
                    return Ok(None);
                };
                let target =
                    db::get_target(conn, delivery.target_id).map_err(ApiError::internal)?;
                let secret =
                    db::target_secret(conn, delivery.target_id).map_err(ApiError::internal)?;
                Ok(target.zip(secret).map(|(t, s)| (delivery, t, s)))
            })
            .await
            .map_err(|e| JobError::retry(format!("The database did not answer: {}", e.message)))?;
        let Some((delivery, target, sealed)) = loaded else {
            // The target was removed, with its deliveries.
            return Ok(());
        };
        if delivery.status != "pending" {
            return Ok(());
        }
        if !target.enabled {
            self.finish(
                &tenant,
                delivery_id,
                "skipped",
                "The target is off. A board manager turns it on again.",
            )
            .await;
            return Ok(());
        }
        let secret = match &self.secrets {
            None => {
                let reason = "The deployment has no KAIROS_SECRETS_KEY, so Kairos cannot open \
                              the secret of the target.";
                self.finish(&tenant, delivery_id, "failed", reason).await;
                return Err(JobError::fail(reason));
            }
            Some(key) => match key.open(
                &crate::secrets::webhook_target_aad(&tenant, target.id),
                &sealed.key_id,
                &sealed.nonce,
                &sealed.ciphertext,
            ) {
                Ok(plain) => String::from_utf8(plain).unwrap_or_default(),
                Err(e) => {
                    let reason =
                        format!("The secret of the target does not open: {e} Make a new target.");
                    self.finish(&tenant, delivery_id, "failed", &reason).await;
                    return Err(JobError::fail(reason));
                }
            },
        };
        // KAIROS-T-0385: the link of the data is a path in the database; the
        // body that is sent, and kept for each next try, has the URL.
        let mut delivery = delivery;
        if let Some(body) = absolute_links(&delivery.body, self.public_url.as_deref()) {
            let kept = body.clone();
            let written = self
                .blocking
                .run(&tenant, move |conn| {
                    db::set_body(conn, delivery_id, &kept).map_err(ApiError::internal)
                })
                .await;
            if let Err(e) = written {
                return Err(JobError::retry(format!(
                    "The database did not answer: {}",
                    e.message
                )));
            }
            delivery.body = body;
        }
        handle.progress();
        let started = Instant::now();
        let sent = self.send(&target.url, &secret, &delivery).await;
        handle.progress();
        let try_number = job.tries;
        let last = !sent.ok && try_number >= self.settings.max_tries;
        let wait = self.settings.wait_after(try_number);
        let result = TryResult {
            try_number,
            ok: sent.ok,
            response_code: sent.code.map(i32::from),
            error: sent.error.clone(),
            duration_ms: i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX),
            last,
            next_try_at: (!sent.ok && !last)
                .then(|| chrono::Utc::now() + chrono::Duration::from_std(wait).unwrap_or_default()),
        };
        let disable_after = self.settings.disable_after;
        let recorded = self
            .blocking
            .run(&tenant, move |conn| {
                db::record_try(conn, delivery_id, &result, disable_after)
                    .map_err(ApiError::internal)
            })
            .await;
        let turned_off = match recorded {
            Ok(Some(recorded)) => recorded.target_turned_off,
            Ok(None) => return Ok(()),
            // The try ran; the log did not take it. The next try writes.
            Err(e) => {
                tracing::warn!(delivery = %delivery_id, error = ?e, "webhook try not written");
                false
            }
        };
        if sent.ok {
            tracing::info!(tenant = %tenant, delivery = %delivery_id, try_ = try_number, "webhook delivered");
            return Ok(());
        }
        let reason = sent.error.unwrap_or_else(|| "The try failed.".into());
        tracing::warn!(tenant = %tenant, delivery = %delivery_id, try_ = try_number, %reason, "webhook try failed");
        if last || turned_off {
            return Err(JobError::fail(reason));
        }
        Err(JobError::retry_in(reason, wait))
    }

    async fn finish(&self, tenant: &str, delivery_id: Uuid, status: &'static str, reason: &str) {
        let reason = reason.to_string();
        if let Err(e) = self
            .blocking
            .run(tenant, move |conn| {
                db::finish_delivery(conn, delivery_id, status, &reason).map_err(ApiError::internal)
            })
            .await
        {
            tracing::warn!(delivery = %delivery_id, error = ?e, "webhook delivery end not written");
        }
    }
}

impl JobHandler for WebhookService {
    fn run(&self, handle: JobHandle) -> JobFuture {
        Box::pin(self.clone().deliver(handle))
    }

    /// The lease of the last try ran out: the delivery fails with the
    /// reason of the job.
    fn failed(&self, job: kairos_db::jobs::Job) -> UnitFuture {
        let service = self.clone();
        Box::pin(async move {
            let (Some(tenant), Some(id)) = (
                job.tenant.clone(),
                job.payload_json()["delivery_id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok()),
            ) else {
                return;
            };
            let reason = job
                .last_error
                .clone()
                .unwrap_or_else(|| "The delivery used its tries.".into());
            service.finish(&tenant, id, "failed", &reason).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_that_is_a_path_becomes_a_url() {
        let body = r#"{"data":{"code":"K-T-1","link":"/items/K-T-1"},"event":"x","id":"1"}"#;
        let out = absolute_links(body, Some("https://kairos.example.com")).expect("changed");
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value["data"]["link"],
            "https://kairos.example.com/items/K-T-1"
        );
        assert_eq!(value["data"]["code"], "K-T-1");
        assert_eq!(absolute_links(body, None), None);
        assert_eq!(absolute_links(&out, Some("https://other.example")), None);
        assert_eq!(
            absolute_links(r#"{"data":{"n":1}}"#, Some("https://k")),
            None
        );
    }

    #[test]
    fn the_waits_double_to_the_cap() {
        let settings = WebhookSettings::default();
        assert_eq!(settings.wait_after(1), Duration::from_secs(30));
        assert_eq!(settings.wait_after(2), Duration::from_secs(60));
        assert_eq!(settings.wait_after(3), Duration::from_secs(120));
        assert_eq!(settings.wait_after(40), Duration::from_secs(12 * 60 * 60));
        // The default tries span about a day.
        let total: Duration = (1..settings.max_tries)
            .map(|n| settings.wait_after(n))
            .sum();
        assert!(
            total > Duration::from_secs(12 * 3600) && total < Duration::from_secs(30 * 3600),
            "{total:?}"
        );
    }

    #[test]
    fn the_signature_is_the_hmac_of_the_body() {
        // RFC 4231, test case 2.
        assert_eq!(
            signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let secret = generate_secret();
        assert!(secret.starts_with(SECRET_PREFIX));
        assert_eq!(secret.len(), SECRET_PREFIX.len() + 64);
        assert_ne!(secret, generate_secret());
    }

    #[test]
    fn the_guard_refuses_the_private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_blocked(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "192.0.2.10",
            "203.0.113.5",
            "198.51.100.7",
            "2001:db8::1",
            "8.8.8.8",
        ] {
            assert!(!is_blocked(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn the_url_check_refuses_a_bad_url() {
        assert!(check_url("https://hooks.example.com/kairos?x=1", false).is_ok());
        assert!(check_url("http://203.0.113.5:8080/hook", false).is_ok());
        for (url, says) in [
            ("ftp://example.com/x", "http:// or https://"),
            ("https://user:pw@example.com/", "user name"),
            ("http://127.0.0.1:9000/", "KAIROS_WEBHOOK_ALLOW_PRIVATE"),
            ("http://[::1]/", "KAIROS_WEBHOOK_ALLOW_PRIVATE"),
            ("http://localhost/", "KAIROS_WEBHOOK_ALLOW_PRIVATE"),
            (
                "http://169.254.169.254/latest/meta-data",
                "KAIROS_WEBHOOK_ALLOW_PRIVATE",
            ),
            ("not a url", "is not a URL"),
        ] {
            let err = check_url(url, false).expect_err(url);
            assert!(err.contains(says), "{url}: {err}");
        }
        assert!(check_url("http://127.0.0.1:9000/", true).is_ok());
    }
}
