//! Throttling failed authentication (KAIROS-T-0202, KAIROS-I-0018).
//!
//! Kairos had no rate limiting anywhere. That was survivable while every
//! credential was either a 32-byte random API key or a signed JWT — guessing
//! those is infeasible regardless — but KAIROS-I-0018 adds a password endpoint,
//! and a password endpoint without a throttle is a brute-force target.
//!
//! # Scope
//!
//! Failed **authentication** only, not a general-purpose request limiter. A
//! limiter for every endpoint is a bigger product decision than this needs, and
//! pretending this is one would be worse than saying it is not. The mechanism
//! could be generalised later; nothing here assumes it will be.
//!
//! # Where the state lives, and what that costs
//!
//! In PostgreSQL, in `public.auth_failures` ([`kairos_db::auth_failures`],
//! KAIROS-T-0377). Every server reads and writes the same rows, so a lockout
//! holds on each replica, and a restart forgets nothing. The cost on a
//! request is one read of the primary key before the credential is checked;
//! a failure adds one upsert, and a success adds one delete only when the
//! read found a row. So a client that authenticates well pays one indexed
//! read.
//!
//! The table is public, not in a tenant schema: the throttle runs before the
//! organization is known (`/api/login` names a person, and an API key names
//! its organization only after it is verified). A row holds the hash of the
//! email or the address, not the value. The retention pass removes the rows
//! that can change no decision.
//!
//! When the database cannot be read, the attempt goes on (and its own
//! database work fails): an outage of the throttle must not lock out every
//! person.
//!
//! # API keys (KAIROS-T-0539)
//!
//! An API-key attempt is counted by the key, and by its address at a second,
//! higher limit ([`KEY_ADDRESS_FACTOR`] times the limit). A stale key in a loop
//! locks itself out and no other key: the attempts that the lockout refuses are
//! not counted, so the address sees at most the limit of one key in each
//! lockout. A flood of different keys from one address still locks the
//! address. The 429 says which limit refused the attempt.
//!
//! # Time is a parameter
//!
//! Every method takes `now`. Decay is arithmetic on times, and a test that has
//! to `sleep` to check a lockout expires is a slow test that will one day be
//! flaky. Callers pass `Utc::now()`; tests pass whatever they want.

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderValue;
use axum::http::header::RETRY_AFTER;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use kairos_db::auth_failures::{self as store, Rules, SubjectKey};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::net::SocketAddr;
use std::time::Duration;

use crate::blocking::BlockingTenantPool;
use crate::error::ApiError;

/// The address limit of the API keys is this many times
/// `KAIROS_AUTH_MAX_FAILURES` (KAIROS-T-0539): 100 failures in the window by
/// default. One stale key adds at most the limit (5) to its address in each
/// lockout, so a few stale keys behind one address do not reach it.
pub const KEY_ADDRESS_FACTOR: u32 = 20;

/// The namespace of the API-key rows (KAIROS-T-0539). It keeps the rows of
/// the keys apart from the rows of the password login, and it makes the hash
/// of a key different from `api_keys.token_hash`.
const KEY_NAMESPACE: &str = "api_key:";

/// What is being counted against. Both are tracked, deliberately.
///
/// Per-identity alone lets an attacker spray one attempt each at a thousand
/// accounts and never trip a limit. Per-source alone lets a stranger lock a known
/// account out of a service by failing against it, and is meaningless behind a
/// proxy where every request shares one address. Neither is sufficient; together
/// they cover the realistic attacks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    /// The identity being attempted by a password login: an email.
    Identity(String),
    /// Where a password login came from.
    Source(IpAddr),
    /// The API key that was presented, the full key (KAIROS-T-0539). Not its
    /// display prefix: anyone who saw a prefix in a key list could lock that
    /// key out, and two keys can share a prefix.
    Key(String),
    /// Where an API-key attempt came from: the second, higher limit
    /// (KAIROS-T-0539).
    KeyAddress(IpAddr),
}

/// Which limit refused an attempt (KAIROS-T-0539). The 429 says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// A password login: the account or the address. One uniform 429.
    Login,
    /// One API key failed too often. Other keys still work.
    Key,
    /// API keys from one address failed too often.
    KeyAddress,
}

impl Subject {
    /// The kind of the row (the table allows `identity` and `source`). A key
    /// is an identity and its address a source, in the API-key namespace.
    pub fn kind(&self) -> &'static str {
        match self {
            Subject::Identity(_) | Subject::Key(_) => "identity",
            Subject::Source(_) | Subject::KeyAddress(_) => "source",
        }
    }

    /// The metric/log label of this subject. The label is safe to record
    /// where the value is not.
    pub fn label(&self) -> &'static str {
        match self {
            Subject::Identity(_) => "identity",
            Subject::Source(_) => "source",
            Subject::Key(_) => "key",
            Subject::KeyAddress(_) => "key_address",
        }
    }

    /// The limit that this subject sets.
    pub fn limit(&self) -> Limit {
        match self {
            Subject::Identity(_) | Subject::Source(_) => Limit::Login,
            Subject::Key(_) => Limit::Key,
            Subject::KeyAddress(_) => Limit::KeyAddress,
        }
    }

    /// The key of the row of this subject: its kind and the SHA-256 (hex) of
    /// its value. The table holds no email, no address and no key.
    pub fn key(&self) -> SubjectKey {
        let value = match self {
            Subject::Identity(identity) => identity.clone(),
            Subject::Source(addr) => addr.to_string(),
            Subject::Key(key) => format!("{KEY_NAMESPACE}{key}"),
            Subject::KeyAddress(addr) => format!("{KEY_NAMESPACE}{addr}"),
        };
        let digest = Sha256::digest(value.as_bytes());
        SubjectKey {
            kind: self.kind().to_string(),
            subject: digest.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    /// True when a success removes the count of this subject. A key that
    /// verifies does not remove the count of its address: else one good key
    /// would let a flood from its address go on.
    fn cleared_by_success(&self) -> bool {
        !matches!(self, Subject::KeyAddress(_))
    }
}

/// Thresholds. Defaults chosen to be invisible to a person who mistypes and
/// expensive for a script.
#[derive(Debug, Clone, Copy)]
pub struct ThrottleConfig {
    /// Failures within `window` before the subject is locked out.
    pub max_failures: u32,
    /// How long failures accumulate. Older ones are forgotten.
    pub window: Duration,
    /// How long a lockout lasts once tripped.
    pub lockout: Duration,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            // Five is comfortably above a human's mistyping and far below what a
            // script needs to be useful.
            max_failures: 5,
            window: Duration::from_secs(300),
            // A minute, and DELIBERATELY SHORT. A long lockout is a
            // denial-of-service an attacker can aim at a known account, and it
            // turns one person's typo into a support request. Short and repeatable
            // beats long and punitive: five more failures locks them out again.
            lockout: Duration::from_secs(60),
        }
    }
}

/// The throttle. Cheap to share behind an `Arc`; its counts are in the
/// database, shared by every server (KAIROS-T-0377).
#[derive(Debug)]
pub struct AuthThrottle {
    config: ThrottleConfig,
    blocking: BlockingTenantPool,
}

/// A subject is locked out, and for how much longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockedOut {
    /// Seconds until the subject may try again — the `Retry-After` value.
    pub retry_after_secs: u64,
    /// The limit that refused the attempt (KAIROS-T-0539).
    pub limit: Limit,
}

/// What a check found: no lockout, and whether the subjects have counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Clear {
    /// True when one of the subjects has a row. A success removes it.
    pub known: bool,
}

impl AuthThrottle {
    pub fn new(config: ThrottleConfig, blocking: BlockingTenantPool) -> Self {
        Self { config, blocking }
    }

    /// The rules of `subject`: the address of an API key has the higher
    /// limit (KAIROS-T-0539).
    fn rules(&self, subject: &Subject) -> Rules {
        let max_failures = match subject {
            Subject::KeyAddress(_) => self.config.max_failures.saturating_mul(KEY_ADDRESS_FACTOR),
            _ => self.config.max_failures,
        };
        Rules {
            max_failures: i32::try_from(max_failures).unwrap_or(i32::MAX),
            window_secs: self.config.window.as_secs_f64(),
            lockout_secs: self.config.lockout.as_secs_f64(),
        }
    }

    /// May these subjects attempt authentication?
    ///
    /// Checked BEFORE verifying a credential, so a locked-out attacker does not
    /// get the argon2 work done on their behalf — which matters because that work
    /// is the expensive thing they would otherwise be making the server do.
    /// One read of the primary key. A database error lets the attempt go on.
    pub async fn check(
        &self,
        subjects: &[Subject],
        now: DateTime<Utc>,
    ) -> Result<Clear, LockedOut> {
        if subjects.is_empty() {
            return Ok(Clear::default());
        }
        let keys: Vec<SubjectKey> = subjects.iter().map(Subject::key).collect();
        let status = match self
            .blocking
            .run_public(move |conn| store::status(conn, &keys, now).map_err(ApiError::internal))
            .await
        {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!(error = ?e, "the throttle cannot read its counts; the attempt goes on");
                return Ok(Clear { known: true });
            }
        };
        match status.locked_until {
            // Rounded UP, so a caller that waits exactly this long is past it
            // rather than one millisecond short and refused again.
            Some(until) if until > now => Err(LockedOut {
                retry_after_secs: (until - now).to_std().unwrap_or_default().as_secs() + 1,
                limit: status
                    .locked
                    .and_then(|locked| subjects.iter().find(|s| s.key() == locked))
                    .map_or(Limit::Login, Subject::limit),
            }),
            _ => Ok(Clear {
                known: status.known,
            }),
        }
    }

    /// Record a failed attempt, and lock out any subject that has now had too
    /// many. Returns the lockout if one was tripped, for logging.
    pub async fn record_failure(
        &self,
        subjects: &[Subject],
        now: DateTime<Utc>,
    ) -> Option<LockedOut> {
        self.record_failure_tripped(subjects, now)
            .await
            .first()
            .map(|subject| LockedOut {
                retry_after_secs: self.config.lockout.as_secs(),
                limit: subject.limit(),
            })
    }

    /// Record a failed attempt, and return the subjects that it locked out.
    /// One upsert for each rule set: one for a password login, two for an API
    /// key (the key and its address have different limits).
    async fn record_failure_tripped(
        &self,
        subjects: &[Subject],
        now: DateTime<Utc>,
    ) -> Vec<Subject> {
        let mut groups: Vec<(Rules, Vec<Subject>)> = Vec::new();
        for subject in subjects {
            let rules = self.rules(subject);
            match groups.iter_mut().find(|(r, _)| *r == rules) {
                Some((_, group)) => group.push(subject.clone()),
                None => groups.push((rules, vec![subject.clone()])),
            }
        }
        if groups.is_empty() {
            return Vec::new();
        }
        let batches: Vec<(Vec<SubjectKey>, Rules)> = groups
            .iter()
            .map(|(rules, group)| (group.iter().map(Subject::key).collect(), *rules))
            .collect();
        match self
            .blocking
            .run_public(move |conn| {
                batches
                    .iter()
                    .map(|(keys, rules)| store::record_failure(conn, keys, now, *rules))
                    .collect::<Result<Vec<bool>, _>>()
                    .map_err(ApiError::internal)
            })
            .await
        {
            Ok(tripped) => groups
                .into_iter()
                .zip(tripped)
                .filter(|(_, tripped)| *tripped)
                .flat_map(|((_, group), _)| group)
                .collect(),
            Err(e) => {
                tracing::warn!(error = ?e, "the throttle cannot count a failure");
                Vec::new()
            }
        }
    }

    /// Record a success, clearing the subjects' counters.
    ///
    /// Only the counters — a live lockout is NOT cleared. Otherwise an attacker
    /// who locks an account out and then authenticates as themselves from the
    /// same address would lift it.
    ///
    /// The address of an API key keeps its count (KAIROS-T-0539).
    pub async fn record_success(&self, subjects: &[Subject], now: DateTime<Utc>) {
        let keys: Vec<SubjectKey> = subjects
            .iter()
            .filter(|s| s.cleared_by_success())
            .map(Subject::key)
            .collect();
        if keys.is_empty() {
            return;
        }
        if let Err(e) = self
            .blocking
            .run_public(move |conn| {
                store::record_success(conn, &keys, now).map_err(ApiError::internal)
            })
            .await
        {
            tracing::warn!(error = ?e, "the throttle cannot clear a count");
        }
    }
}

/// Build the throttle this deployment's configuration asks for, or `None` when
/// `KAIROS_AUTH_MAX_FAILURES` is 0 and throttling is off.
pub fn from_config(
    config: &crate::config::AppConfig,
    blocking: &BlockingTenantPool,
) -> Option<AuthThrottle> {
    (config.auth_max_failures > 0).then(|| {
        AuthThrottle::new(
            ThrottleConfig {
                max_failures: config.auth_max_failures,
                window: Duration::from_secs(config.auth_failure_window_secs),
                lockout: Duration::from_secs(config.auth_lockout_secs),
            },
            blocking.clone(),
        )
    })
}

/// The client address to count against, or `None` when nothing trustworthy is
/// available.
///
/// # What is trusted, and why so little
///
/// `X-Forwarded-For` is read **only** when `KAIROS_TRUSTED_PROXY` is on, because
/// the header is caller-supplied: on a directly exposed server, trusting it gives
/// an attacker a new identity per request, which does not weaken a source-based
/// throttle so much as delete it.
///
/// When it is trusted, the **LAST** element is used, not the first. A proxy
/// APPENDS the peer it saw, so the list reads
/// `<whatever the client sent>, <the address the proxy observed>`. The first
/// element is the one under the caller's control; reading it is the classic form
/// of this bug.
///
/// With the switch on and no header present, the answer is `None` rather than the
/// socket peer. Behind a proxy the peer IS the proxy, so counting it would pool
/// every client into one bucket and let one clumsy client lock out the world.
///
/// `None` is also what in-process tests see, since `Router::oneshot` inserts no
/// [`ConnectInfo`]. That is correct rather than convenient: with no address to
/// attribute, there is no source to throttle.
pub fn client_addr(req: &Request, trusted_proxy: bool) -> Option<IpAddr> {
    if trusted_proxy {
        return req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|list| {
                list.rsplit(',')
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .and_then(|s| s.parse().ok())
            });
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

/// The 429 for a locked-out caller, with `Retry-After`.
///
/// A [`Response`] rather than an [`ApiError`], because `ApiError` carries no
/// headers and `Retry-After` is the one thing a well-behaved client needs from
/// this reply. Callers that return `Result<Response, ApiError>` hand this back as
/// `Ok` — unusual enough to say out loud, which is why it lives here and not
/// inline at each call site.
pub fn locked_out_response(locked: LockedOut) -> Response {
    let error = match locked.limit {
        Limit::Login => ApiError::too_many_requests(locked.retry_after_secs),
        Limit::Key => ApiError::key_locked_out(locked.retry_after_secs),
        Limit::KeyAddress => ApiError::address_locked_out(locked.retry_after_secs),
    };
    let mut response = error.into_response();
    if let Ok(value) = HeaderValue::from_str(&locked.retry_after_secs.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

/// One authentication attempt, from the throttle's point of view.
///
/// Wraps the three-step dance — refuse if locked out, then report success or
/// failure — so a call site cannot accidentally do two of the three. Also absorbs
/// "this deployment has throttling off" and "there is no address to attribute",
/// which otherwise spread `if let Some` across every auth path.
pub struct Attempt<'a> {
    throttle: Option<&'a AuthThrottle>,
    metrics: &'a crate::metrics::Metrics,
    subjects: Vec<Subject>,
    now: DateTime<Utc>,
    /// The check found a row of the subjects: a success removes it.
    known: bool,
}

impl<'a> Attempt<'a> {
    /// Begin an attempt against `identity` (the thing being authenticated *as*,
    /// when the caller has one worth naming) from `source`.
    ///
    /// Either may be absent. With both absent the attempt is unthrottled, because
    /// there is nothing to count against — which is the honest outcome rather than
    /// a hole: an attempt that cannot be attributed cannot be rationed either.
    pub fn begin(
        state: &'a crate::app::AppState,
        source: Option<IpAddr>,
        identity: Option<String>,
    ) -> Self {
        let mut subjects = Vec::with_capacity(2);
        if let Some(identity) = identity {
            subjects.push(Subject::Identity(identity));
        }
        if let Some(source) = source {
            subjects.push(Subject::Source(source));
        }
        Self {
            throttle: state.throttle.as_deref(),
            metrics: &state.metrics,
            subjects,
            now: Utc::now(),
            known: true,
        }
    }

    /// The 429 to return instead of checking the credential, if this caller is
    /// locked out.
    ///
    /// Called BEFORE the credential is verified, so the expensive work — an argon2
    /// verification, or two database round trips for an API key — is never done on
    /// a locked-out caller's behalf.
    pub async fn refuse_if_locked_out(&mut self) -> Option<Response> {
        let throttle = self.throttle?;
        match throttle.check(&self.subjects, self.now).await {
            Ok(clear) => {
                self.known = clear.known;
                None
            }
            Err(locked) => {
                tracing::debug!(
                    retry_after_secs = locked.retry_after_secs,
                    "refusing a locked-out authentication attempt"
                );
                Some(locked_out_response(locked))
            }
        }
    }

    /// Begin an API-key attempt (KAIROS-T-0539): counted by the key, and by
    /// `source` at the higher address limit.
    pub fn api_key(state: &'a crate::app::AppState, source: Option<IpAddr>, key: &str) -> Self {
        let mut subjects = vec![Subject::Key(key.to_string())];
        if let Some(source) = source {
            subjects.push(Subject::KeyAddress(source));
        }
        Self {
            throttle: state.throttle.as_deref(),
            metrics: &state.metrics,
            subjects,
            now: Utc::now(),
            known: true,
        }
    }

    /// Report that the credential did not verify.
    pub async fn failed(&self) {
        let Some(throttle) = self.throttle else {
            return;
        };
        let tripped = throttle
            .record_failure_tripped(&self.subjects, self.now)
            .await;
        let retry_after_secs = throttle.config.lockout.as_secs();
        // WARN, not INFO: a lockout is the one event here an operator wants to
        // find in a log. The subject LABEL is recorded and the subject VALUE is
        // not — an email, an address or a key in a log line outlives the
        // incident it was gathered for.
        for subject in &tripped {
            let label = subject.label();
            self.metrics.record_lockout(label);
            tracing::warn!(subject = label, retry_after_secs, "authentication lockout");
        }
    }

    /// Report that the credential verified, clearing the counters. No write
    /// when the check found no count.
    pub async fn succeeded(&self) {
        if let (Some(throttle), true) = (self.throttle, self.known) {
            throttle.record_success(&self.subjects, self.now).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_holds_the_kind_and_a_hash_not_the_value() {
        let identity = Subject::Identity("someone@example.test".into()).key();
        assert_eq!(identity.kind, "identity");
        assert_eq!(identity.subject.len(), 64);
        assert!(!identity.subject.contains("someone"));
        let source = Subject::Source("203.0.113.9".parse().unwrap()).key();
        assert_eq!(source.kind, "source");
        assert_ne!(source.subject, identity.subject);
        assert_eq!(
            source,
            Subject::Source("203.0.113.9".parse().unwrap()).key(),
            "the same value, the same row"
        );
    }

    #[test]
    fn the_rows_of_a_key_are_apart_from_the_rows_of_a_login() {
        let addr: IpAddr = "203.0.113.9".parse().unwrap();
        let key_address = Subject::KeyAddress(addr).key();
        assert_eq!(key_address.kind, "source");
        assert_ne!(key_address, Subject::Source(addr).key());
        let key = crate::service_accounts::auth::generate_key("acme");
        let row = Subject::Key(key.clone()).key();
        assert_eq!(row.kind, "identity");
        assert_ne!(
            row.subject,
            crate::service_accounts::auth::hash_key(&key),
            "the row is not the token hash of the key"
        );
        assert_eq!(Subject::Key(key).label(), "key");
        assert_eq!(Subject::KeyAddress(addr).label(), "key_address");
    }
}
