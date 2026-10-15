//! The limits of a hosted summarizer (KAIROS-T-0495): the back-off after a
//! refusal for the rate, and one cap of requests in flight for each
//! endpoint, shared by all the builds of the process.
//!
//! - **The back-off.** A 429 or a 503 (a Bedrock throttle) means "too many
//!   requests". The client waits and tries again, up to [`Backoff::tries`]
//!   tries. It waits the time of the `Retry-After` header when the answer
//!   has one (seconds or an HTTP date), at most [`Backoff::retry_after_cap`];
//!   else 2 s, then 4 s, 8 s and so on, at most [`Backoff::max`]. Each other
//!   error (another 5xx, no connection) keeps the rule of KAIROS-T-0341: 3
//!   tries, with pauses of 0.5 s and 2 s.
//! - **The cap.** The `concurrency` of the settings is the most requests in
//!   flight to one endpoint (the base URL and the model) from this process,
//!   whatever the number of builds that run ([`limit_for`]). A request that
//!   waits for the rate holds no permit.
//!
//! A request that the provider still refuses after its tries fails with a
//! message that [`is_rate_limited`] recognizes, so that the build knows the
//! summary is missing for the rate and not for a wrong key.
//!
//! While a request waits for a permit or for its back-off, the client
//! calls its [`Beat`] about each second: the builder reports progress
//! there, so the watchdog does not stop the build as stalled. A request in
//! flight, or a model that hangs, gives no beat.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// The words at the end of the error of a request that the provider still
/// refused for the rate after its last try.
pub const RATE_LIMITED: &str = "the provider limits the rate";

/// Whether `message`, the error of a summary, is a refusal for the rate.
pub fn is_rate_limited(message: &str) -> bool {
    message.contains(RATE_LIMITED)
}

/// A function that the client calls while a request waits: the progress
/// of the build.
pub type Beat = Arc<dyn Fn() + Send + Sync>;

/// The longest time between 2 beats of a wait.
const BEAT: Duration = Duration::from_secs(1);

/// The tries of a request that fails for a reason other than the rate,
/// and the pause before each try after the first (KAIROS-T-0341).
const TRIES: usize = 3;
const PAUSES: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(2_000)];

/// How a request waits after a refusal for the rate.
#[derive(Clone)]
pub struct Backoff {
    /// The first wait with no `Retry-After`: 2 s.
    pub first: Duration,
    /// The longest wait with no `Retry-After`: 2 min.
    pub max: Duration,
    /// The longest wait that a `Retry-After` gets: 5 min.
    pub retry_after_cap: Duration,
    /// The tries of a request that the provider refuses for the rate: 6.
    pub tries: usize,
    /// How the client waits: the time, and the beat to call while it
    /// waits. A test gives a function that records the waits and does not
    /// sleep.
    pub sleep: Sleep,
}

/// How a client waits: the time, and the beat to call while it waits.
pub type Sleep = Arc<dyn Fn(Duration, &dyn Fn()) + Send + Sync>;

/// Sleep for `wait`, and call `beat` each [`BEAT`].
fn sleep_with_beats(wait: Duration, beat: &dyn Fn()) {
    let mut left = wait;
    while !left.is_zero() {
        let step = left.min(BEAT);
        std::thread::sleep(step);
        left -= step;
        beat();
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            first: Duration::from_secs(2),
            max: Duration::from_secs(120),
            retry_after_cap: Duration::from_secs(300),
            tries: 6,
            sleep: Arc::new(sleep_with_beats),
        }
    }
}

impl std::fmt::Debug for Backoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backoff")
            .field("first", &self.first)
            .field("max", &self.max)
            .field("retry_after_cap", &self.retry_after_cap)
            .field("tries", &self.tries)
            .finish_non_exhaustive()
    }
}

impl Backoff {
    /// The wait after the `refused`-th refusal for the rate (1 or more),
    /// with the `Retry-After` of the answer when it had one.
    pub fn wait(&self, refused: usize, retry_after: Option<Duration>) -> Duration {
        match retry_after {
            Some(wait) => wait.min(self.retry_after_cap),
            None => {
                let doublings = u32::try_from(refused.saturating_sub(1))
                    .unwrap_or(u32::MAX)
                    .min(30);
                self.first.saturating_mul(1 << doublings).min(self.max)
            }
        }
    }
}

/// The wait of a `Retry-After` header: a number of seconds, or an HTTP date
/// (RFC 9110). A date in the past is no wait. `None` when the value is
/// neither.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let at = SystemTime::from(at);
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// What one try of a request gave.
pub(crate) enum Attempt {
    /// The end: a summary, or an error that a new try does not change.
    Done(Result<String, String>),
    /// A refusal for the rate (429, 503 or a throttle), with the wait of
    /// its `Retry-After`.
    Limited {
        message: String,
        retry_after: Option<Duration>,
    },
    /// Another error that a new try can change: another 5xx, no answer.
    Failed(String),
}

/// Send a request with `attempt`, each try with a permit of `limit`, and
/// try again as the module doc says. `beat` is called while it waits.
pub(crate) fn send(
    limit: &Limit,
    backoff: &Backoff,
    beat: Option<&Beat>,
    mut attempt: impl FnMut() -> Attempt,
) -> Result<String, String> {
    let no_beat = || {};
    let beat: &dyn Fn() = match beat {
        Some(beat) => beat.as_ref(),
        None => &no_beat,
    };
    let mut limited = 0usize;
    let mut failed = 0usize;
    loop {
        let answer = {
            let _permit = limit.acquire(beat);
            attempt()
        };
        match answer {
            Attempt::Done(result) => return result,
            Attempt::Limited {
                message,
                retry_after,
            } => {
                limited += 1;
                if limited >= backoff.tries.max(1) {
                    return Err(format!("{message} ({limited} tries; {RATE_LIMITED})"));
                }
                (backoff.sleep)(backoff.wait(limited, retry_after), beat);
            }
            Attempt::Failed(message) => {
                failed += 1;
                if failed >= TRIES {
                    return Err(format!("{message} ({failed} tries)"));
                }
                (backoff.sleep)(PAUSES[(failed - 1).min(PAUSES.len() - 1)], beat);
            }
        }
    }
}

/// The cap of requests in flight to one endpoint.
#[derive(Debug)]
pub struct Limit {
    /// The requests in flight, and the cap.
    state: Mutex<(usize, usize)>,
    freed: Condvar,
}

/// A permit of a [`Limit`]: one request in flight. Dropping it frees it.
pub(crate) struct Permit<'a>(&'a Limit);

impl Limit {
    fn new(cap: usize) -> Self {
        Limit {
            state: Mutex::new((0, cap.max(1))),
            freed: Condvar::new(),
        }
    }

    /// Wait for a free permit, and call `beat` each [`BEAT`] while it
    /// waits.
    pub(crate) fn acquire(&self, beat: &dyn Fn()) -> Permit<'_> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.0 >= state.1 {
            let (next, waited) = self
                .freed
                .wait_timeout(state, BEAT)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            if waited.timed_out() {
                beat();
            }
        }
        state.0 += 1;
        Permit(self)
    }

    fn set_cap(&self, cap: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 = cap.max(1);
        self.freed.notify_all();
    }

    /// The cap: the most requests in flight.
    pub fn cap(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = state.0.saturating_sub(1);
        self.0.freed.notify_all();
    }
}

/// The limit of `endpoint` in this process, with the cap `cap`. Each
/// client of one endpoint shares it; the cap of the newest client holds,
/// so a change of the settings applies to the next build.
pub fn limit_for(endpoint: &str, cap: usize) -> Arc<Limit> {
    static LIMITS: OnceLock<Mutex<HashMap<String, Arc<Limit>>>> = OnceLock::new();
    let mut limits = LIMITS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let limit = limits
        .entry(endpoint.to_string())
        .or_insert_with(|| Arc::new(Limit::new(cap)));
    limit.set_cap(cap);
    Arc::clone(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn recorded() -> (Backoff, Arc<Mutex<Vec<Duration>>>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&waits);
        let backoff = Backoff {
            sleep: Arc::new(move |d, _: &dyn Fn()| seen.lock().unwrap().push(d)),
            ..Backoff::default()
        };
        (backoff, waits)
    }

    #[test]
    fn the_wait_doubles_from_2_s_to_2_min() {
        let backoff = Backoff::default();
        let waits: Vec<u64> = (1..=8).map(|n| backoff.wait(n, None).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 64, 120, 120]);
    }

    #[test]
    fn a_retry_after_holds_up_to_5_min() {
        let backoff = Backoff::default();
        assert_eq!(
            backoff.wait(1, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            backoff.wait(3, Some(Duration::from_secs(3600))),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn a_retry_after_is_seconds_or_an_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480); // 2015-10-21 07:28:00Z
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn a_request_refused_for_the_rate_waits_and_gets_its_summary() {
        let (backoff, waits) = recorded();
        let limit = Limit::new(1);
        let mut tries = 0;
        let result = send(&limit, &backoff, None, || {
            tries += 1;
            match tries {
                1 => Attempt::Limited {
                    message: "429".into(),
                    retry_after: Some(Duration::from_secs(7)),
                },
                2 | 3 => Attempt::Limited {
                    message: "429".into(),
                    retry_after: None,
                },
                _ => Attempt::Done(Ok("a summary".into())),
            }
        });
        assert_eq!(result.as_deref(), Ok("a summary"));
        assert_eq!(
            *waits.lock().unwrap(),
            [7, 4, 8].map(Duration::from_secs),
            "the Retry-After, then the doubling wait of the 2nd and 3rd refusal"
        );
    }

    #[test]
    fn a_request_always_refused_for_the_rate_stops_after_6_tries() {
        let (backoff, waits) = recorded();
        let limit = Limit::new(1);
        let mut tries = 0;
        let result = send(&limit, &backoff, None, || {
            tries += 1;
            Attempt::Limited {
                message: "x answered 429".into(),
                retry_after: None,
            }
        });
        let error = result.unwrap_err();
        assert_eq!(tries, 6);
        assert!(is_rate_limited(&error), "{error}");
        assert!(error.contains("6 tries"), "{error}");
        assert_eq!(
            *waits.lock().unwrap(),
            [2, 4, 8, 16, 32].map(Duration::from_secs)
        );
    }

    #[test]
    fn another_error_keeps_3_tries() {
        let (backoff, waits) = recorded();
        let limit = Limit::new(1);
        let mut tries = 0;
        let error = send(&limit, &backoff, None, || {
            tries += 1;
            Attempt::Failed("x answered 500".into())
        })
        .unwrap_err();
        assert_eq!(tries, 3);
        assert!(!is_rate_limited(&error), "{error}");
        assert_eq!(
            *waits.lock().unwrap(),
            [Duration::from_millis(500), Duration::from_millis(2_000)]
        );
    }

    #[test]
    fn a_wait_for_a_permit_or_a_back_off_beats() {
        let beats = AtomicUsize::new(0);
        let beat = || {
            beats.fetch_add(1, Ordering::SeqCst);
        };
        sleep_with_beats(Duration::from_millis(2_500), &beat);
        assert_eq!(beats.load(Ordering::SeqCst), 3, "at 1 s, 2 s and 2.5 s");

        let limit = Limit::new(1);
        let held = limit.acquire(&|| {});
        beats.store(0, Ordering::SeqCst);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _second = limit.acquire(&beat);
            });
            std::thread::sleep(Duration::from_millis(2_200));
            drop(held);
        });
        assert!(beats.load(Ordering::SeqCst) >= 2, "{beats:?}");
    }

    #[test]
    fn one_endpoint_has_one_cap_for_all_its_clients() {
        let a = limit_for("http://rate-test.invalid/v1|m", 2);
        let b = limit_for("http://rate-test.invalid/v1|m", 2);
        let other = limit_for("http://rate-test.invalid/v1|other", 5);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &other));
        let in_flight = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for limit in [&a, &b, &a, &b, &a, &b] {
                let (in_flight, most) = (&in_flight, &most);
                scope.spawn(move || {
                    let _permit = limit.acquire(&|| {});
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(most.load(Ordering::SeqCst), 2);
        assert_eq!(limit_for("http://rate-test.invalid/v1|m", 3).cap(), 3);
    }
}
