//! Published versioned runtime-config registry contracts.

use super::pinned_registry as canonical;
use crate::contract::scope::{ScopeError, ScopeId};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::marker::PhantomData;
use std::sync::Arc;
use thiserror::Error;

/// A concrete published version reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct VersionRef {
    pub kind: String,
    pub id: String,
    pub version: u64,
}

/// One immutable published runtime-config version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionedRecord<T> {
    pub kind: String,
    pub id: String,
    pub version: u64,
    pub content_hash: String,
    pub value_schema_version: u32,
    pub value: T,
    pub canonical_json_bytes: Vec<u8>,
    pub created_at_ms: u64,
    pub metadata: Value,
}

impl<T> VersionedRecord<T> {
    /// Recompute the SHA-256 of `canonical_json_bytes` and verify it matches
    /// the stored `content_hash`. ADR-0035 D3/D9 require this check before
    /// trusting a record loaded from a store — without it, a column-level
    /// rewrite of `value` or `canonical_json_bytes` would go undetected and
    /// `PinnedRegistryEntry.content_hash` would become decorative.
    pub fn verify_content_hash(&self) -> Result<(), VersionedRegistryError> {
        let digest = Sha256::digest(&self.canonical_json_bytes);
        let actual = format!("sha256:{digest:x}");
        if actual != self.content_hash {
            return Err(VersionedRegistryError::Backend(format!(
                "stored content_hash {stored} does not match recomputed digest {actual} \
                 for {kind}/{id}@{version}",
                stored = self.content_hash,
                kind = self.kind,
                id = self.id,
                version = self.version,
            )));
        }
        Ok(())
    }
}

/// Result of publishing a resource version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublishOutcome<T> {
    Created(VersionedRecord<T>),
    Noop(VersionedRecord<T>),
}

/// One resource value to publish inside an atomic registry publication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryResourcePublish {
    pub kind: String,
    pub id: String,
    pub value: Value,
    pub value_schema_version: u32,
    pub metadata: Value,
}

/// Mutable state for one published resource identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionedResourceState {
    pub scope_id: String,
    pub kind: String,
    pub id: String,
    pub current_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub metadata: Value,
}

impl VersionedResourceState {
    /// Reject publish/rollback when the resource is already archived
    /// (ADR-0035 D6). Returns `Archived` with the caller-supplied
    /// kind/id so the error matches the request, not the persisted row.
    pub fn ensure_not_archived(&self, kind: &str, id: &str) -> Result<(), VersionedRegistryError> {
        if self.archived_at_ms.is_some() {
            Err(VersionedRegistryError::Archived {
                kind: kind.to_string(),
                id: id.to_string(),
            })
        } else {
            Ok(())
        }
    }
}

/// Editing-store revision included in a publication audit boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigRevisionRef {
    pub namespace: String,
    pub id: String,
    pub revision: u64,
}

/// Atomic published graph snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryPublication {
    pub publication_id: String,
    pub scope_id: String,
    pub snapshot_version: u64,
    pub entries: Vec<VersionRef>,
    #[serde(default)]
    pub source_config_revisions: Vec<ConfigRevisionRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at_ms: u64,
    pub metadata: Value,
}

pub use super::pinned_registry::{PinnedRegistryEntry, PinnedRegistryManifest};

/// Errors returned by versioned registry stores.
///
/// Marked `#[non_exhaustive]` so adding variants in future minor
/// releases does not break downstream `match` arms.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VersionedRegistryError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already archived: {kind}/{id}")]
    Archived { kind: String, id: String },
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error(
        "incompatible schema: {kind}/{id}@{version} stored as schema v{stored} but reader \
         supports {supported:?}"
    )]
    IncompatibleSchema {
        kind: String,
        id: String,
        version: u64,
        stored: u32,
        supported: Vec<u32>,
    },
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("versioned registry error: {0}")]
    Backend(String),
}

impl From<canonical::PinnedRegistryHashError> for VersionedRegistryError {
    fn from(error: canonical::PinnedRegistryHashError) -> Self {
        use canonical::PinnedRegistryHashError as Hash;
        match error {
            Hash::Serialization(message) => VersionedRegistryError::Serialization(message),
            Hash::InvalidRequest(message) => VersionedRegistryError::InvalidRequest(message),
        }
    }
}

/// Canonical JSON bytes used as the persisted hash source (ADR-0035 D3).
///
/// Delegates to the single canonicalization implementation in
/// [`awaken_runtime_contract::contract::pinned_registry`] so a locally pinned
/// manifest entry and a server-published version always hash identically.
pub fn canonical_registry_json_bytes(
    value_schema_version: u32,
    value: &Value,
) -> Result<Vec<u8>, VersionedRegistryError> {
    Ok(canonical::canonical_registry_json_bytes(
        value_schema_version,
        value,
    )?)
}

/// ADR-0035 D19 retention policy. A historical version is eligible for
/// physical purge only when every rule below allows it; the absence of a
/// rule is permissive.
#[derive(Debug, Clone, Default)]
pub struct RegistryRetentionPolicy {
    /// Keep at least this many of the most recent historical versions per
    /// resource (in addition to the current pointer). `None` disables the
    /// per-resource floor.
    pub keep_last_versions: Option<u64>,
    /// Keep versions whose `created_at_ms` is within this many milliseconds
    /// of `now_ms`. `None` disables the age floor.
    pub keep_younger_than_ms: Option<u64>,
    /// Additional pinned versions the caller wants to protect — typically
    /// derived from retained run resolution metadata that the registry store
    /// cannot enumerate on its own.
    pub protected_versions: Vec<VersionRef>,
}

/// Capability-based view onto a versioned registry store that exposes
/// purge planning. Implementations MUST NOT purge anything in dry-run
/// mode and MUST treat current pointers and any version referenced by a
/// retained publication or `protected_versions` entry as ineligible.
#[async_trait]
pub trait VersionedRegistryRetention: Send + Sync {
    /// Return the versions eligible for purge under `policy`. When
    /// `dry_run` is true the store must not delete anything. When false,
    /// the store may delete the returned versions; callers should still
    /// inspect the returned list as the operator-facing audit record.
    async fn purge_eligible_versions(
        &self,
        scope_id: &str,
        now_ms: u64,
        policy: RegistryRetentionPolicy,
        dry_run: bool,
    ) -> Result<Vec<VersionRef>, VersionedRegistryError>;
}

/// Sort publication entries by `(kind, id)` so different store backends
/// expose the same `RegistryPublication.entries` order. Callers should
/// apply this to entries before persisting; readers can rely on the
/// resulting order being stable across backends.
#[must_use]
pub fn sort_publication_entries(mut entries: Vec<VersionRef>) -> Vec<VersionRef> {
    entries.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.id.cmp(&b.id)));
    entries
}

/// Normalize rollback metadata so it always carries `restored_from`
/// pointing at the source version, per ADR-0035 D4. Callers may supply
/// additional metadata fields, but the `restored_from` key is reserved:
/// supplying it with a value other than `to_version` is rejected, and
/// when absent it is injected automatically.
pub fn build_rollback_metadata(
    metadata: Value,
    to_version: u64,
) -> Result<Value, VersionedRegistryError> {
    let mut object = match metadata {
        Value::Null => serde_json::Map::new(),
        Value::Object(map) => map,
        other => {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "rollback metadata must be a JSON object or null, got {}",
                value_kind_name(&other)
            )));
        }
    };
    let expected = serde_json::json!(to_version);
    if let Some(existing) = object.get("restored_from")
        && existing != &expected
    {
        return Err(VersionedRegistryError::InvalidRequest(format!(
            "rollback metadata.restored_from must be {to_version}, got {existing}"
        )));
    }
    object.insert("restored_from".to_string(), expected);
    Ok(Value::Object(object))
}

fn value_kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Compute the ADR-0035 content hash for a canonical published value envelope.
///
/// Delegates to the single implementation in
/// [`awaken_runtime_contract::contract::pinned_registry`].
pub fn registry_content_hash(
    value_schema_version: u32,
    value: &Value,
) -> Result<(String, Vec<u8>), VersionedRegistryError> {
    Ok(canonical::registry_content_hash(
        value_schema_version,
        value,
    )?)
}

/// Async server/store contract for immutable published runtime-config versions.
#[async_trait]
pub trait VersionedRegistryStore: Send + Sync {
    async fn resource_state(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError>;

    async fn current(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError>;

    async fn get(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError>;

    async fn list_versions(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Vec<VersionedRecord<Value>>, VersionedRegistryError>;

    async fn publish_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        value: Value,
        value_schema_version: u32,
        metadata: Value,
    ) -> Result<PublishOutcome<Value>, VersionedRegistryError>;

    async fn rollback_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        to_version: u64,
        metadata: Value,
    ) -> Result<VersionedRecord<Value>, VersionedRegistryError>;

    async fn rollback_publication(
        &self,
        scope_id: &str,
        source_snapshot_version: u64,
        publication_id: &str,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        if publication_id.trim().is_empty() {
            return Err(VersionedRegistryError::InvalidRequest(
                "publication_id cannot be empty".to_string(),
            ));
        }
        let source_publication = self
            .get_publication(scope_id, source_snapshot_version)
            .await?
            .ok_or_else(|| {
                VersionedRegistryError::NotFound(format!(
                    "publication/{scope_id}@{source_snapshot_version}"
                ))
            })?;

        let mut resources = Vec::with_capacity(source_publication.entries.len());
        for entry in &source_publication.entries {
            let record = self
                .get(scope_id, &entry.kind, &entry.id, entry.version)
                .await?
                .ok_or_else(|| {
                    VersionedRegistryError::NotFound(format!(
                        "{}/{}@{}",
                        entry.kind, entry.id, entry.version
                    ))
                })?;
            resources.push(RegistryResourcePublish {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                value: record.value,
                value_schema_version: record.value_schema_version,
                metadata: json!({
                    "rollback_source_publication_id": source_publication.publication_id.clone(),
                    "rollback_source_snapshot_version": source_publication.snapshot_version,
                    "rollback_source_version": entry.version,
                    "rollback_source_content_hash": record.content_hash,
                }),
            });
        }

        self.publish_resources_and_create_publication(
            scope_id,
            publication_id,
            resources,
            source_publication.source_config_revisions,
            created_by,
            metadata,
        )
        .await
    }

    async fn archive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError>;

    async fn unarchive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError>;

    async fn publish_resources_and_create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        resources: Vec<RegistryResourcePublish>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError>;

    async fn create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        entries: Vec<VersionRef>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError>;

    async fn latest_publication(
        &self,
        scope_id: &str,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError>;

    async fn get_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError>;

    async fn pinned_manifest_for_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<PinnedRegistryManifest>, VersionedRegistryError> {
        let Some(publication) = self.get_publication(scope_id, snapshot_version).await? else {
            return Ok(None);
        };
        let mut entries = Vec::with_capacity(publication.entries.len());
        for entry in &publication.entries {
            let record = self
                .get(scope_id, &entry.kind, &entry.id, entry.version)
                .await?
                .ok_or_else(|| {
                    VersionedRegistryError::NotFound(format!(
                        "{}/{}@{}",
                        entry.kind, entry.id, entry.version
                    ))
                })?;
            entries.push(PinnedRegistryEntry {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
                version: entry.version,
                content_hash: record.content_hash,
            });
        }
        Ok(Some(PinnedRegistryManifest {
            publication_id: Some(publication.publication_id),
            registry_snapshot_version: Some(publication.snapshot_version),
            entries,
        }))
    }

    async fn latest_pinned_manifest(
        &self,
        scope_id: &str,
    ) -> Result<Option<PinnedRegistryManifest>, VersionedRegistryError> {
        let Some(publication) = self.latest_publication(scope_id).await? else {
            return Ok(None);
        };
        self.pinned_manifest_for_publication(scope_id, publication.snapshot_version)
            .await
    }
}

/// Typed view over a kind-scoped published runtime-config registry.
///
/// The underlying store remains kind-discriminated and JSON based so it can
/// publish mixed resource graphs atomically. This wrapper binds one
/// `(scope_id, kind)` pair and performs serde conversion for concrete Awaken
/// runtime-config specs.
#[derive(Clone)]
pub struct TypedVersionedRegistry<T> {
    pub store: Arc<dyn VersionedRegistryStore>,
    pub scope_id: String,
    pub kind: String,
    /// Schema versions this typed wrapper can decode without migration.
    /// Empty means "accept any schema version" — appropriate only for
    /// transitional code; production wrappers should enumerate the
    /// versions they understand (ADR-0035 D2a).
    pub supported_schema_versions: Vec<u32>,
    pub _phantom: PhantomData<T>,
}

pub type ScopedVersionedRegistry<T> = TypedVersionedRegistry<T>;

impl<T> TypedVersionedRegistry<T> {
    #[must_use]
    pub fn new(
        store: Arc<dyn VersionedRegistryStore>,
        scope_id: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            store,
            scope_id: scope_id.into(),
            kind: kind.into(),
            supported_schema_versions: Vec::new(),
            _phantom: PhantomData,
        }
    }

    pub fn try_new(
        store: Arc<dyn VersionedRegistryStore>,
        scope_id: impl Into<String>,
        kind: impl Into<String>,
    ) -> Result<Self, ScopeError> {
        let scope_id = ScopeId::new(scope_id.into())?;
        Ok(Self::new_scoped(store, scope_id, kind))
    }

    pub fn new_scoped(
        store: Arc<dyn VersionedRegistryStore>,
        scope_id: ScopeId,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            store,
            scope_id: scope_id.into(),
            kind: kind.into(),
            supported_schema_versions: Vec::new(),
            _phantom: PhantomData,
        }
    }

    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }

    /// Declare which `value_schema_version`s this wrapper can decode.
    /// Reads of records with an unsupported version surface
    /// `IncompatibleSchema` instead of silently returning a stale
    /// deserialization (ADR-0035 D2a).
    #[must_use]
    pub fn with_supported_schema_versions(
        mut self,
        versions: impl IntoIterator<Item = u32>,
    ) -> Self {
        self.supported_schema_versions = versions.into_iter().collect();
        self
    }

    fn check_schema_version(
        &self,
        record: &VersionedRecord<Value>,
    ) -> Result<(), VersionedRegistryError> {
        if self.supported_schema_versions.is_empty()
            || self
                .supported_schema_versions
                .contains(&record.value_schema_version)
        {
            return Ok(());
        }
        Err(VersionedRegistryError::IncompatibleSchema {
            kind: record.kind.clone(),
            id: record.id.clone(),
            version: record.version,
            stored: record.value_schema_version,
            supported: self.supported_schema_versions.clone(),
        })
    }

    #[must_use]
    pub fn version_ref(&self, id: impl Into<String>, version: u64) -> VersionRef {
        VersionRef {
            kind: self.kind.clone(),
            id: id.into(),
            version,
        }
    }
}

impl<T> TypedVersionedRegistry<T>
where
    T: DeserializeOwned,
{
    pub async fn current(
        &self,
        id: &str,
    ) -> Result<Option<VersionedRecord<T>>, VersionedRegistryError> {
        self.store
            .current(&self.scope_id, &self.kind, id)
            .await?
            .map(|record| {
                self.check_schema_version(&record)?;
                decode_record(record)
            })
            .transpose()
    }

    pub async fn get(
        &self,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<T>>, VersionedRegistryError> {
        self.store
            .get(&self.scope_id, &self.kind, id, version)
            .await?
            .map(|record| {
                self.check_schema_version(&record)?;
                decode_record(record)
            })
            .transpose()
    }

    pub async fn list_versions(
        &self,
        id: &str,
    ) -> Result<Vec<VersionedRecord<T>>, VersionedRegistryError> {
        self.store
            .list_versions(&self.scope_id, &self.kind, id)
            .await?
            .into_iter()
            .map(|record| {
                self.check_schema_version(&record)?;
                decode_record(record)
            })
            .collect()
    }

    pub async fn rollback(
        &self,
        id: &str,
        to_version: u64,
        metadata: Value,
    ) -> Result<VersionedRecord<T>, VersionedRegistryError> {
        let record = self
            .store
            .rollback_resource(&self.scope_id, &self.kind, id, to_version, metadata)
            .await?;
        self.check_schema_version(&record)?;
        decode_record(record)
    }
}

impl<T> TypedVersionedRegistry<T>
where
    T: Serialize + DeserializeOwned,
{
    pub async fn publish(
        &self,
        id: &str,
        value: T,
        value_schema_version: u32,
        metadata: Value,
    ) -> Result<PublishOutcome<T>, VersionedRegistryError> {
        if !self.supported_schema_versions.is_empty()
            && !self
                .supported_schema_versions
                .contains(&value_schema_version)
        {
            return Err(VersionedRegistryError::IncompatibleSchema {
                kind: self.kind.clone(),
                id: id.to_string(),
                version: 0,
                stored: value_schema_version,
                supported: self.supported_schema_versions.clone(),
            });
        }
        let value = serde_json::to_value(value)
            .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))?;
        let outcome = self
            .store
            .publish_resource(
                &self.scope_id,
                &self.kind,
                id,
                value,
                value_schema_version,
                metadata,
            )
            .await?;
        decode_publish_outcome(outcome)
    }
}

impl<T> TypedVersionedRegistry<T> {
    pub async fn resource_state(
        &self,
        id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError> {
        self.store
            .resource_state(&self.scope_id, &self.kind, id)
            .await
    }

    pub async fn archive(&self, id: &str) -> Result<(), VersionedRegistryError> {
        self.store
            .archive_resource(&self.scope_id, &self.kind, id)
            .await
    }

    pub async fn unarchive(&self, id: &str) -> Result<(), VersionedRegistryError> {
        self.store
            .unarchive_resource(&self.scope_id, &self.kind, id)
            .await
    }
}

fn decode_publish_outcome<T>(
    outcome: PublishOutcome<Value>,
) -> Result<PublishOutcome<T>, VersionedRegistryError>
where
    T: DeserializeOwned,
{
    match outcome {
        PublishOutcome::Created(record) => Ok(PublishOutcome::Created(decode_record(record)?)),
        PublishOutcome::Noop(record) => Ok(PublishOutcome::Noop(decode_record(record)?)),
    }
}

fn decode_record<T>(
    record: VersionedRecord<Value>,
) -> Result<VersionedRecord<T>, VersionedRegistryError>
where
    T: DeserializeOwned,
{
    let value = serde_json::from_value(record.value)
        .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))?;
    Ok(VersionedRecord {
        kind: record.kind,
        id: record.id,
        version: record.version,
        content_hash: record.content_hash,
        value_schema_version: record.value_schema_version,
        value,
        canonical_json_bytes: record.canonical_json_bytes,
        created_at_ms: record.created_at_ms,
        metadata: record.metadata,
    })
}

#[cfg(test)]
mod tests;
