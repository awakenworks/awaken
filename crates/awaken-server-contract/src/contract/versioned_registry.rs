pub use awaken_runtime_contract::contract::versioned_registry::*;

use std::marker::PhantomData;
use std::sync::Arc;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::contract::scope::{ScopeError, ScopeId};

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
