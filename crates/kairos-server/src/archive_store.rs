//! The S3 store of the retention archive (KAIROS-T-0671).
//!
//! `KAIROS_ARCHIVE_TARGET=s3://bucket/prefix` makes a store of the
//! `object_store` crate, as the docs sites do ([`crate::site_store`]).
//! Credentials come from the default chain of the crate: the standard AWS
//! variables, a web identity token (IRSA), the container credentials of ECS
//! and EKS Pod Identity, and the instance role. `KAIROS_ARCHIVE_S3_ENDPOINT`
//! and `KAIROS_ARCHIVE_S3_REGION` are for an S3-compatible store.
//!
//! The retention sweep ([`kairos_db::retention`]) is blocking code on a
//! thread of `spawn_blocking`. [`S3ArchiveStore`] runs each upload on the
//! runtime that made it, and returns when the object is stored.

use std::path::Path;
use std::sync::Arc;

use kairos_core::retention::{ArchiveStore, ArchiveStoreHandle, RetentionConfig};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, WriteMultipart};
use tokio::io::AsyncReadExt as _;

/// The size of each part of an upload, and of each read of the file.
const PART_BYTES: usize = 8 * 1024 * 1024;

/// The parts of one upload that wait to be sent while the next is read.
const PARTS_IN_FLIGHT: usize = 4;

/// An S3 bucket that takes the archive files.
pub struct S3ArchiveStore {
    store: Arc<dyn ObjectStore>,
    runtime: tokio::runtime::Handle,
}

impl S3ArchiveStore {
    /// A store over `store`, with the uploads on the runtime of the caller.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        S3ArchiveStore {
            store,
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl ArchiveStore for S3ArchiveStore {
    fn put_file(&self, key: &str, file: &Path) -> Result<(), String> {
        self.runtime
            .block_on(upload(self.store.as_ref(), key, file))
    }
}

/// Upload the file in parts, so a large archive is never in memory whole.
async fn upload(store: &dyn ObjectStore, key: &str, file: &Path) -> Result<(), String> {
    let mut source = tokio::fs::File::open(file)
        .await
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let multipart = store
        .put_multipart(&StorePath::from(key))
        .await
        .map_err(|e| e.to_string())?;
    let mut write = WriteMultipart::new_with_chunk_size(multipart, PART_BYTES);
    let mut buffer = vec![0_u8; PART_BYTES];
    loop {
        let read = match source.read(&mut buffer).await {
            Ok(read) => read,
            Err(e) => {
                let _ = write.abort().await;
                return Err(format!("cannot read {}: {e}", file.display()));
            }
        };
        if read == 0 {
            break;
        }
        if let Err(e) = write.wait_for_capacity(PARTS_IN_FLIGHT).await {
            let _ = write.abort().await;
            return Err(e.to_string());
        }
        write.write(&buffer[..read]);
    }
    write.finish().await.map(|_| ()).map_err(|e| e.to_string())
}

/// The store of an `s3://` target, from the config; `None` for any other
/// target. Call it inside the runtime. An error stops the start of the
/// server.
pub fn for_config(config: &RetentionConfig) -> Result<Option<ArchiveStoreHandle>, String> {
    let Some(location) = config
        .archive_target
        .as_ref()
        .and_then(|target| target.s3_location())
    else {
        return Ok(None);
    };
    let mut builder = AmazonS3Builder::from_env().with_bucket_name(&location.bucket);
    if let Some(endpoint) = &config.archive_s3_endpoint {
        builder = builder
            .with_endpoint(endpoint)
            .with_allow_http(endpoint.starts_with("http://"))
            // MinIO and most S3-compatible stores take path-style URLs.
            .with_virtual_hosted_style_request(false);
    }
    if let Some(region) = &config.archive_s3_region {
        builder = builder.with_region(region);
    }
    let store = builder
        .build()
        .map_err(|e| format!("the S3 store of the retention archive: {e}"))?;
    Ok(Some(ArchiveStoreHandle::new(Arc::new(
        S3ArchiveStore::new(Arc::new(store)),
    ))))
}
