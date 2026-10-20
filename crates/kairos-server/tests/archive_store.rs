//! KAIROS-T-0671 — the S3 store of the retention archive: a file goes to
//! the bucket of an `s3://` target in parts and comes back whole, and a
//! bucket that does not exist is an error. Runs against the S3-compatible
//! store of the dev stack (`kairos-dev-s3`, Adobe S3Mock on 41900), which
//! has the bucket `kairos-sites` (`angreal services up`).

use std::io::Write as _;

use kairos_core::retention::{ArchiveTarget, RetentionConfig};
use kairos_server::archive_store::for_config;
use object_store::ObjectStore as _;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as StorePath;
use uuid::Uuid;

fn s3_endpoint() -> String {
    std::env::var("KAIROS_TEST_S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:41900".into())
}

fn config(target: &str) -> RetentionConfig {
    RetentionConfig {
        archive_target: ArchiveTarget::parse(target),
        archive_s3_endpoint: Some(s3_endpoint()),
        archive_s3_region: Some("us-east-1".into()),
        ..RetentionConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_goes_to_the_bucket_in_parts_and_comes_back_whole() {
    // S3Mock takes any credentials; the store reads them from the standard
    // variables, as a deployment does.
    // SAFETY: the other test of this binary does not read the environment
    // before the call, and this one sets it first.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "kairos-test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "kairos-test");
    }
    let prefix = format!("archive-t0671-{}", Uuid::new_v4());
    let config = config(&format!("s3://kairos-sites/{prefix}"));
    let handle = for_config(&config)
        .expect("the store starts")
        .expect("an s3 target has a store");
    let store = handle.get().expect("the handle holds the store").clone();

    // 20 MiB of lines: 3 parts of 8 MiB. And a small file: 1 part.
    let dir = tempfile::tempdir().expect("tempdir");
    let big = dir.path().join("big.ndjson");
    let mut file = std::fs::File::create(&big).expect("create");
    let line = format!("{{\"row\":\"{}\"}}\n", "x".repeat(1000));
    let lines = 20 * 1024 * 1024 / line.len();
    for _ in 0..lines {
        file.write_all(line.as_bytes()).expect("write");
    }
    drop(file);
    let small = dir.path().join("small.ndjson");
    std::fs::write(&small, "{\"row\":1}\n").expect("write small");

    let (key_big, key_small) = (
        format!("{prefix}/acme/item_history/big.ndjson"),
        format!("{prefix}/acme/activity_log/small.ndjson"),
    );
    let (store_a, key_a, big_a) = (store.clone(), key_big.clone(), big.clone());
    tokio::task::spawn_blocking(move || store_a.put_file(&key_a, &big_a))
        .await
        .expect("join")
        .expect("the big file is stored");
    let (store_b, key_b, small_b) = (store.clone(), key_small.clone(), small.clone());
    tokio::task::spawn_blocking(move || store_b.put_file(&key_b, &small_b))
        .await
        .expect("join")
        .expect("the small file is stored");

    let reader = AmazonS3Builder::from_env()
        .with_bucket_name("kairos-sites")
        .with_endpoint(s3_endpoint())
        .with_allow_http(true)
        .with_virtual_hosted_style_request(false)
        .with_region("us-east-1")
        .build()
        .expect("a reader");
    let got = reader
        .get(&StorePath::from(key_big.as_str()))
        .await
        .expect("get big")
        .bytes()
        .await
        .expect("bytes");
    assert_eq!(got.len(), lines * line.len());
    assert_eq!(&got[..line.len()], line.as_bytes());
    assert_eq!(&got[got.len() - line.len()..], line.as_bytes());
    let got = reader
        .get(&StorePath::from(key_small.as_str()))
        .await
        .expect("get small")
        .bytes()
        .await
        .expect("bytes");
    assert_eq!(&got[..], b"{\"row\":1}\n");

    reader
        .delete(&StorePath::from(key_big.as_str()))
        .await
        .expect("clean up big");
    reader
        .delete(&StorePath::from(key_small.as_str()))
        .await
        .expect("clean up small");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bucket_that_does_not_exist_is_an_error() {
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "kairos-test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "kairos-test");
    }
    let handle = for_config(&config("s3://kairos-no-such-bucket/prefix"))
        .expect("the store starts")
        .expect("an s3 target has a store");
    let store = handle.get().expect("the store").clone();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.ndjson");
    std::fs::write(&file, "{}\n").expect("write");
    let err = tokio::task::spawn_blocking(move || store.put_file("prefix/a.ndjson", &file))
        .await
        .expect("join")
        .expect_err("no such bucket");
    assert!(!err.is_empty());
}

#[tokio::test]
async fn a_filesystem_target_has_no_store() {
    let config = RetentionConfig {
        archive_target: ArchiveTarget::parse("/var/archive"),
        ..RetentionConfig::default()
    };
    assert!(for_config(&config).expect("no store is no error").is_none());
}
