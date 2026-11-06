// @formal entrypoint object_store.configuration_is_secret_free_and_provider_exact
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixShape {
    Missing,
    Invalid,
    Normalized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionShape {
    Absent,
    Blank,
    Named,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObjectStoreConfigShape {
    bucket_nonblank: bool,
    prefix: PrefixShape,
    region: RegionShape,
    endpoint_present: bool,
}

fn classify_prefix_bytes(bytes: &[u8]) -> PrefixShape {
    if bytes.iter().all(|byte| *byte == b'/') {
        return PrefixShape::Missing;
    }
    let mut segment_start = 0;
    for index in 0..=bytes.len() {
        if index < bytes.len() && bytes[index] != b'/' {
            continue;
        }
        let segment = &bytes[segment_start..index];
        if segment.is_empty() || segment == b"." || segment == b".." {
            return PrefixShape::Invalid;
        }
        segment_start = index + 1;
    }
    PrefixShape::Normalized
}

fn classify_prefix(prefix: &str) -> PrefixShape {
    classify_prefix_bytes(prefix.as_bytes())
}

fn ascii_has_nonblank(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| !matches!(*byte, b'\t'..=b'\r' | b' '))
}

fn bucket_has_nonblank(bucket: &str) -> bool {
    if bucket.is_ascii() {
        // `str::trim` includes vertical tab; `u8::is_ascii_whitespace` does not.
        ascii_has_nonblank(bucket.as_bytes())
    } else {
        !bucket.trim().is_empty()
    }
}

fn object_store_config_shape(
    bucket: &str,
    prefix: &str,
    region: Option<&str>,
    endpoint: Option<&str>,
) -> ObjectStoreConfigShape {
    ObjectStoreConfigShape {
        bucket_nonblank: bucket_has_nonblank(bucket),
        prefix: classify_prefix(prefix),
        region: match region {
            None => RegionShape::Absent,
            Some(value) if !bucket_has_nonblank(value) => RegionShape::Blank,
            Some(_) => RegionShape::Named,
        },
        endpoint_present: endpoint.is_some(),
    }
}

const fn object_store_config_kernel(
    provider: ObjectStoreProvider,
    shape: ObjectStoreConfigShape,
) -> ObjectStoreConfigDecision {
    if !shape.bucket_nonblank || matches!(shape.prefix, PrefixShape::Missing) {
        return ObjectStoreConfigDecision::MissingBucketOrPrefix;
    }
    if matches!(shape.prefix, PrefixShape::Invalid) {
        return ObjectStoreConfigDecision::InvalidPrefix;
    }
    match provider {
        ObjectStoreProvider::S3 if !matches!(shape.region, RegionShape::Named) => {
            ObjectStoreConfigDecision::MissingS3Region
        }
        ObjectStoreProvider::Gcs
            if shape.endpoint_present || !matches!(shape.region, RegionShape::Absent) =>
        {
            ObjectStoreConfigDecision::GcsHasS3Coordinates
        }
        ObjectStoreProvider::S3 | ObjectStoreProvider::Gcs => ObjectStoreConfigDecision::Accept,
    }
}

fn object_store_config_decision(
    provider: ObjectStoreProvider,
    bucket: &str,
    prefix: &str,
    region: Option<&str>,
    endpoint: Option<&str>,
) -> ObjectStoreConfigDecision {
    object_store_config_kernel(
        provider,
        object_store_config_shape(bucket, prefix, region, endpoint),
    )
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
            &self.bucket,
            &self.prefix,
            self.region.as_deref(),
            self.endpoint.as_deref(),
        );
        let invalid = |detail: &str| FileStoreError::InvalidConfiguration {
            detail: detail.into(),
        };
        match decision {
            ObjectStoreConfigDecision::Accept => Ok(()),
            ObjectStoreConfigDecision::MissingBucketOrPrefix => {
                Err(invalid("object bucket and prefix must be non-empty"))
            }
            ObjectStoreConfigDecision::InvalidPrefix => {
                Err(invalid("object prefix must be a normalized relative path"))
            }
            ObjectStoreConfigDecision::MissingS3Region => {
                Err(invalid("S3 object backing requires a region"))
            }
            ObjectStoreConfigDecision::GcsHasS3Coordinates => Err(invalid(
                "GCS object backing does not accept S3 region or endpoint fields",
            )),
        }
    }
}

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError::Indeterminate {
        detail: x.to_string(),
    }
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
                let region = config.region.as_deref().ok_or_else(|| {
                    FileStoreError::InvalidConfiguration {
                        detail: "S3 object backing requires a region".into(),
                    }
                })?;
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
                    Err(FileStoreError::CorruptContent { content_id: id })
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
                crate::verify_blob(id, &bytes)?;
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
#[path = "../tests/support/s3_fixture.rs"]
mod s3_fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    #[test]
    fn bucket_shape_preserves_original_trim_rule() {
        // Causes: C1 zero, one or two ASCII bytes including control/space;
        // C2 non-ASCII whitespace with or without visible content. Effects:
        // E1 the sole classifier agrees with `!trim().is_empty()` for every
        // C1 combination; E2 C2 retains the Unicode rule. Decision rows:
        // R1 all whitespace -> false; R2 any non-whitespace -> true; R3
        // non-ASCII -> the original Unicode result, not ASCII approximation.
        assert!(!bucket_has_nonblank(""), "R1 empty");
        for first in 0_u8..=127 {
            let one = String::from_utf8(vec![first]).unwrap();
            assert_eq!(
                bucket_has_nonblank(&one),
                !one.trim().is_empty(),
                "R1/R2 {first}"
            );
            for second in 0_u8..=127 {
                let two = String::from_utf8(vec![first, second]).unwrap();
                assert_eq!(
                    bucket_has_nonblank(&two),
                    !two.trim().is_empty(),
                    "R1/R2 {first},{second}",
                );
            }
        }
        for bucket in [
            "\u{2003}",
            " \u{2003} ",
            "\u{2003}a",
            "é",
            "\u{00a0}\u{2003}",
        ] {
            assert_eq!(
                bucket_has_nonblank(bucket),
                !bucket.trim().is_empty(),
                "R3 {bucket:?}"
            );
        }
    }

    #[test]
    fn prefix_shape_matches_original_string_rules() {
        // Cause/effect graph: C1=empty or slash-only prefix, C2=a nonempty
        // path with an empty, dot, or parent segment, C3=all ordinary nonempty
        // segments. Effects: C1 -> Missing before C2, C2 -> Invalid, C3 ->
        // Normalized. Decision table exhausts every 0..=6-byte ASCII path
        // over '/', '.', and 'p'; Unicode examples preserve UTF-8 boundaries
        // while exercising slash and dot decisions. The original string rules
        // below are a test oracle only, never another production validator.
        let original = |prefix: &str| {
            if prefix.trim_matches('/').is_empty() {
                PrefixShape::Missing
            } else if prefix
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            {
                PrefixShape::Invalid
            } else {
                PrefixShape::Normalized
            }
        };
        let alphabet = ['/', '.', 'p'];
        for length in 0..=6_u32 {
            for mut encoded in 0..3_usize.pow(length) {
                let mut prefix = String::new();
                for _ in 0..length {
                    prefix.push(alphabet[encoded % alphabet.len()]);
                    encoded /= alphabet.len();
                }
                assert_eq!(classify_prefix(&prefix), original(&prefix), "{prefix:?}");
            }
        }
        for prefix in [
            "é",
            "é/p",
            "/é",
            "é/",
            "é//p",
            "é/./p",
            "é/../p",
            "\u{2003}",
            "p/\u{2003}",
            "e\u{301}",
        ] {
            assert_eq!(classify_prefix(prefix), original(prefix), "{prefix:?}");
        }
    }

    fn minio_backend(
        secret_key: &str,
    ) -> Result<Option<Arc<dyn ObjectStore>>, Box<dyn std::error::Error>> {
        let Some(endpoint) = s3_fixture::endpoint()? else {
            return Ok(None);
        };
        let access_key = s3_fixture::required_field("AWAKEN_TEST_S3_ACCESS_KEY")?;
        Ok(Some(Arc::new(
            object_store::aws::AmazonS3Builder::new()
                .with_endpoint(endpoint)
                .with_region("us-east-1")
                .with_bucket_name("awaken-blobs")
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key)
                // Bound negative probes while retaining transport recovery for
                // a stale pooled connection on the native Windows substrate.
                .with_retry(object_store::RetryConfig {
                    max_retries: 2,
                    retry_timeout: std::time::Duration::from_secs(5),
                    ..Default::default()
                })
                .with_client_options(
                    object_store::ClientOptions::new()
                        // Keep HTTP admission with the timeout options: replacing
                        // client options resets flags set on the outer builder.
                        .with_allow_http(true)
                        .with_connect_timeout(std::time::Duration::from_secs(2))
                        .with_timeout(std::time::Duration::from_secs(10)),
                )
                .build()?,
        )))
    }

    fn current_minio_backend() -> Result<Option<Arc<dyn ObjectStore>>, Box<dyn std::error::Error>> {
        let Some(secret_key) = s3_fixture::current_secret()? else {
            return Ok(None);
        };
        minio_backend(&secret_key)
    }

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
        // C4/E4 preserves the exact corrupted identity across both adapters;
        // unknown transport failure must not substitute for observed corruption.
        assert!(matches!(store.get(&id).await,
            Err(FileStoreError::CorruptContent { content_id }) if content_id == id));
        assert!(matches!(store.put(bytes).await,
            Err(FileStoreError::CorruptContent { content_id }) if content_id == id));

        backend.delete(&partial).await?;
        assert!(store.delete(&id).await?);
        assert!(!store.delete(&id).await?);
        assert!(store.get("../outside").await?.is_none());
        assert!(!store.delete("../outside").await?);
        // The medium-specific corruption/partial-upload checks above and the
        // shared FileStore set/empty rules both run on memory and MinIO.
        // F1 distinct/replayed bytes -> exact ordered set; F2 zero bytes ->
        // stable id, readable empty payload, then absence after deletion.
        let set_store = ObjectFileStore::new(Arc::clone(&backend), format!("{prefix}/set"));
        crate::tests::list_sorted_and_dedup(&set_store).await;
        crate::tests::empty_bytes_round_trip(&store).await;
        Ok(())
    }

    #[tokio::test]
    async fn in_memory_object_store_conforms() -> Result<(), Box<dyn std::error::Error>> {
        // Dependency compatibility P2: the patched SDK/RNG graph retains the
        // existing F1/F2 content-id, replay, exact-set, empty-read and delete
        // effects below. The shared table owns those assertions; no replacement
        // object-store or cryptographic implementation is introduced here.
        object_store_conformance(Arc::new(InMemory::new()), "blobs").await
    }

    #[test]
    fn allocation_config_is_secret_free_and_provider_exact() {
        // Cause/effect graph: C1=bucket has non-whitespace content;
        // C2=prefix has one or more nonempty ordinary relative segments;
        // C3=S3 region is present and nonblank; C4=GCS region and endpoint
        // are absent. E1=accept, E2=InvalidConfiguration with the exact
        // failing shape. Decision rules: R1=S3+C1+C2+C3 -> E1 (endpoint
        // optional); R2=GCS+C1+C2+C4 -> E1; R3=!C1 or empty C2 -> E2
        // bucket/prefix; R4=malformed C2 -> E2 prefix; R5=S3+!C3 -> E2
        // region; R6=GCS+!C4 -> E2 GCS coordinates. validate() is the
        // authority before SDK credential discovery or network access;
        // R3 precedes R4, which precedes R5/R6 when causes overlap. Static
        // key fields do not exist in the configuration type.
        let s3 = ObjectFileStoreConfig {
            provider: ObjectStoreProvider::S3,
            bucket: "awaken-a".into(),
            prefix: "deployments/a/files".into(),
            region: Some("us-east-1".into()),
            endpoint: None,
        };
        assert!(s3.validate().is_ok());
        assert!(
            ObjectFileStoreConfig {
                prefix: "blobs".into(),
                ..s3.clone()
            }
            .validate()
            .is_ok()
        );
        assert!(
            ObjectFileStoreConfig {
                endpoint: Some("https://objects.example".into()),
                ..s3.clone()
            }
            .validate()
            .is_ok()
        );
        let gcs = ObjectFileStoreConfig {
            provider: ObjectStoreProvider::Gcs,
            bucket: "awaken-a".into(),
            prefix: "deployments/a/files".into(),
            region: None,
            endpoint: None,
        };
        assert!(gcs.validate().is_ok());
        let invalid_rows = [
            (
                "R3 empty bucket",
                ObjectFileStoreConfig {
                    bucket: "".into(),
                    ..s3.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R3 whitespace bucket",
                ObjectFileStoreConfig {
                    bucket: " \t ".into(),
                    ..s3.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R3 Unicode whitespace bucket",
                ObjectFileStoreConfig {
                    bucket: "\u{2003}".into(),
                    ..s3.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R3 empty prefix",
                ObjectFileStoreConfig {
                    prefix: "".into(),
                    ..s3.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R3 slash-only prefix",
                ObjectFileStoreConfig {
                    prefix: "/".into(),
                    ..s3.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R3 wins over invalid prefix and missing S3 region",
                ObjectFileStoreConfig {
                    bucket: "".into(),
                    prefix: "/bad".into(),
                    region: None,
                    ..s3.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R4 wins over missing S3 region",
                ObjectFileStoreConfig {
                    prefix: "/bad".into(),
                    region: None,
                    ..s3.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            (
                "R4 leading slash",
                ObjectFileStoreConfig {
                    prefix: "/blobs".into(),
                    ..s3.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            (
                "R4 trailing slash",
                ObjectFileStoreConfig {
                    prefix: "blobs/".into(),
                    ..s3.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            (
                "R4 empty segment",
                ObjectFileStoreConfig {
                    prefix: "blobs//a".into(),
                    ..s3.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            (
                "R4 dot segment",
                ObjectFileStoreConfig {
                    prefix: "blobs/./a".into(),
                    ..s3.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            (
                "R4 parent segment",
                ObjectFileStoreConfig {
                    prefix: "blobs/../a".into(),
                    ..s3.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            (
                "R6 GCS present empty endpoint still conflicts",
                ObjectFileStoreConfig {
                    endpoint: Some(String::new()),
                    ..gcs.clone()
                },
                "GCS object backing does not accept S3 region or endpoint fields",
            ),
            (
                "R3 GCS empty bucket wins over prefix and coordinates",
                ObjectFileStoreConfig {
                    bucket: String::new(),
                    prefix: "/bad".into(),
                    region: Some("r".into()),
                    endpoint: Some("e".into()),
                    ..gcs.clone()
                },
                "object bucket and prefix must be non-empty",
            ),
            (
                "R4 GCS invalid prefix wins over coordinates",
                ObjectFileStoreConfig {
                    prefix: "/bad".into(),
                    region: Some("r".into()),
                    endpoint: Some("e".into()),
                    ..gcs.clone()
                },
                "object prefix must be a normalized relative path",
            ),
            // R5's Unicode shape is outside Kani's one-ASCII-space bound.
            (
                "R5 Unicode blank region",
                ObjectFileStoreConfig {
                    region: Some("\u{2003}".into()),
                    ..s3.clone()
                },
                "S3 object backing requires a region",
            ),
        ];
        for (rule, config, expected_detail) in invalid_rows {
            match config.validate() {
                Err(FileStoreError::InvalidConfiguration { detail }) => {
                    assert_eq!(detail, expected_detail, "{rule}");
                }
                other => panic!("{rule}: expected InvalidConfiguration, got {other:?}"),
            }
        }

        // R1/R2/R5/R6 cross-product: both providers x four region shapes x
        // two endpoint states = 16 rules. A valid bucket/prefix holds C1/C2
        // true. S3 needs only the named region (endpoint is irrelevant);
        // GCS needs both coordinates absent. Effects are exact acceptance or
        // the corresponding InvalidConfiguration detail, including both
        // conflicting GCS coordinates and S3 invalid region plus endpoint.
        for provider in [ObjectStoreProvider::S3, ObjectStoreProvider::Gcs] {
            for (region_case, region) in [None, Some(""), Some(" "), Some("r")]
                .into_iter()
                .enumerate()
            {
                for endpoint_present in [false, true] {
                    let config = ObjectFileStoreConfig {
                        provider,
                        bucket: "b".into(),
                        prefix: "p".into(),
                        region: region.map(str::to_owned),
                        endpoint: endpoint_present.then(|| "e".into()),
                    };
                    let expected: Result<(), FileStoreError> = match provider {
                        ObjectStoreProvider::S3 if region_case == 3 => Ok(()),
                        ObjectStoreProvider::S3 => Err(FileStoreError::InvalidConfiguration {
                            detail: "S3 object backing requires a region".into(),
                        }),
                        ObjectStoreProvider::Gcs if region_case == 0 && !endpoint_present => Ok(()),
                        ObjectStoreProvider::Gcs => Err(FileStoreError::InvalidConfiguration {
                            detail:
                                "GCS object backing does not accept S3 region or endpoint fields"
                                    .into(),
                        }),
                    };
                    assert_eq!(
                        config.validate(),
                        expected,
                        "provider={provider:?} region_case={region_case} endpoint_present={endpoint_present}"
                    );
                }
            }
        }
    }

    /// Live round-trip against a real object store (MinIO/S3). Skips unless
    /// `AWAKEN_TEST_S3_ENDPOINT` is set (see `crates/platform/resources/docker-compose.test.yml`),
    /// proving the same `FileStore` contract holds over the network backend — including
    /// the content id, which is computed in the core and so matches every other backend.
    #[tokio::test]
    async fn minio_conforms() -> Result<(), Box<dyn std::error::Error>> {
        let Some(s3) = current_minio_backend()? else {
            return Ok(());
        };
        let forbidden = s3
            .put(
                &ObjPath::from("outside/forbidden"),
                PutPayload::from_static(b"must be denied"),
            )
            .await;
        assert!(
            matches!(forbidden, Err(object_store::Error::PermissionDenied { .. })),
            "least-privilege identity must receive authorization denial outside its prefixes"
        );
        let prefix = format!("contract/{}", std::process::id());
        object_store_conformance(s3, &prefix).await?;
        s3_fixture::report_exercised();
        Ok(())
    }

    #[test]
    fn minio_conforms_fixture_admission() {
        s3_fixture::assert_child_contract("object::tests::minio_conforms");
    }

    #[tokio::test]
    async fn minio_rotation_seed() -> Result<(), Box<dyn std::error::Error>> {
        let Some(rotation_id) = s3_fixture::rotation_id()? else {
            return Ok(());
        };
        let backend = current_minio_backend()?
            .ok_or("credential-rotation seed requires the live object-store endpoint")?;
        let store = ObjectFileStore::new(backend, format!("rotation/{rotation_id}"));
        let id = store.put(b"credential-rotation-durable-object").await?;
        assert_eq!(id, content_id(b"credential-rotation-durable-object"));
        s3_fixture::report_exercised();
        Ok(())
    }

    #[tokio::test]
    async fn minio_rotated_credentials_revoke_old_secret_and_preserve_objects()
    -> Result<(), Box<dyn std::error::Error>> {
        // Metamorphic design: only the credential changes. The old client must
        // lose access, while the replacement client observes the same immutable
        // object id and bytes under the unchanged bucket/prefix coordinate.
        let Some(rotation_id) = s3_fixture::rotation_id()? else {
            return Ok(());
        };
        let old_secret = s3_fixture::required_field("AWAKEN_TEST_S3_OLD_SECRET_KEY")?;
        let old_backend = minio_backend(&old_secret)?
            .ok_or("credential-rotation verification requires the live endpoint")?;
        let prefix = format!("rotation/{rotation_id}");
        let id = content_id(b"credential-rotation-durable-object");
        let revoked = old_backend
            .get(&ObjPath::from(format!("{prefix}/{id}")))
            .await;
        assert!(
            matches!(
                revoked,
                Err(object_store::Error::PermissionDenied { .. }
                    | object_store::Error::Unauthenticated { .. })
            ),
            "revoked object-store secret must receive authentication/authorization denial"
        );

        let new_backend = current_minio_backend()?
            .ok_or("credential-rotation verification requires replacement credentials")?;
        let new_store = ObjectFileStore::new(new_backend, prefix);
        assert_eq!(
            new_store.get(&id).await?.as_deref(),
            Some(b"credential-rotation-durable-object".as_slice())
        );
        s3_fixture::report_exercised();
        Ok(())
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_PROVIDER: [ObjectStoreProvider; 2] =
        [ObjectStoreProvider::S3, ObjectStoreProvider::Gcs];

    const fn listed_provider(provider: ObjectStoreProvider) -> usize {
        match provider {
            ObjectStoreProvider::S3 => 0,
            ObjectStoreProvider::Gcs => 1,
        }
    }

    const EVERY_PREFIX_SHAPE: [PrefixShape; 3] = [
        PrefixShape::Missing,
        PrefixShape::Invalid,
        PrefixShape::Normalized,
    ];

    const fn listed_prefix_shape(shape: PrefixShape) -> usize {
        match shape {
            PrefixShape::Missing => 0,
            PrefixShape::Invalid => 1,
            PrefixShape::Normalized => 2,
        }
    }

    const EVERY_REGION_SHAPE: [RegionShape; 3] =
        [RegionShape::Absent, RegionShape::Blank, RegionShape::Named];

    const fn listed_region_shape(shape: RegionShape) -> usize {
        match shape {
            RegionShape::Absent => 0,
            RegionShape::Blank => 1,
            RegionShape::Named => 2,
        }
    }

    #[test]
    fn every_object_store_shape_is_listed_once() {
        for (index, provider) in EVERY_PROVIDER.into_iter().enumerate() {
            assert_eq!(listed_provider(provider), index);
        }
        for (index, shape) in EVERY_PREFIX_SHAPE.into_iter().enumerate() {
            assert_eq!(listed_prefix_shape(shape), index);
        }
        for (index, shape) in EVERY_REGION_SHAPE.into_iter().enumerate() {
            assert_eq!(listed_region_shape(shape), index);
        }
    }

    /// Cause/effect rule K: enumerate both providers, both bucket classes,
    /// all three prefix classes, all three region classes and both endpoint
    /// states (72 combinations). Missing bucket/prefix wins first, invalid
    /// prefix second, then provider-specific coordinates decide; both exact
    /// rejection categories and acceptance are checked against this table.
    /// Raw-string classification is a separate premise checked below; this
    /// finite proof does not claim arbitrary UTF-8 classification.
    /// @exhaustive ObjectStoreProvider(2) x bool(2) x PrefixShape(3) x RegionShape(3) x bool(2) = 72
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_finite_kernel_accepts_exactly_compatible_shapes() {
        for provider in EVERY_PROVIDER {
            for bucket_nonblank in [false, true] {
                for prefix in EVERY_PREFIX_SHAPE {
                    for region in EVERY_REGION_SHAPE {
                        for endpoint_present in [false, true] {
                            let shape = ObjectStoreConfigShape {
                                bucket_nonblank,
                                prefix,
                                region,
                                endpoint_present,
                            };
                            let expected =
                                match (bucket_nonblank, prefix, provider, region, endpoint_present)
                                {
                                    (false, _, _, _, _) | (_, PrefixShape::Missing, _, _, _) => {
                                        ObjectStoreConfigDecision::MissingBucketOrPrefix
                                    }
                                    (_, PrefixShape::Invalid, _, _, _) => {
                                        ObjectStoreConfigDecision::InvalidPrefix
                                    }
                                    (
                                        true,
                                        PrefixShape::Normalized,
                                        ObjectStoreProvider::S3,
                                        RegionShape::Named,
                                        _,
                                    )
                                    | (
                                        true,
                                        PrefixShape::Normalized,
                                        ObjectStoreProvider::Gcs,
                                        RegionShape::Absent,
                                        false,
                                    ) => ObjectStoreConfigDecision::Accept,
                                    (
                                        true,
                                        PrefixShape::Normalized,
                                        ObjectStoreProvider::S3,
                                        _,
                                        _,
                                    ) => ObjectStoreConfigDecision::MissingS3Region,
                                    (
                                        true,
                                        PrefixShape::Normalized,
                                        ObjectStoreProvider::Gcs,
                                        _,
                                        _,
                                    ) => ObjectStoreConfigDecision::GcsHasS3Coordinates,
                                };
                            assert_eq!(object_store_config_kernel(provider, shape), expected);
                        }
                    }
                }
            }
        }
    }

    /// Cause/effect rule S3-raw: a valid literal bucket and prefix hold their
    /// two classifications fixed; the actual production decision receives an
    /// absent, one-space or named region and an independently absent/present
    /// endpoint. S3 accepts exactly the named region in both endpoint states;
    /// the other four cases report MissingS3Region. This proves only these
    /// borrowed raw representatives, not arbitrary UTF-8 region strings.
    /// @exhaustive region representative(3) x bool(2) = 6
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_derives_s3_region_from_raw_representatives() {
        for region_case in 0_u8..3 {
            for endpoint_present in [false, true] {
                let region = match region_case {
                    0 => None,
                    1 => Some(" "),
                    _ => Some("r"),
                };
                let decision = object_store_config_decision(
                    ObjectStoreProvider::S3,
                    "b",
                    "p",
                    region,
                    endpoint_present.then_some("e"),
                );
                assert_eq!(
                    decision,
                    if region_case == 2 {
                        ObjectStoreConfigDecision::Accept
                    } else {
                        ObjectStoreConfigDecision::MissingS3Region
                    }
                );
            }
        }
    }

    /// Cause/effect rule BP-raw: an empty or named bucket crosses a missing,
    /// malformed or normalized prefix. With valid S3 coordinates, the real
    /// production decision reports Missing first, Invalid second, otherwise
    /// Accept. Six raw-string combinations expose precedence without assuming
    /// preclassified bucket/prefix bits; longer and Unicode inputs belong to
    /// the executable differential tests, not this bounded proof.
    /// @exhaustive bool(2) x prefix representative(3) = 6
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_derives_bucket_prefix_from_raw_representatives() {
        for bucket_named in [false, true] {
            for prefix_case in 0_u8..3 {
                let bucket = if bucket_named { "b" } else { "" };
                let prefix = match prefix_case {
                    0 => "/",
                    1 => "/bad",
                    _ => "p",
                };
                let decision = object_store_config_decision(
                    ObjectStoreProvider::S3,
                    bucket,
                    prefix,
                    Some("r"),
                    None,
                );
                let expected = if !bucket_named || prefix_case == 0 {
                    ObjectStoreConfigDecision::MissingBucketOrPrefix
                } else if prefix_case == 1 {
                    ObjectStoreConfigDecision::InvalidPrefix
                } else {
                    ObjectStoreConfigDecision::Accept
                };
                assert_eq!(decision, expected);
            }
        }
    }

    /// Cause/effect rule GCS-raw: with valid literal bucket and prefix,
    /// absent, blank and named raw region options cross an independent absent
    /// or present endpoint. GCS accepts only both absent; all five other
    /// cases reject S3 coordinates, including an empty but present region.
    /// This is a bounded representative proof, not arbitrary UTF-8 admission.
    /// @exhaustive region representative(3) x bool(2) = 6
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_derives_gcs_coordinates_from_raw_representatives() {
        for region_case in 0_u8..3 {
            for endpoint_present in [false, true] {
                let region = match region_case {
                    0 => None,
                    1 => Some(" "),
                    _ => Some("r"),
                };
                let decision = object_store_config_decision(
                    ObjectStoreProvider::Gcs,
                    "b",
                    "p",
                    region,
                    endpoint_present.then_some("e"),
                );
                assert_eq!(
                    decision,
                    if region_case == 0 && !endpoint_present {
                        ObjectStoreConfigDecision::Accept
                    } else {
                        ObjectStoreConfigDecision::GcsHasS3Coordinates
                    }
                );
            }
        }
    }

    /// Cause/effect rule B-ASCII: zero to two bytes from the admitted ASCII
    /// alphabet {space, 'a'} feed the production ASCII classifier. All-space
    /// bytes are blank; any 'a' makes the bucket or region nonblank. The
    /// production string wrapper uses this classifier only after is_ascii();
    /// executable differential tests cover every short ASCII byte and Unicode.
    /// @exhaustive bool(2) x bool(2) x length(3) = 12
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_classifies_bounded_ascii_blankness() {
        for first in [b' ', b'a'] {
            for second in [b' ', b'a'] {
                for len in 0_u8..=2 {
                    let bytes = [first, second];
                    let raw = &bytes[..usize::from(len)];
                    let expected = (len >= 1 && first == b'a') || (len == 2 && second == b'a');
                    assert_eq!(ascii_has_nonblank(raw), expected);
                }
            }
        }
    }

    /// Cause/effect rule P-ASCII: zero to three bytes from {'/', 'p'} feed
    /// the production prefix byte classifier. Empty/all slash is Missing;
    /// a leading or trailing slash is Invalid; all other shapes are
    /// Normalized. The full UTF-8 wrapper is covered by executable differential
    /// tests against the original string rule, including dot and Unicode.
    /// @exhaustive bool(2) x bool(2) x bool(2) x length(4) = 32
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_classifies_bounded_ascii_prefix() {
        for first in [b'/', b'p'] {
            for middle in [b'/', b'p'] {
                for last in [b'/', b'p'] {
                    for len in 0_u8..=3 {
                        let bytes = [first, middle, last];
                        let raw = &bytes[..usize::from(len)];
                        let expected = match len {
                            0 => PrefixShape::Missing,
                            1 if first == b'/' => PrefixShape::Missing,
                            1 => PrefixShape::Normalized,
                            2 if first == b'/' && middle == b'/' => PrefixShape::Missing,
                            2 if first == b'p' && middle == b'p' => PrefixShape::Normalized,
                            2 => PrefixShape::Invalid,
                            _ if first == b'/' && middle == b'/' && last == b'/' => {
                                PrefixShape::Missing
                            }
                            _ if first == b'p' && last == b'p' => PrefixShape::Normalized,
                            _ => PrefixShape::Invalid,
                        };
                        assert_eq!(classify_prefix_bytes(raw), expected);
                    }
                }
            }
        }
    }

    /// Cause/effect rule P-dot: a whole dot or parent segment rejects;
    /// a dot inside an ordinary segment accepts. Other slash patterns
    /// belong to P-ASCII and the executable exhaustive differential table.
    /// @exhaustive dot-segment representative(5) = 5
    /// @obligation object_store.configuration_is_secret_free_and_provider_exact
    #[test]
    fn object_store_configuration_classifies_bounded_dot_segments() {
        for prefix_case in 0_u8..5 {
            let prefix = match prefix_case {
                0 => b".".as_slice(),
                1 => b"..".as_slice(),
                2 => b"p/./q".as_slice(),
                3 => b"p/../q".as_slice(),
                _ => b"p.q".as_slice(),
            };
            assert_eq!(
                classify_prefix_bytes(prefix),
                if prefix_case == 4 {
                    PrefixShape::Normalized
                } else {
                    PrefixShape::Invalid
                }
            );
        }
    }
}
