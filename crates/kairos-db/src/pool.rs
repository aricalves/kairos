//! The bb8 + diesel-async connection pool with per-checkout tenant
//! `search_path` pinning (KAIROS-A-0001/A-0009, KAIROS-T-0009).
//!
//! # Isolation mechanism
//!
//! One shared [`bb8`] pool of [`AsyncPgConnection`]s serves every tenant.
//! Tenant selection is the connection's `search_path`:
//!
//! - [`TenantPool::tenant`] checks a connection out and pins
//!   `search_path` to `"org_{slug}", public` **before handing it over** —
//!   unqualified (tenant) tables in `schema.rs` then resolve to that
//!   tenant's schema, while the schema-qualified `public.*` tables resolve
//!   regardless.
//! - [`TenantPool::public_conn`] checks a connection out and pins
//!   `search_path` to `public` for cross-tenant/admin work, so no tenant
//!   table is reachable unqualified.
//! - When a [`TenantConnection`] is dropped, its search_path is RESET to
//!   `public` before the connection re-enters the pool (see below).
//!
//! # Reset-on-return mechanism and its failure mode
//!
//! `Drop` is synchronous but the reset is a SQL round-trip, so
//! [`TenantConnection`]'s `Drop` impl moves the owned pooled connection
//! into a `tokio::spawn`ed task that executes `SET search_path TO public`
//! and only then lets the connection return to the pool (bb8 gets it back
//! when the task drops it, so the next checkout can never race the reset).
//! Callers who want a deterministic, awaitable reset can call
//! [`TenantConnection::release`] instead of dropping.
//!
//! Failure mode: if there is no tokio runtime at drop time, or the reset
//! statement itself fails, the connection returns to the pool with a stale
//! tenant search_path. This is contained by construction — the *only* ways
//! to get a connection out of [`TenantPool`] are `tenant()` /
//! `public_conn()`, and both unconditionally overwrite `search_path` on
//! checkout before the caller sees the connection — so a stale path on an
//! idle pooled connection is never observable through this API. The reset
//! is defense-in-depth for anything that bypasses it.

use std::ops::{Deref, DerefMut};
use std::time::Duration;

use diesel_async::pooled_connection::bb8::{Pool, PooledConnection};
use diesel_async::pooled_connection::{
    AsyncDieselConnectionManager, ManagerConfig, RecyclingMethod,
};
use diesel_async::{AsyncPgConnection, SimpleAsyncConnection};

use crate::tenant::{is_valid_tenant_slug, tenant_schema_name};

/// The shared async pool type (bb8 over diesel-async's manager).
pub type PgPool = Pool<AsyncPgConnection>;

/// An owned checkout from the pool (`'static`: not borrowing the pool, so
/// it can be moved into the reset task on drop).
type OwnedConnection = PooledConnection<'static, AsyncPgConnection>;

/// The longest a statement of a pooled connection runs (KAIROS-T-0614).
/// A statement over it is cancelled, and the request fails with the error.
pub const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest a statement of a pooled connection, or of the migration
/// connection, waits for a lock (KAIROS-T-0614). The wait for the
/// migration lock of [`crate::with_migration_lock`] is not under it.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest a pooled connection stays idle inside an open transaction
/// (KAIROS-T-0614). Postgres then closes the session, and the next
/// checkout of the connection fails and opens a new one.
pub const IDLE_IN_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// The statement that sets the three timeouts of a pooled connection. Each
/// pool runs it once on each NEW connection, not on each checkout: a
/// session setting holds for the life of the connection, so a checkout
/// pays nothing for it.
pub fn session_timeouts_sql() -> String {
    format!(
        "SET statement_timeout = '{}s'; SET lock_timeout = '{}s'; \
         SET idle_in_transaction_session_timeout = '{}s'",
        STATEMENT_TIMEOUT.as_secs(),
        LOCK_TIMEOUT.as_secs(),
        IDLE_IN_TRANSACTION_TIMEOUT.as_secs()
    )
}

/// The statement that sets [`LOCK_TIMEOUT`] alone: the migration connection
/// runs long statements, so it gets no `statement_timeout`.
pub fn lock_timeout_sql() -> String {
    format!("SET lock_timeout = '{}s'", LOCK_TIMEOUT.as_secs())
}

/// Errors from building the pool or checking connections out.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// The slug does not match the KAIROS-S-0004 organization slug pattern.
    /// Validated slugs contain no quoting metacharacters, which is what
    /// makes interpolating `org_{slug}` into `SET search_path` safe.
    #[error(
        "The tenant slug {0:?} is not correct. A tenant slug must match ^[a-z][a-z0-9_-]{{1,62}}$."
    )]
    InvalidSlug(String),
    /// Building the pool failed (bad URL, unreachable server, ...).
    #[error("The server cannot make the connection pool: {0}.")]
    Build(#[from] diesel_async::pooled_connection::PoolError),
    /// Checking a connection out of the pool failed.
    #[error("The server cannot get a connection from the pool: {0}.")]
    Checkout(#[from] diesel_async::pooled_connection::bb8::RunError),
    /// Pinning or resetting `search_path` failed.
    #[error("The server cannot set search_path: {0}.")]
    SearchPath(#[from] diesel::result::Error),
}

/// A shared connection pool that serves every tenant, pinning
/// `search_path` per checkout. See the module docs for the isolation and
/// reset mechanics.
#[derive(Clone)]
pub struct TenantPool {
    pool: PgPool,
}

impl std::fmt::Debug for TenantPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantPool")
            .field("state", &self.pool.state())
            .finish()
    }
}

impl TenantPool {
    /// Build a pool of at most `max_size` connections against
    /// `database_url`. Connections negotiate TLS as `sslmode` asks (see
    /// [`crate::tls`]).
    ///
    /// KAIROS-T-0608: no ping on checkout (`test_on_check_out(false)` and
    /// `RecyclingMethod::Fast`): every checkout runs `SET search_path`
    /// first, so a dead connection fails there, and a live one is not
    /// asked twice. A 5 s `connection_timeout`, so a full pool answers
    /// with an error instead of 30 s of silence.
    ///
    /// KAIROS-T-0614: each new connection gets the timeouts of
    /// [`session_timeouts_sql`] in its setup, one round trip per
    /// connection and none per checkout.
    pub async fn new(database_url: &str, max_size: u32) -> Result<Self, PoolError> {
        let mut config = ManagerConfig::default();
        config.custom_setup = Box::new(|url| {
            let url = url.to_owned();
            Box::pin(async move {
                let mut conn = crate::tls::establish(&url).await?;
                conn.batch_execute(&session_timeouts_sql())
                    .await
                    .map_err(diesel::ConnectionError::CouldntSetupConfiguration)?;
                Ok(conn)
            })
        });
        config.recycling_method = RecyclingMethod::Fast;
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(
            database_url,
            config,
        );
        let pool = Pool::builder()
            .max_size(max_size)
            .test_on_check_out(false)
            .connection_timeout(std::time::Duration::from_secs(5))
            .build(manager)
            .await?;
        Ok(Self { pool })
    }

    /// Check out a connection pinned to `slug`'s tenant schema:
    /// `search_path = "org_{slug}", public`.
    ///
    /// The pin happens before the connection is handed over, so the
    /// checkout is deterministic regardless of what previously ran on the
    /// underlying connection.
    pub async fn tenant(&self, slug: &str) -> Result<TenantConnection, PoolError> {
        if !is_valid_tenant_slug(slug) {
            return Err(PoolError::InvalidSlug(slug.to_string()));
        }
        let schema = tenant_schema_name(slug);
        let mut conn = self.pool.get_owned().await?;
        conn.batch_execute(&format!("SET search_path TO \"{schema}\", public"))
            .await?;
        Ok(TenantConnection {
            conn: Some(conn),
            schema,
        })
    }

    /// Check out a connection pinned to `search_path = public` (no tenant
    /// tables reachable unqualified) for cross-tenant/admin work.
    pub async fn public_conn(&self) -> Result<OwnedConnection, PoolError> {
        let mut conn = self.pool.get_owned().await?;
        conn.batch_execute("SET search_path TO public").await?;
        Ok(conn)
    }

    /// The underlying bb8 pool. Checkouts taken directly from it are NOT
    /// search_path-pinned — use [`Self::tenant`] / [`Self::public_conn`]
    /// for queries; this exists for pool introspection and tests that
    /// observe the reset-on-return behavior.
    pub fn raw_pool(&self) -> &PgPool {
        &self.pool
    }
}

/// A pooled connection pinned to one tenant's schema. Derefs to
/// [`AsyncPgConnection`]; on drop the `search_path` is reset to `public`
/// before the connection re-enters the pool (module docs).
pub struct TenantConnection {
    /// `Some` until dropped/released; `Option` so `Drop` can move the
    /// connection into the async reset task.
    conn: Option<OwnedConnection>,
    schema: String,
}

impl TenantConnection {
    /// The schema this connection is pinned to (`org_{slug}`).
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Reset `search_path` and return the connection to the pool,
    /// deterministically (awaitable alternative to relying on `Drop`).
    pub async fn release(mut self) -> Result<(), PoolError> {
        if let Some(mut conn) = self.conn.take() {
            conn.batch_execute("SET search_path TO public").await?;
        }
        Ok(())
    }
}

impl Deref for TenantConnection {
    type Target = AsyncPgConnection;

    fn deref(&self) -> &Self::Target {
        self.conn
            .as_ref()
            .expect("TenantConnection used after release")
    }
}

impl DerefMut for TenantConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn
            .as_mut()
            .expect("TenantConnection used after release")
    }
}

impl Drop for TenantConnection {
    fn drop(&mut self) {
        let Some(mut conn) = self.conn.take() else {
            return; // released explicitly
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // The connection only re-enters the pool when this task
                // drops it, i.e. after the reset ran (or failed).
                handle.spawn(async move {
                    let _ = conn.batch_execute("SET search_path TO public").await;
                });
            }
            Err(_) => {
                // No runtime: the connection returns with a stale
                // search_path. Harmless through this API — every checkout
                // re-pins before handing the connection over (module docs).
            }
        }
    }
}
