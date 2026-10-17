//! KAIROS-T-0614: the timeouts of the connections, and the pool of the jobs.
//!
//! 1. Each connection of the blocking pool and of the async pool has
//!    `statement_timeout` (30 s), `lock_timeout` (5 s) and
//!    `idle_in_transaction_session_timeout` (60 s). A connection outside
//!    the pools has none: the pools set them, not the server.
//! 2. The migration connection has the `lock_timeout` alone.
//! 3. The jobs run on a pool of their own, sized by `KAIROS_JOB_POOL_SIZE`.
//!    A full job pool leaves the handlers their connections.
//!
//! Runs against the compose Postgres (`angreal services up`). It touches
//! no table, so it uses the admin database and no scratch database.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use diesel::pg::PgConnection;
use diesel::sql_query;
use diesel::sql_types::Text;
use diesel::{Connection, QueryableByName, RunQueryDsl};
use diesel_async::RunQueryDsl as AsyncRunQueryDsl;

use kairos_db::TenantPool;
use kairos_server::app;
use kairos_server::blocking::BlockingTenantPool;
use kairos_server::config::AppConfig;
use kairos_server::error::ApiError;
use kairos_server::middleware::auth::Authenticator;

#[derive(QueryableByName)]
struct Setting {
    #[diesel(sql_type = Text)]
    value: String,
}

/// The three timeouts of the session, as `SHOW` reports them, in one text.
const TIMEOUTS: &str = "SELECT current_setting('statement_timeout') || ' ' \
     || current_setting('lock_timeout') || ' ' \
     || current_setting('idle_in_transaction_session_timeout') AS value";

const POOLED: &str = "30s 5s 1min";

fn timeouts(conn: &mut PgConnection) -> Result<String, ApiError> {
    RunQueryDsl::get_result::<Setting>(sql_query(TIMEOUTS), conn)
        .map(|row| row.value)
        .map_err(ApiError::internal)
}

#[tokio::test]
async fn every_pooled_connection_has_the_timeouts() {
    let url = common::admin_database_url();

    // The control: a connection outside the pools has the defaults of the
    // server, so the pools set the timeouts.
    let mut plain = PgConnection::establish(&url).expect("connect");
    assert_eq!(timeouts(&mut plain).expect("plain"), "0 0 0");

    // The blocking pool of the handlers and the jobs: once per connection,
    // so a second checkout of the same connection sees them too.
    let blocking = BlockingTenantPool::new(&url, 1);
    for round in 0..2 {
        let seen = blocking.run_public(timeouts).await.expect("blocking");
        assert_eq!(seen, POOLED, "blocking pool, checkout {round}");
    }
    let (total, _idle) = blocking.pool_state();
    assert_eq!(total, 1, "one connection served both checkouts");

    // The async pool of the middleware.
    let pool = TenantPool::new(&url, 1).await.expect("async pool");
    for round in 0..2 {
        let mut conn = pool.public_conn().await.expect("public checkout");
        let row: Setting = AsyncRunQueryDsl::get_result(sql_query(TIMEOUTS), &mut *conn)
            .await
            .expect("timeouts");
        assert_eq!(row.value, POOLED, "async pool, checkout {round}");
    }

    // The migration connection: the lock timeout alone. A migration can run
    // long, so no statement timeout.
    let mut migration = kairos_db::establish_migration_connection(&url).expect("connect");
    assert_eq!(timeouts(&mut migration).expect("migration"), "0 5s 0");
}

/// The settings of a config that connects to `url`, with the variables
/// `extra`.
fn config(url: &str, extra: &[(&str, &str)]) -> AppConfig {
    let mut vars = vec![
        ("DATABASE_URL", url),
        ("OIDC_ISSUER_URL", "http://localhost:5558/dex/"),
        ("OIDC_AUDIENCE", "kairos-cli"),
        ("KAIROS_AUTH_MAX_FAILURES", "0"),
    ];
    vars.extend_from_slice(extra);
    AppConfig::from_lookup(|var| {
        vars.iter()
            .find(|(name, _)| *name == var)
            .map(|(_, value)| value.to_string())
    })
    .expect("valid config")
}

#[tokio::test]
async fn the_jobs_run_on_a_pool_of_their_own() {
    let url = common::admin_database_url();

    // The default, and the setting.
    assert_eq!(config(&url, &[]).job_pool_size, 6);
    let config = config(&url, &[("KAIROS_JOB_POOL_SIZE", "2")]);
    assert_eq!(config.job_pool_size, 2);

    let pool = TenantPool::new(&url, 2).await.expect("async pool");
    let state = app::state_with(config, pool, Arc::new(Authenticator::disabled()));
    assert_eq!(
        state.job_pool.max_size(),
        2,
        "the job pool has the size of the setting"
    );
    assert_eq!(
        state.blocking.max_size(),
        16,
        "the handlers keep their pool"
    );

    // Fill the job pool: two checkouts that hold their connection.
    let hold = |pool: BlockingTenantPool| {
        tokio::spawn(async move {
            pool.run_public(|_conn| {
                std::thread::sleep(Duration::from_millis(1500));
                Ok(())
            })
            .await
        })
    };
    let first = hold(state.job_pool.clone());
    let second = hold(state.job_pool.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (job_total, job_idle) = state.job_pool.pool_state();
    assert_eq!((job_total, job_idle), (2, 0), "the job pool is full");

    // A handler gets a connection at once: its pool is another pool.
    let started = Instant::now();
    state
        .blocking
        .run_public(|conn| {
            RunQueryDsl::execute(sql_query("SELECT 1"), conn).map_err(ApiError::internal)
        })
        .await
        .expect("the handler's checkout");
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the handler did not wait for the jobs: {:?}",
        started.elapsed()
    );
    let (handler_total, _idle) = state.blocking.pool_state();
    assert_eq!(
        handler_total, 1,
        "the handler's pool opened its own connection"
    );

    // A third job waits for one of the two.
    let started = Instant::now();
    state
        .job_pool
        .run_public(|_conn| Ok(()))
        .await
        .expect("the third job's checkout");
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the third job waited for a connection of the job pool: {:?}",
        started.elapsed()
    );
    first.await.expect("first").expect("first checkout");
    second.await.expect("second").expect("second checkout");
}
