//! KAIROS-T-0405 — the store of the uploaded repository sites
//! (KAIROS-D-0012 "Storage"):
//!
//! - the S3 backend writes, reads, finds a missing key and deletes the files
//!   of one publish, against the S3-compatible store of the dev stack
//!   (`kairos-dev-s3`, Adobe S3Mock on 41900);
//! - a cleanup job deletes the files of its publish and nothing else, and a
//!   job with a later `run_after` (a replaced publish) waits.
//!
//! The disk backend has the same cases in the unit tests of `site_store`.
//! Runs against the LIVE compose stack (`angreal services up`).

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use uuid::Uuid;

use common::{AUDIENCE, ISSUER, base_config, recreate_scratch_db, with_database};
use kairos_db::{TenantPool, run_public_migrations};
use kairos_server::app;
use kairos_server::middleware::auth::Authenticator;
use kairos_server::site_store::{PublishKey, SiteStore, StoreConfig, cleanup_job};

const SCRATCH_DB: &str = "kairos_site_store_t0405_test";

fn s3_endpoint() -> String {
    std::env::var("KAIROS_TEST_S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:41900".into())
}

fn key() -> PublishKey {
    PublishKey {
        org_id: Uuid::new_v4(),
        repository_id: Uuid::new_v4(),
        publish_id: Uuid::new_v4(),
    }
}

#[tokio::test]
async fn the_s3_backend_against_live_stack() {
    // S3Mock takes any credentials; object_store reads them from the
    // standard variables, as a deployment does.
    // SAFETY: the other test of this binary does not read the environment
    // (it uses the disk backend), so no thread reads it while it is set.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "kairos-test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "kairos-test");
    }
    let store = SiteStore::new(&StoreConfig::S3 {
        bucket: "kairos-sites".into(),
        prefix: Some(format!("t0405-{}", Uuid::new_v4())),
        endpoint: Some(s3_endpoint()),
        region: Some("us-east-1".into()),
    })
    .expect("the S3 store starts");

    let (one, two) = (key(), key());
    store
        .put(&one, "index.html", Bytes::from_static(b"<h1>one</h1>"))
        .await
        .expect("put");
    store
        .put(&one, "guide/install.html", Bytes::from_static(b"install"))
        .await
        .expect("put nested");
    store
        .put(&two, "index.html", Bytes::from_static(b"two"))
        .await
        .expect("put other publish");

    assert_eq!(
        store.get(&one, "index.html").await.expect("get").as_deref(),
        Some(&b"<h1>one</h1>"[..])
    );
    assert_eq!(
        store
            .get(&one, "guide/install.html")
            .await
            .expect("get nested")
            .as_deref(),
        Some(&b"install"[..])
    );
    assert_eq!(store.get(&one, "missing.html").await.expect("get"), None);

    assert_eq!(store.delete_publish(&one).await.expect("delete"), 2);
    assert_eq!(store.get(&one, "index.html").await.expect("get"), None);
    assert_eq!(
        store.get(&two, "index.html").await.expect("get").as_deref(),
        Some(&b"two"[..]),
        "the other publish stays"
    );
    store.delete_publish(&two).await.expect("clean up");
}

#[tokio::test]
async fn cleanup_jobs_delete_their_publish_only_against_live_stack() {
    drop(recreate_scratch_db(SCRATCH_DB));
    let scratch_url = with_database(&common::admin_database_url(), SCRATCH_DB);
    let mut conn = PgConnection::establish(&scratch_url).expect("connect scratch");
    run_public_migrations(&mut conn).expect("public migrations");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = base_config(&scratch_url);
    config.pages.store = Some(StoreConfig::Disk {
        dir: dir.path().join("pages"),
    });
    let pool = TenantPool::new(&scratch_url, 4).await.expect("pool");
    let auth = Arc::new(Authenticator::with_static_keys(ISSUER, AUDIENCE, []));
    let state = app::state_with(config, pool, auth);
    let store = state.site_store.clone().expect("a store is configured");

    // now: a failed upload or an unpublish; later: a replaced publish; kept:
    // the live publish, with no job.
    let (now, later, kept) = (key(), key(), key());
    for k in [&now, &later, &kept] {
        store
            .put(k, "index.html", Bytes::from_static(b"x"))
            .await
            .expect("put");
        store
            .put(k, "a/b.html", Bytes::from_static(b"y"))
            .await
            .expect("put");
    }
    kairos_db::jobs::enqueue(&mut conn, &cleanup_job(&now, Duration::ZERO)).expect("enqueue now");
    kairos_db::jobs::enqueue(
        &mut conn,
        &cleanup_job(&later, kairos_server::site_store::REPLACED_GRACE),
    )
    .expect("enqueue later");

    state.jobs.drain().await;

    assert_eq!(store.get(&now, "index.html").await.expect("get"), None);
    assert_eq!(store.get(&now, "a/b.html").await.expect("get"), None);
    assert!(
        store
            .get(&later, "index.html")
            .await
            .expect("get")
            .is_some(),
        "a replaced publish waits for its grace time"
    );
    assert!(
        store.get(&kept, "a/b.html").await.expect("get").is_some(),
        "a publish with no job stays"
    );

    let states: Vec<(String, String)> = diesel::sql_query(
        "SELECT payload::text AS payload, state FROM public.jobs WHERE kind = 'site_cleanup' ORDER BY run_after",
    )
    .load::<JobRow>(&mut conn)
    .expect("jobs")
    .into_iter()
    .map(|row| (row.payload, row.state))
    .collect();
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(states[0].0.contains(&now.publish_id.to_string()));
    assert_eq!(states[0].1, "done", "{states:?}");
    assert_eq!(states[1].1, "queued", "{states:?}");
}

#[derive(QueryableByName)]
struct JobRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    payload: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    state: String,
}
