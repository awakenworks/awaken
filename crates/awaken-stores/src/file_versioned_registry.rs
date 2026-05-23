//! File-system implementation of the published versioned registry store.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::contract::versioned_registry::{
    ConfigRevisionRef, PublishOutcome, RegistryPublication, RegistryResourcePublish, VersionRef,
    VersionedRecord, VersionedRegistryError, VersionedRegistryStore, VersionedResourceState,
    build_rollback_metadata, registry_content_hash, sort_publication_entries,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::current_millis;
use crate::file::{atomic_write, read_json, validate_id};

/// File-system published versioned runtime-config registry.
pub struct FileVersionedRegistryStore {
    base_path: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileRegistryState {
    #[serde(default)]
    resources: Vec<VersionedResourceState>,
    #[serde(default)]
    versions: Vec<FileVersionedRecord>,
    #[serde(default)]
    publications: Vec<RegistryPublication>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileVersionedRecord {
    scope_id: String,
    record: VersionedRecord<Value>,
}

impl FileVersionedRegistryStore {
    /// Create a new file registry store rooted at `base_path`.
    #[must_use]
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        let base_path = base_path.into();
        Self {
            lock: shared_registry_lock(&base_path),
            base_path,
        }
    }

    fn registry_dir(&self) -> PathBuf {
        self.base_path.join("versioned_registry")
    }

    fn state_path(&self) -> PathBuf {
        self.registry_dir().join("state.json")
    }

    async fn load_state(&self) -> Result<FileRegistryState, VersionedRegistryError> {
        read_json(&self.state_path())
            .await
            .map_err(from_storage_error)
            .map(|state| state.unwrap_or_default())
    }

    async fn save_state(&self, state: &FileRegistryState) -> Result<(), VersionedRegistryError> {
        let payload = serde_json::to_string_pretty(state)
            .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))?;
        atomic_write(&self.registry_dir(), "state.json", &payload)
            .await
            .map_err(from_storage_error)
    }
}

#[async_trait]
impl VersionedRegistryStore for FileVersionedRegistryStore {
    async fn resource_state(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let state = self.load_state().await?;
        Ok(find_resource(&state, scope_id, kind, id).cloned())
    }

    async fn current(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let state = self.load_state().await?;
        Ok(current_record(&state, scope_id, kind, id))
    }

    async fn get(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let state = self.load_state().await?;
        Ok(find_version(&state, scope_id, kind, id, version).cloned())
    }

    async fn list_versions(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Vec<VersionedRecord<Value>>, VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let state = self.load_state().await?;
        let mut records: Vec<_> = state
            .versions
            .into_iter()
            .filter(|entry| entry.scope_id == scope_id)
            .map(|entry| entry.record)
            .filter(|record| record.kind == kind && record.id == id)
            .collect();
        records.sort_by_key(|record| record.version);
        Ok(records)
    }

    async fn publish_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        value: Value,
        value_schema_version: u32,
        metadata: Value,
    ) -> Result<PublishOutcome<Value>, VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let (content_hash, canonical_json_bytes) =
            registry_content_hash(value_schema_version, &value)?;
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        if let Some(resource) = find_resource(&state, scope_id, kind, id) {
            resource.ensure_not_archived(kind, id)?;
        }
        if let Some(current) = current_record(&state, scope_id, kind, id)
            && current.content_hash == content_hash
        {
            return Ok(PublishOutcome::Noop(current));
        }

        let now = current_millis();
        let version = next_version(&state, scope_id, kind, id);
        let record = VersionedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            version,
            content_hash,
            value_schema_version,
            value,
            canonical_json_bytes,
            created_at_ms: now,
            metadata: metadata.clone(),
        };
        state.versions.push(FileVersionedRecord {
            scope_id: scope_id.to_string(),
            record: record.clone(),
        });
        upsert_resource(&mut state, scope_id, kind, id, version, now, metadata);
        self.save_state(&state).await?;
        Ok(PublishOutcome::Created(record))
    }

    async fn rollback_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        to_version: u64,
        metadata: Value,
    ) -> Result<VersionedRecord<Value>, VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        if let Some(resource) = find_resource(&state, scope_id, kind, id) {
            resource.ensure_not_archived(kind, id)?;
        }
        let prior = find_version(&state, scope_id, kind, id, to_version)
            .cloned()
            .ok_or_else(|| VersionedRegistryError::NotFound(version_name(kind, id, to_version)))?;
        let metadata = build_rollback_metadata(metadata, to_version)?;
        let (content_hash, canonical_json_bytes) =
            registry_content_hash(prior.value_schema_version, &prior.value)?;
        let now = current_millis();
        let version = next_version(&state, scope_id, kind, id);
        let record = VersionedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            version,
            content_hash,
            value_schema_version: prior.value_schema_version,
            value: prior.value,
            canonical_json_bytes,
            created_at_ms: now,
            metadata: metadata.clone(),
        };
        state.versions.push(FileVersionedRecord {
            scope_id: scope_id.to_string(),
            record: record.clone(),
        });
        upsert_resource(&mut state, scope_id, kind, id, version, now, metadata);
        self.save_state(&state).await?;
        Ok(record)
    }

    async fn archive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        let resource = find_resource_mut(&mut state, scope_id, kind, id)
            .ok_or_else(|| VersionedRegistryError::NotFound(resource_name(kind, id)))?;
        if resource.archived_at_ms.is_none() {
            let now = current_millis();
            resource.archived_at_ms = Some(now);
            resource.updated_at_ms = now;
            self.save_state(&state).await?;
        }
        Ok(())
    }

    async fn unarchive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError> {
        validate_resource_identity(scope_id, kind, id)?;
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        let resource = find_resource_mut(&mut state, scope_id, kind, id)
            .ok_or_else(|| VersionedRegistryError::NotFound(resource_name(kind, id)))?;
        if resource.archived_at_ms.take().is_some() {
            resource.updated_at_ms = current_millis();
            self.save_state(&state).await?;
        }
        Ok(())
    }

    async fn publish_resources_and_create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        resources: Vec<RegistryResourcePublish>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        validate_id(scope_id, "registry scope id").map_err(from_storage_error)?;
        validate_id(publication_id, "registry publication id").map_err(from_storage_error)?;
        validate_resource_publication_request(scope_id, &resources)?;
        let prepared = resources
            .into_iter()
            .map(|resource| {
                let (content_hash, canonical_json_bytes) =
                    registry_content_hash(resource.value_schema_version, &resource.value)?;
                Ok((resource, content_hash, canonical_json_bytes))
            })
            .collect::<Result<Vec<_>, VersionedRegistryError>>()?;
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        if state.publications.iter().any(|publication| {
            publication.scope_id == scope_id && publication.publication_id == publication_id
        }) {
            return Err(VersionedRegistryError::AlreadyExists(format!(
                "publication/{publication_id}"
            )));
        }
        for (resource, _, _) in &prepared {
            if let Some(existing) = find_resource(&state, scope_id, &resource.kind, &resource.id) {
                existing.ensure_not_archived(&resource.kind, &resource.id)?;
            }
        }

        let now = current_millis();
        let mut entries = Vec::with_capacity(prepared.len());
        for (resource, content_hash, canonical_json_bytes) in prepared {
            let record = if let Some(current) =
                current_record(&state, scope_id, &resource.kind, &resource.id)
                && current.content_hash == content_hash
            {
                current
            } else {
                let version = next_version(&state, scope_id, &resource.kind, &resource.id);
                let record = VersionedRecord {
                    kind: resource.kind.clone(),
                    id: resource.id.clone(),
                    version,
                    content_hash,
                    value_schema_version: resource.value_schema_version,
                    value: resource.value,
                    canonical_json_bytes,
                    created_at_ms: now,
                    metadata: resource.metadata.clone(),
                };
                state.versions.push(FileVersionedRecord {
                    scope_id: scope_id.to_string(),
                    record: record.clone(),
                });
                upsert_resource(
                    &mut state,
                    scope_id,
                    &resource.kind,
                    &resource.id,
                    version,
                    now,
                    resource.metadata,
                );
                record
            };
            entries.push(VersionRef {
                kind: record.kind,
                id: record.id,
                version: record.version,
            });
        }

        let snapshot_version = state
            .publications
            .iter()
            .filter(|publication| publication.scope_id == scope_id)
            .map(|publication| publication.snapshot_version)
            .max()
            .unwrap_or(0)
            + 1;
        let publication = RegistryPublication {
            publication_id: publication_id.to_string(),
            scope_id: scope_id.to_string(),
            snapshot_version,
            entries: sort_publication_entries(entries),
            source_config_revisions,
            created_by,
            created_at_ms: now,
            metadata,
        };
        state.publications.push(publication.clone());
        self.save_state(&state).await?;
        Ok(publication)
    }

    async fn create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        entries: Vec<VersionRef>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        validate_id(scope_id, "registry scope id").map_err(from_storage_error)?;
        validate_id(publication_id, "registry publication id").map_err(from_storage_error)?;
        validate_publication_request(&entries)?;
        let _guard = self.lock.lock().await;
        let mut state = self.load_state().await?;
        if state.publications.iter().any(|publication| {
            publication.scope_id == scope_id && publication.publication_id == publication_id
        }) {
            return Err(VersionedRegistryError::AlreadyExists(format!(
                "publication/{publication_id}"
            )));
        }
        validate_publication_entries(&state, scope_id, &entries)?;
        let snapshot_version = state
            .publications
            .iter()
            .filter(|publication| publication.scope_id == scope_id)
            .map(|publication| publication.snapshot_version)
            .max()
            .unwrap_or(0)
            + 1;
        let publication = RegistryPublication {
            publication_id: publication_id.to_string(),
            scope_id: scope_id.to_string(),
            snapshot_version,
            entries: sort_publication_entries(entries),
            source_config_revisions,
            created_by,
            created_at_ms: current_millis(),
            metadata,
        };
        state.publications.push(publication.clone());
        self.save_state(&state).await?;
        Ok(publication)
    }

    async fn latest_publication(
        &self,
        scope_id: &str,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        validate_id(scope_id, "registry scope id").map_err(from_storage_error)?;
        let _guard = self.lock.lock().await;
        let state = self.load_state().await?;
        Ok(state
            .publications
            .into_iter()
            .filter(|publication| publication.scope_id == scope_id)
            .max_by_key(|publication| publication.snapshot_version))
    }

    async fn get_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        validate_id(scope_id, "registry scope id").map_err(from_storage_error)?;
        let _guard = self.lock.lock().await;
        let state = self.load_state().await?;
        Ok(state.publications.into_iter().find(|publication| {
            publication.scope_id == scope_id && publication.snapshot_version == snapshot_version
        }))
    }
}

fn validate_publication_request(entries: &[VersionRef]) -> Result<(), VersionedRegistryError> {
    if entries.is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication entries cannot be empty".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for entry in entries {
        validate_resource_identity("default", &entry.kind, &entry.id)?;
        if entry.version == 0 {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "{} version cannot be 0",
                resource_name(&entry.kind, &entry.id)
            )));
        }
        if !seen.insert((entry.kind.as_str(), entry.id.as_str())) {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "duplicate publication entry {}",
                resource_name(&entry.kind, &entry.id)
            )));
        }
    }
    Ok(())
}

fn validate_resource_publication_request(
    scope_id: &str,
    resources: &[RegistryResourcePublish],
) -> Result<(), VersionedRegistryError> {
    if resources.is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication resources cannot be empty".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for resource in resources {
        validate_resource_identity(scope_id, &resource.kind, &resource.id)?;
        if !seen.insert((resource.kind.as_str(), resource.id.as_str())) {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "duplicate publication resource {}",
                resource_name(&resource.kind, &resource.id)
            )));
        }
    }
    Ok(())
}

fn validate_publication_entries(
    state: &FileRegistryState,
    scope_id: &str,
    entries: &[VersionRef],
) -> Result<(), VersionedRegistryError> {
    for entry in entries {
        let resource = find_resource(state, scope_id, &entry.kind, &entry.id).ok_or_else(|| {
            VersionedRegistryError::NotFound(resource_name(&entry.kind, &entry.id))
        })?;
        resource.ensure_not_archived(&entry.kind, &entry.id)?;
        if find_version(state, scope_id, &entry.kind, &entry.id, entry.version).is_none() {
            return Err(VersionedRegistryError::NotFound(version_name(
                &entry.kind,
                &entry.id,
                entry.version,
            )));
        }
    }
    Ok(())
}

fn validate_resource_identity(
    scope_id: &str,
    kind: &str,
    id: &str,
) -> Result<(), VersionedRegistryError> {
    validate_id(scope_id, "registry scope id").map_err(from_storage_error)?;
    validate_id(kind, "registry kind").map_err(from_storage_error)?;
    validate_id(id, "registry id").map_err(from_storage_error)
}

fn find_resource<'a>(
    state: &'a FileRegistryState,
    scope_id: &str,
    kind: &str,
    id: &str,
) -> Option<&'a VersionedResourceState> {
    state.resources.iter().find(|resource| {
        resource.scope_id == scope_id && resource.kind == kind && resource.id == id
    })
}

fn find_resource_mut<'a>(
    state: &'a mut FileRegistryState,
    scope_id: &str,
    kind: &str,
    id: &str,
) -> Option<&'a mut VersionedResourceState> {
    state.resources.iter_mut().find(|resource| {
        resource.scope_id == scope_id && resource.kind == kind && resource.id == id
    })
}

fn find_version<'a>(
    state: &'a FileRegistryState,
    scope_id: &str,
    kind: &str,
    id: &str,
    version: u64,
) -> Option<&'a VersionedRecord<Value>> {
    find_resource(state, scope_id, kind, id)?;
    state
        .versions
        .iter()
        .find(|entry| {
            entry.scope_id == scope_id
                && entry.record.kind == kind
                && entry.record.id == id
                && entry.record.version == version
        })
        .map(|entry| &entry.record)
}

fn current_record(
    state: &FileRegistryState,
    scope_id: &str,
    kind: &str,
    id: &str,
) -> Option<VersionedRecord<Value>> {
    let current_version = find_resource(state, scope_id, kind, id)?.current_version?;
    find_version(state, scope_id, kind, id, current_version).cloned()
}

fn next_version(state: &FileRegistryState, scope_id: &str, kind: &str, id: &str) -> u64 {
    state
        .versions
        .iter()
        .filter(|entry| entry.scope_id == scope_id)
        .map(|entry| &entry.record)
        .filter(|record| record.kind == kind && record.id == id)
        .map(|record| record.version)
        .max()
        .unwrap_or(0)
        + 1
}

fn upsert_resource(
    state: &mut FileRegistryState,
    scope_id: &str,
    kind: &str,
    id: &str,
    version: u64,
    now: u64,
    metadata: Value,
) {
    if let Some(resource) = find_resource_mut(state, scope_id, kind, id) {
        resource.current_version = Some(version);
        resource.updated_at_ms = now;
        resource.metadata = metadata;
        return;
    }
    state.resources.push(VersionedResourceState {
        scope_id: scope_id.to_string(),
        kind: kind.to_string(),
        id: id.to_string(),
        current_version: Some(version),
        archived_at_ms: None,
        created_at_ms: now,
        updated_at_ms: now,
        metadata,
    });
}

fn shared_registry_lock(base_path: &PathBuf) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("file registry lock map poisoned");
    if let Some(lock) = locks.get(base_path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(base_path.clone(), Arc::downgrade(&lock));
    lock
}

fn from_storage_error(error: StorageError) -> VersionedRegistryError {
    match error {
        StorageError::Serialization(message) => VersionedRegistryError::Serialization(message),
        other => VersionedRegistryError::Backend(other.to_string()),
    }
}

fn resource_name(kind: &str, id: &str) -> String {
    format!("{kind}/{id}")
}

fn version_name(kind: &str, id: &str, version: u64) -> String {
    format!("{kind}/{id}@{version}")
}
