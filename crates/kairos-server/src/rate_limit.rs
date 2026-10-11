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

/// What is being counted against. Both are tracked, deliberately.
///
/// Per-identity alone lets an attacker spray one attempt each at a thousand
/// accounts and never trip a limit. Per-source alone lets a stranger lock a known
/// account out of a service by failing against it, and is meaningless behind a
/// proxy where every request shares one address. Neither is sufficient; together
/// they cover the realistic attacks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    /// The identity being attempted — an email, or an API key's tenant.
    Identity(String),
    /// Where the attempt came from.
    Source(IpAddr),
}

impl Subject {
    /// The metric/log label for this kind of subject. The kind is safe to record
    /// where the value is not.
    pub fn kind(&self) -> &'static str {
        match self {
            Subject::Identity(_) => "identity",
            Subject::Source(_) => "source",
        }
    }

    /// The key of the row of this subject: its kind and the SHA-256 (hex) of
    /// its value. The table holds no email and no address.
    pub fn key(&self) -> SubjectKey {
        let value = match self {
            Subject::Identity(identity) => identity.clone(),
            Subject::Source(addr) => addr.to_string(),
        };
        let digest = Sha256::digest(value.as_bytes());
        SubjectKey {
            kind: self.kind().to_string(),
            subject: digest.iter().map(|b| format!("{b:02x}")).collect(),
        }
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

    fn rules(&self) -> Rules {
        Rules {
            max_failures: i32::try_from(self.config.max_failures).unwrap_or(i32::MAX),
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
            }),
            _ => Ok(Clear {
                known: status.known,
            }),
        }
    }

    /// Record a failed attempt, and lock out any subject that has now had too
    /// many. Returns the lockout if one was tripped, for logging. One upsert.
    pub async fn record_failure(
        &self,
        subjects: &[Subject],
        now: DateTime<Utc>,
    ) -> Option<LockedOut> {
        if subjects.is_empty() {
            return None;
        }
        let keys: Vec<SubjectKey> = subjects.iter().map(Subject::key).collect();
        let rules = self.rules();
        match self
            .blocking
            .run_public(move |conn| {
                store::record_failure(conn, &keys, now, rules).map_err(ApiError::internal)
            })
            .await
        {
            Ok(true) => Some(LockedOut {
                retry_after_secs: self.config.lockout.as_secs(),
            }),
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(error = ?e, "the throttle cannot count a failure");
                None
            }
        }
    }

    /// Record a success, clearing the subjects' counters.
    ///
    /// Only the counters — a live lockout is NOT cleared. Otherwise an attacker
    /// who locks an account out and then authenticates as themselves from the
    /// same address would lift it.
    pub async fn record_success(&self, subjects: &[Subject], now: DateTime<Utc>) {
        if subjects.is_empty() {
            return;
        }
        let keys: Vec<SubjectKey> = subjects.iter().map(Subject::key).collect();
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
    let mut response = ApiError::too_many_requests(locked.retry_after_secs).into_response();
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

    /// Report that the credential did not verify.
    pub async fn failed(&self) {
        let Some(throttle) = self.throttle else {
            return;
        };
        if let Some(locked) = throttle.record_failure(&self.subjects, self.now).await {
            // WARN, not INFO: a lockout is the one event here an operator wants to
            // find in a log. The subject KIND is recorded and the subject VALUE is
            // not — an email or an address in a log line outlives the incident it
            // was gathered for.
            for subject in &self.subjects {
                let kind = subject.kind();
                self.metrics.record_lockout(kind);
                tracing::warn!(
                    subject = kind,
                    retry_after_secs = locked.retry_after_secs,
                    "authentication lockout"
                );
            }
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
}
