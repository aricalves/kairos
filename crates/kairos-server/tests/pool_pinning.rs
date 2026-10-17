//! KAIROS-T-0608: the pools no longer ping a connection on checkout, and the
//! isolation still holds: every checkout sets `search_path` before the
//! caller sees the connection. With a pool of ONE connection, a checkout for
//! `acme` sees `org_acme`, the next for `beta` sees `org_beta`, and the
//! public checkout sees `public`, on the blocking pool and on the async pool.

mod common;

use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::sql_query;
use diesel::sql_types::Text;
use diesel::{Connection, QueryableByName, RunQueryDsl};
use diesel_async::RunQueryDsl as AsyncRunQueryDsl;

use common::{drop_scratch_db, recreate_scratch_db, with_database};
use kairos_db::TenantPool;
use kairos_server::blocking::BlockingTenantPool;

const SCRATCH_DB: &str = "kairos_pool_pinning_t0608_test";

#[derive(QueryableByName)]
struct Schema {
    #[diesel(sql_type = Text)]
    schema: String,
}

const CURRENT: &str = "SELECT current_schema() AS schema";

#[tokio::test]
async fn each_checkout_sees_its_own_schema_with_one_connection() {
    let mut admin = recreate_scratch_db(SCRATCH_DB);
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    {
        let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
        conn.batch_execute("CREATE SCHEMA org_acme; CREATE SCHEMA org_beta")
            .expect("schemas");
    }

    // The blocking pool of the handlers, one connection, reused three times.
    let blocking = BlockingTenantPool::with_min_idle(&scratch_url, 1, 1);
    let seen = |slug: &str| {
        let slug = slug.to_string();
        blocking.run(&slug, |conn| {
            RunQueryDsl::get_result::<Schema>(sql_query(CURRENT), conn)
                .map(|row| row.schema)
                .map_err(kairos_server::error::ApiError::internal)
        })
    };
    assert_eq!(seen("acme").await.expect("acme"), "org_acme");
    assert_eq!(seen("beta").await.expect("beta"), "org_beta");
    assert_eq!(seen("acme").await.expect("acme again"), "org_acme");
    let public = blocking
        .run_public(|conn| {
            RunQueryDsl::get_result::<Schema>(sql_query(CURRENT), conn)
                .map(|row| row.schema)
                .map_err(kairos_server::error::ApiError::internal)
        })
        .await
        .expect("public");
    assert_eq!(public, "public");
    let (total, _idle) = blocking.pool_state();
    assert_eq!(total, 1, "one connection served every checkout");

    // The async pool of the middleware, the same.
    let pool = TenantPool::new(&scratch_url, 1).await.expect("async pool");
    for (slug, schema) in [
        ("acme", "org_acme"),
        ("beta", "org_beta"),
        ("acme", "org_acme"),
    ] {
        let mut conn = pool.tenant(slug).await.expect("tenant checkout");
        let row: Schema = AsyncRunQueryDsl::get_result(sql_query(CURRENT), &mut *conn)
            .await
            .expect("current_schema");
        assert_eq!(row.schema, schema, "{slug}");
        conn.release().await.expect("release");
    }
    let mut conn = pool.public_conn().await.expect("public checkout");
    let row: Schema = AsyncRunQueryDsl::get_result(sql_query(CURRENT), &mut *conn)
        .await
        .expect("current_schema");
    assert_eq!(row.schema, "public");
    drop(conn);

    drop_scratch_db(&mut admin, SCRATCH_DB);
}
