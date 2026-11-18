//! Engine SPI for conditional publication in one logical Session partition.
//! Domain repositories supply snapshots and immutable records; the engine owns routing.
use async_trait::async_trait;
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub struct SessionObject {
    pub session_id: String,
    pub owner_scope: String,
    pub version: String,
    /// None means the Session partition exists but this component is absent.
    pub payload: Option<String>,
    pub conditions: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct ImmutableSessionRecord {
    pub id: String,
    pub kind: String,
    pub payload: String,
}
#[derive(Clone, Debug)]
pub struct SessionObjectUpdate {
    pub payload: String,
    pub records: Vec<ImmutableSessionRecord>,
    pub markers: BTreeMap<String, PublicationMarker>,
    pub condition: Option<String>,
    pub next_claim_at_ms: Option<u64>,
}
impl SessionObjectUpdate {
    pub fn snapshot(payload: String) -> Self {
        Self {
            payload,
            records: Vec::new(),
            markers: BTreeMap::new(),
            condition: None,
            next_claim_at_ms: None,
        }
    }
}
/// One bounded discovery pass. The engine owns the meaning of the offset.
pub struct RunnableSessions {
    pub sessions: Vec<SessionObject>,
    pub resume_after: Option<String>,
}

/// Engine SPI. A version fences the whole Session partition, including other
/// domain components; it is never a local cache version. Errors preserve an
/// unknown publication outcome. Compare-and-exchange false proves no apply.
#[async_trait]
pub trait SessionObjectStore: Send + Sync {
    async fn load(&self, session_id: &str) -> Result<Option<SessionObject>, PublicationError>;
    async fn insert(
        &self,
        owner_scope: &str,
        session_id: &str,
        update: &SessionObjectUpdate,
    ) -> Result<bool, PublicationError>;
    async fn replace(
        &self,
        observed: &SessionObject,
        update: &SessionObjectUpdate,
    ) -> Result<bool, PublicationError>;
    async fn list(&self) -> Result<Vec<SessionObject>, PublicationError>;

    /// Read a bounded part of the scheduling index. A supplied offset refers
    /// to the last inspected index entry, even if it yielded no live Session.
    async fn scan_runnable_sessions(
        &self,
        _due_at_ms: u64,
        _shard: Option<u8>,
        _after: Option<&str>,
        _limit: usize,
    ) -> Result<RunnableSessions, PublicationError> {
        Err(PublicationError::NotApplied(
            "bounded Dispatch discovery is unsupported".into(),
        ))
    }

    /// Candidate prefixes may include future work. Exact discovery checks time.
    async fn claimable_shards(&self, _due_at_ms: u64) -> Result<Option<Vec<u8>>, PublicationError> {
        Ok(None)
    }

    /// The index is initialized after existing partitions have been backfilled.
    async fn claim_index_initialized(&self) -> Result<bool, PublicationError> {
        Ok(true)
    }

    async fn finish_claim_index_backfill(&self) -> Result<(), PublicationError> {
        Ok(())
    }
    /// Observe a condition and optionally the exact version of the preceding snapshot.
    async fn validate_condition(
        &self,
        _session_id: &str,
        _token: &str,
        _version: Option<&str>,
    ) -> Result<bool, PublicationError> {
        Err(PublicationError::NotApplied(
            "conditional observation is unsupported".into(),
        ))
    }
    /// Stable opaque namespace for portable cross-partition scan cursors.
    async fn scan_namespace(&self) -> Result<String, PublicationError> {
        Err(PublicationError::NotApplied(
            "partition scan namespace is unsupported".into(),
        ))
    }
    /// Resolve or reserve routing for one acquisition request. This reserve
    /// grants no claim. Its unique key serializes the choice of Session across
    /// retries; the Session still commits the claim and its receipt together.
    async fn claim_request_partition(
        &self,
        _identity: &str,
        _expires_at_ms: u64,
        _proposed: Option<&SessionObject>,
    ) -> Result<Option<SessionObject>, PublicationError> {
        Err(PublicationError::NotApplied(
            "claim request routing is unsupported".into(),
        ))
    }

    /// Durable lower bound below which a retired request cannot be recreated.
    async fn claim_request_cutoff(&self) -> Result<u64, PublicationError> {
        Ok(0)
    }

    /// Resolve a retained immutable identity. A route alone is not evidence:
    /// the engine also verifies publication in the target component.
    async fn locate_record(&self, _id: &str) -> Result<Option<SessionObject>, PublicationError> {
        Err(PublicationError::NotApplied(
            "immutable identity lookup is unsupported".into(),
        ))
    }
}

/// Technical publication outcome; domain repositories translate it into their own errors.
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    #[error("object identity belongs to an existing partition")]
    AlreadyExists,
    #[error("publication was refused before applying: {0}")]
    NotApplied(String),
    #[error("publication authority unavailable or outcome unknown: {0}")]
    AuthorityUnavailable(String),
    #[error("corrupt object publication: {0}")]
    Corrupt(String),
    #[error("invalid object publication: {0}")]
    InvalidMutation(String),
}

/// Component-owned authority value; the engine hashes it before publication.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PublicationMarker {
    pub value: String,
    pub expires_ms: u64,
}

/// AWS-compatible transport selected only by concrete object adapters.
/// Domain repositories using the publication SPI do not enable this feature.
#[cfg(feature = "s3")]
pub fn s3_transport(
    bucket: &str,
    region: Option<&str>,
    endpoint: Option<&str>,
    allow_http: bool,
) -> Result<std::sync::Arc<dyn object_store::ObjectStore>, object_store::Error> {
    let mut builder = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(allow_http);
    if let Some(region) = region {
        builder = builder.with_region(region);
    }
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    builder
        .build()
        .map(|store| std::sync::Arc::new(store) as std::sync::Arc<dyn object_store::ObjectStore>)
}
