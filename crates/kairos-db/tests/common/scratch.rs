//! Per-run scratch database names (KAIROS-T-0445).
//!
//! Integration tests create scratch databases on the shared compose
//! Postgres. A fixed name per test lets two runs (two worktrees, or two
//! sessions) drop each other's databases, so every name goes through
//! [`scratch_db_name`]: the fixed base name becomes `ktest_<run>_<base>`,
//! where `<run>` is a random value fixed for the process. Each test
//! process (a `cargo test` binary, or one nextest test) gets its own value.
//!
//! The `ktest_` prefix marks a scratch database. A run drops its own
//! databases on teardown ([`ScratchGuard`]); a database left by a crashed
//! run is removed with `angreal db clean-scratch`, which drops only
//! databases named `ktest_…` that nobody is connected to.
//!
//! This one file is shared by the test crates (`kairos-db`,
//! `kairos-server`, `kairos-cli`) through `#[path]`.
#![allow(dead_code)]

use std::fmt;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::ops::Deref;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use diesel::pg::PgConnection;
use diesel::prelude::*;
use diesel::sql_query;

/// Every scratch database name starts with this.
pub const SCRATCH_PREFIX: &str = "ktest_";

/// Postgres truncates identifiers longer than this.
const MAX_IDENTIFIER: usize = 63;

/// The per-process run value: 8 hex digits.
pub fn run_suffix() -> &'static str {
    static SUFFIX: OnceLock<String> = OnceLock::new();
    SUFFIX.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u128(nanos);
        format!("{:08x}", hasher.finish() as u32)
    })
}

/// The scratch database name for `base` in this run:
/// `ktest_<run>_<base without a leading "kairos_">`. A name that already
/// carries this run's prefix comes back unchanged, so the mapping can be
/// applied more than once along a call chain.
pub fn scratch_db_name(base: &str) -> String {
    let own = format!("{SCRATCH_PREFIX}{}_", run_suffix());
    if base.starts_with(&own) {
        return base.to_string();
    }
    assert!(
        !base.is_empty()
            && base
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "scratch database base name {base:?} must be lowercase ascii, digits and _"
    );
    let stem = base.strip_prefix("kairos_").unwrap_or(base);
    let name = format!("{own}{stem}");
    if name.len() <= MAX_IDENTIFIER {
        return name;
    }
    // Too long: keep the start of the stem and add a stable hash of all of it.
    let keep = MAX_IDENTIFIER - own.len() - 9;
    format!("{own}{}_{:08x}", &stem[..keep], fnv1a(stem.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// A scratch database name usable as a `static`: it derefs to (and
/// displays as) this run's name for its base, so `format!("{NAME}")` and
/// `&NAME` work where a `&str` constant did.
pub struct ScratchName {
    base: &'static str,
    name: OnceLock<String>,
}

impl ScratchName {
    pub const fn new(base: &'static str) -> Self {
        Self {
            base,
            name: OnceLock::new(),
        }
    }
}

impl Deref for ScratchName {
    type Target = str;

    fn deref(&self) -> &str {
        self.name.get_or_init(|| scratch_db_name(self.base))
    }
}

impl fmt::Display for ScratchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

/// Drops its scratch database when it goes out of scope, also when the
/// test panics. Errors are ignored: a panic while unwinding would abort,
/// and a leftover is removed by `angreal db clean-scratch`.
pub struct ScratchGuard {
    admin_url: String,
    name: String,
}

impl ScratchGuard {
    /// A guard for `name` on the admin database of `DATABASE_URL` (or the
    /// compose default).
    pub fn new(name: &str) -> Self {
        Self {
            admin_url: admin_database_url(),
            name: scratch_db_name(name),
        }
    }
}

/// The compose Postgres admin URL: `DATABASE_URL`, or the compose default.
pub fn admin_database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://kairos:kairos@localhost:41432/kairos".to_string())
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if let Ok(mut conn) = PgConnection::establish(&self.admin_url) {
            drop_quietly(&mut conn, &self.name);
        }
    }
}

/// `DROP DATABASE IF EXISTS <name> WITH (FORCE)`, ignoring errors.
pub fn drop_quietly(conn: &mut PgConnection, name: &str) {
    let _ = sql_query(format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        scratch_db_name(name)
    ))
    .execute(conn);
}
