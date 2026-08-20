//! Object-store backend (`object-store` feature) via the `object_store` crate.
//! The object **key is the content id**, so writes are naturally immutable and
//! idempotent. Unit-tested against the in-memory object store; real S3 needs creds.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, path::Path as ObjPath,
};

use crate::{FileStore, FileStoreError, content_id, safe_id};

/// Provider selected by a secret-free deployment backing contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectStoreProvider {
    S3,
    Gcs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectStoreConfigDecision {
    Accept,
    MissingBucketOrPrefix,
    InvalidPrefix,
    MissingS3Region,
    GcsHasS3Coordinates,
}

const fn object_store_config_decision(
    provider: ObjectStoreProvider,
    bucket_nonempty: bool,
    prefix_nonempty: bool,
    prefix_normalized: bool,
    region_present: bool,
    region_nonempty: bool,
    endpoint_present: bool,
) -> ObjectStoreConfigDecision {
    if !bucket_nonempty || !prefix_nonempty {
        return ObjectStoreConfigDecision::MissingBucketOrPrefix;
    }
    if !prefix_normalized {
        return ObjectStoreConfigDecision::InvalidPrefix;
    }
    match provider {
        ObjectStoreProvider::S3 if !region_nonempty => ObjectStoreConfigDecision::MissingS3Region,
        ObjectStoreProvider::Gcs if endpoint_present || region_present => {
            ObjectStoreConfigDecision::GcsHasS3Coordinates
        }
        ObjectStoreProvider::S3 | ObjectStoreProvider::Gcs => ObjectStoreConfigDecision::Accept,
    }
}

/// Secret-free object allocation. Credentials come only from the provider
/// workload-identity chain; this value never accepts static keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectFileStoreConfig {
    pub provider: ObjectStoreProvider,
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
}

impl ObjectFileStoreConfig {
    pub fn validate(&self) -> Result<(), FileStoreError> {
        let decision = object_store_config_decision(
            self.provider,
            !self.bucket.trim().is_empty(),
            !self.prefix.trim_matches('/').is_empty(),
            !self
                .prefix
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."),
            self.region.is_some(),
            self.region
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            self.endpoint.is_some(),
        );
        match decision {
            ObjectStoreConfigDecision::Accept => Ok(()),
            ObjectStoreConfigDecision::MissingBucketOrPrefix => {
                Err(e("object bucket and prefix must be non-empty"))
            }
            ObjectStoreConfigDecision::InvalidPrefix => {
                Err(e("object prefix must be a normalized relative path"))
            }
            ObjectStoreConfigDecision::MissingS3Region => {
                Err(e("S3 object backing requires a region"))
            }
            ObjectStoreConfigDecision::GcsHasS3Coordinates => Err(e(
                "GCS object backing does not accept S3 region or endpoint fields",
            )),
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{ObjectStoreConfigDecision, ObjectStoreProvider, object_store_config_decision};

    #[kani::proof]
    fn object_store_configuration_accepts_exactly_the_provider_compatible_shape() {
        let provider = if kani::any::<bool>() {
            ObjectStoreProvider::S3
        } else {
            ObjectStoreProvider::Gcs
        };
        let bucket_nonempty: bool = kani::any();
        let prefix_nonempty: bool = kani::any();
        let prefix_normalized: bool = kani::any();
        let region_present: bool = kani::any();
        let region_nonempty: bool = kani::any();
        let endpoint_present: bool = kani::any();

        let accepted = object_store_config_decision(
            provider,
            bucket_nonempty,
            prefix_nonempty,
            prefix_normalized,
            region_present,
            region_nonempty,
            endpoint_present,
        ) == ObjectStoreConfigDecision::Accept;
        let provider_exact = match provider {
            ObjectStoreProvider::S3 => region_nonempty,
            ObjectStoreProvider::Gcs => !region_present && !endpoint_present,
        };
        assert_eq!(
            accepted,
            bucket_nonempty && prefix_nonempty && prefix_normalized && provider_exact
        );
    }
}

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// An object-store-backed [`FileStore`]. `prefix` namespaces the keys (e.g. `blobs`).
pub struct ObjectFileStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl ObjectFileStore {
    /// Wrap any supported `object_store` implementation.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            store,
            prefix: prefix.into(),
        }
    }

    /// Build the one network adapter from a secret-free allocation. Provider
    /// SDK environment/metadata discovery supplies short-lived credentials.
    pub fn from_config(config: ObjectFileStoreConfig) -> Result<Self, FileStoreError> {
        config.validate()?;
        let store: Arc<dyn ObjectStore> = match config.provider {
            ObjectStoreProvider::S3 => {
                let region = config
                    .region
                    .as_deref()
                    .ok_or_else(|| e("S3 object backing requires a region"))?;
                let mut builder = object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(&config.bucket)
                    .with_region(region);
                if let Some(endpoint) = &config.endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                Arc::new(builder.build().map_err(e)?)
            }
            ObjectStoreProvider::Gcs => Arc::new(
                object_store::gcp::GoogleCloudStorageBuilder::from_env()
                    .with_bucket_name(&config.bucket)
                    .build()
                    .map_err(e)?,
            ),
        };
        Ok(Self::new(store, config.prefix))
    }

    fn key(&self, id: &str) -> ObjPath {
        ObjPath::from(format!("{}/{}", self.prefix, id))
    }
}

#[async_trait]
impl FileStore for ObjectFileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        let key = self.key(&id);
        let options = PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        match self
            .store
            .put_opts(&key, PutPayload::from(bytes.to_vec()), options)
            .await
        {
            Ok(_) => Ok(id),
            Err(object_store::Error::AlreadyExists { .. }) => {
                let existing = self.get(&id).await?.ok_or_else(|| {
                    e("content-addressed object disappeared during idempotent put")
                })?;
                if existing == bytes {
                    Ok(id)
                } else {
                    Err(e(
                        "content-addressed object does not match its immutable id",
                    ))
                }
            }
            Err(error) => Err(e(error)),
        }
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        if !safe_id(id) {
            return Ok(None);
        }
        match self.store.get(&self.key(id)).await {
            Ok(result) => {
                let bytes = result.bytes().await.map_err(e)?.to_vec();
                if content_id(&bytes) != id {
                    return Err(e("content-addressed object failed integrity verification"));
                }
                Ok(Some(bytes))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(e(err)),
        }
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        let prefix = ObjPath::from(self.prefix.as_str());
        let mut stream = self.store.list(Some(&prefix));
        let mut ids = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(e)?;
            if let Some(name) = meta.location.filename().filter(|name| safe_id(name)) {
                ids.push(name.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        if !safe_id(id) {
            return Ok(false);
        }
        // object-store `delete` is idempotent (won't report prior existence), so
        // probe with `head` first to honor the "did it exist?" contract.
        let key = self.key(id);
        match self.store.head(&key).await {
            Ok(_) => {
                self.store.delete(&key).await.map_err(e)?;
                Ok(true)
            }
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(e(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    async fn object_store_conformance(
        backend: Arc<dyn ObjectStore>,
        prefix: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Cause/effect graph: C1=create immutable content-addressed object;
        // C2=exact replay; C3=foreign/in-flight key under the allocation;
        // C4=external corruption at the committed key; C5=delete/replay.
        // Effects: E1=stable id and bytes; E2=no overwrite; E3=foreign key is
        // never projected as a File; E4=get and put fail closed; E5=one visible
        // delete followed by an idempotent absence. The same graph executes on
        // the in-memory adapter and mandatory MinIO substrate.
        let store = ObjectFileStore::new(Arc::clone(&backend), prefix);
        let bytes = b"hello object store";
        let id = store.put(bytes).await?;
        assert_eq!(id, store.put(bytes).await?, "idempotent");
        assert_eq!(store.get(&id).await?.as_deref(), Some(bytes.as_slice()));
        assert!(store.get("blobs/missing").await?.is_none());

        let partial = ObjPath::from(format!("{prefix}/.partial-upload"));
        backend
            .put(&partial, PutPayload::from_static(b"partial"))
            .await?;
        let listed = store.list().await?;
        assert_eq!(listed.as_slice(), std::slice::from_ref(&id));

        backend
            .put(
                &ObjPath::from(format!("{prefix}/{id}")),
                PutPayload::from_static(b"corrupt"),
            )
            .await?;
        assert!(store.get(&id).await.is_err());
        assert!(store.put(bytes).await.is_err());

        backend.delete(&partial).await?;
        assert!(store.delete(&id).await?);
        assert!(!store.delete(&id).await?);
        assert!(store.get("../outside").await?.is_none());
        assert!(!store.delete("../outside").await?);
        Ok(())
    }

    #[tokio::test]
    async fn in_memory_object_store_conforms() -> Result<(), Box<dyn std::error::Error>> {
        object_store_conformance(Arc::new(InMemory::new()), "blobs").await
    }

    #[test]
    fn allocation_config_is_secret_free_and_provider_exact() {
        // Cause/effect graph: C1=bucket non-empty, C2=normalized non-empty
        // prefix, C3=S3 has region, C4=GCS has no S3-only coordinates.
        // Decision table: S3(C1+C2+C3)->accept; GCS(C1+C2+C4)->accept;
        // any missing/provider-conflicting cause -> reject before credentials
        // or network are opened. Static key fields do not exist in the type.
        let s3 = ObjectFileStoreConfig {
            provider: ObjectStoreProvider::S3,
            bucket: "awaken-a".into(),
            prefix: "deployments/a/files".into(),
            region: Some("us-east-1".into()),
            endpoint: None,
        };
        assert!(s3.validate().is_ok());
        let gcs = ObjectFileStoreConfig {
            provider: ObjectStoreProvider::Gcs,
            bucket: "awaken-a".into(),
            prefix: "deployments/a/files".into(),
            region: None,
            endpoint: None,
        };
        assert!(gcs.validate().is_ok());
        assert!(
            ObjectFileStoreConfig {
                region: Some("us-central1".into()),
                ..gcs
            }
            .validate()
            .is_err()
        );
        assert!(
            ObjectFileStoreConfig {
                prefix: "../shared".into(),
                ..s3
            }
            .validate()
            .is_err()
        );
        assert!(
            ObjectFileStoreConfig {
                provider: ObjectStoreProvider::S3,
                bucket: "awaken-a".into(),
                prefix: "deployments/a/files".into(),
                region: Some("  ".into()),
                endpoint: None,
            }
            .validate()
            .is_err(),
            "blank S3 region is not a deployment coordinate"
        );
    }

    /// Live round-trip against a real object store (MinIO/S3). Skips unless
    /// `AWAKEN_TEST_S3_ENDPOINT` is set (see `crates/resources/docker-compose.test.yml`),
    /// proving the same `FileStore` contract holds over the network backend — including
    /// the content id, which is computed in the core and so matches every other backend.
    #[tokio::test]
    async fn minio_conforms() -> Result<(), Box<dyn std::error::Error>> {
        let Ok(endpoint) = std::env::var("AWAKEN_TEST_S3_ENDPOINT") else {
            println!("[skip] no object store reachable (AWAKEN_TEST_S3_ENDPOINT)");
            return Ok(());
        };
        use object_store::aws::AmazonS3Builder;
        let s3: Arc<dyn ObjectStore> = Arc::new(
            AmazonS3Builder::new()
                .with_endpoint(endpoint)
                .with_region("us-east-1")
                .with_bucket_name("awaken-blobs")
                .with_access_key_id("minioadmin")
                .with_secret_access_key("minioadmin")
                .with_allow_http(true) // path-style plain-HTTP MinIO
                .build()?,
        );
        let prefix = format!("blobs/{}", std::process::id());
        object_store_conformance(s3, &prefix).await
    }
}
