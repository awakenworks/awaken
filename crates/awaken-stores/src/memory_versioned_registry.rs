//! In-memory implementation of the published versioned registry store.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_contract::contract::versioned_registry::{
    ConfigRevisionRef, PublishOutcome, RegistryPublication, RegistryResourcePublish,
    RegistryRetentionPolicy, VersionRef, VersionedRecord, VersionedRegistryError,
    VersionedRegistryRetention, VersionedRegistryStore, VersionedResourceState,
    build_rollback_metadata, registry_content_hash, sort_publication_entries,
};
use serde_json::Value;

use crate::current_millis;

type ResourceKey = (String, String, String);

/// In-memory published versioned runtime-config registry.
#[derive(Debug, Clone, Default)]
pub struct InMemoryVersionedRegistryStore {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    resources: HashMap<ResourceKey, VersionedResourceState>,
    versions: HashMap<ResourceKey, Vec<VersionedRecord<Value>>>,
    publications: HashMap<String, BTreeMap<u64, RegistryPublication>>,
}

impl InMemoryVersionedRegistryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl VersionedRegistryStore for InMemoryVersionedRegistryStore {
    async fn resource_state(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError> {
        let inner = self.read_inner()?;
        Ok(inner.resources.get(&key(scope_id, kind, id)).cloned())
    }

    async fn current(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        let inner = self.read_inner()?;
        let resource_key = key(scope_id, kind, id);
        let Some(state) = inner.resources.get(&resource_key) else {
            return Ok(None);
        };
        let Some(current_version) = state.current_version else {
            return Ok(None);
        };
        Ok(inner
            .versions
            .get(&resource_key)
            .and_then(|records| {
                records
                    .iter()
                    .find(|record| record.version == current_version)
            })
            .cloned())
    }

    async fn get(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        let inner = self.read_inner()?;
        Ok(inner
            .versions
            .get(&key(scope_id, kind, id))
            .and_then(|records| records.iter().find(|record| record.version == version))
            .cloned())
    }

    async fn list_versions(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Vec<VersionedRecord<Value>>, VersionedRegistryError> {
        let inner = self.read_inner()?;
        Ok(inner
            .versions
            .get(&key(scope_id, kind, id))
            .cloned()
            .unwrap_or_default())
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
        let (content_hash, canonical_json_bytes) =
            registry_content_hash(value_schema_version, &value)?;
        let now = current_millis();
        let resource_key = key(scope_id, kind, id);
        let mut inner = self.write_inner()?;

        if let Some(state) = inner.resources.get(&resource_key) {
            state.ensure_not_archived(kind, id)?;
        }

        if let Some(current) = current_record(&inner, &resource_key)
            && current.content_hash == content_hash
        {
            return Ok(PublishOutcome::Noop(current));
        }

        let version = inner
            .versions
            .get(&resource_key)
            .and_then(|records| records.last())
            .map_or(1, |record| record.version + 1);
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
        inner
            .versions
            .entry(resource_key.clone())
            .or_default()
            .push(record.clone());
        upsert_resource_state(&mut inner, scope_id, kind, id, version, now, metadata);
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
        let prior = self
            .get(scope_id, kind, id, to_version)
            .await?
            .ok_or_else(|| VersionedRegistryError::NotFound(version_name(kind, id, to_version)))?;
        let metadata = build_rollback_metadata(metadata, to_version)?;
        let (content_hash, canonical_json_bytes) =
            registry_content_hash(prior.value_schema_version, &prior.value)?;
        let now = current_millis();
        let resource_key = key(scope_id, kind, id);
        let mut inner = self.write_inner()?;
        if let Some(state) = inner.resources.get(&resource_key) {
            state.ensure_not_archived(kind, id)?;
        }
        let version = inner
            .versions
            .get(&resource_key)
            .and_then(|records| records.last())
            .map_or(1, |record| record.version + 1);
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
        inner
            .versions
            .entry(resource_key.clone())
            .or_default()
            .push(record.clone());
        upsert_resource_state(&mut inner, scope_id, kind, id, version, now, metadata);
        Ok(record)
    }

    async fn archive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError> {
        let mut inner = self.write_inner()?;
        let state = inner
            .resources
            .get_mut(&key(scope_id, kind, id))
            .ok_or_else(|| VersionedRegistryError::NotFound(resource_name(kind, id)))?;
        if state.archived_at_ms.is_none() {
            let now = current_millis();
            state.archived_at_ms = Some(now);
            state.updated_at_ms = now;
        }
        Ok(())
    }

    async fn unarchive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError> {
        let mut inner = self.write_inner()?;
        let state = inner
            .resources
            .get_mut(&key(scope_id, kind, id))
            .ok_or_else(|| VersionedRegistryError::NotFound(resource_name(kind, id)))?;
        if state.archived_at_ms.take().is_some() {
            state.updated_at_ms = current_millis();
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
        validate_resource_publication_request(publication_id, &resources)?;
        let prepared = resources
            .into_iter()
            .map(|resource| {
                let (content_hash, canonical_json_bytes) =
                    registry_content_hash(resource.value_schema_version, &resource.value)?;
                Ok((resource, content_hash, canonical_json_bytes))
            })
            .collect::<Result<Vec<_>, VersionedRegistryError>>()?;
        let now = current_millis();
        let mut inner = self.write_inner()?;
        if inner
            .publications
            .get(scope_id)
            .into_iter()
            .flat_map(|publications| publications.values())
            .any(|publication| publication.publication_id == publication_id)
        {
            return Err(VersionedRegistryError::AlreadyExists(format!(
                "publication/{publication_id}"
            )));
        }
        for (resource, _, _) in &prepared {
            if let Some(state) = inner
                .resources
                .get(&key(scope_id, &resource.kind, &resource.id))
                && state.archived_at_ms.is_some()
            {
                return Err(VersionedRegistryError::Archived {
                    kind: resource.kind.clone(),
                    id: resource.id.clone(),
                });
            }
        }

        let mut entries = Vec::with_capacity(prepared.len());
        for (resource, content_hash, canonical_json_bytes) in prepared {
            let resource_key = key(scope_id, &resource.kind, &resource.id);
            let record = if let Some(current) = current_record(&inner, &resource_key)
                && current.content_hash == content_hash
            {
                current
            } else {
                let version = inner
                    .versions
                    .get(&resource_key)
                    .and_then(|records| records.last())
                    .map_or(1, |record| record.version + 1);
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
                inner
                    .versions
                    .entry(resource_key.clone())
                    .or_default()
                    .push(record.clone());
                upsert_resource_state(
                    &mut inner,
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

        let publications = inner.publications.entry(scope_id.to_string()).or_default();
        let snapshot_version = publications
            .last_key_value()
            .map_or(1, |(snapshot_version, _)| snapshot_version + 1);
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
        publications.insert(snapshot_version, publication.clone());
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
        validate_publication_request(publication_id, &entries)?;
        let now = current_millis();
        let mut inner = self.write_inner()?;
        if inner
            .publications
            .get(scope_id)
            .into_iter()
            .flat_map(|publications| publications.values())
            .any(|publication| publication.publication_id == publication_id)
        {
            return Err(VersionedRegistryError::AlreadyExists(format!(
                "publication/{publication_id}"
            )));
        }
        validate_publication_entries(&inner, scope_id, &entries)?;
        let publications = inner.publications.entry(scope_id.to_string()).or_default();
        let snapshot_version = publications
            .last_key_value()
            .map_or(1, |(snapshot_version, _)| snapshot_version + 1);
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
        publications.insert(snapshot_version, publication.clone());
        Ok(publication)
    }

    async fn latest_publication(
        &self,
        scope_id: &str,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        let inner = self.read_inner()?;
        Ok(inner
            .publications
            .get(scope_id)
            .and_then(|publications| publications.last_key_value())
            .map(|(_, publication)| publication.clone()))
    }

    async fn get_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        let inner = self.read_inner()?;
        Ok(inner
            .publications
            .get(scope_id)
            .and_then(|publications| publications.get(&snapshot_version))
            .cloned())
    }
}

#[async_trait]
impl VersionedRegistryRetention for InMemoryVersionedRegistryStore {
    async fn purge_eligible_versions(
        &self,
        scope_id: &str,
        now_ms: u64,
        policy: RegistryRetentionPolicy,
        dry_run: bool,
    ) -> Result<Vec<VersionRef>, VersionedRegistryError> {
        // Snapshot the protection set + planning result under a read lock
        // so concurrent publishes cannot race a purge into deleting a
        // version that was about to be referenced. Mutation (when
        // `dry_run = false`) takes a second write lock and re-checks
        // membership before delete.
        let mut protected = HashSet::new();
        for version in &policy.protected_versions {
            protected.insert((version.kind.clone(), version.id.clone(), version.version));
        }

        let plan: Vec<VersionRef> = {
            let inner = self.read_inner()?;
            if let Some(publications) = inner.publications.get(scope_id) {
                for publication in publications.values() {
                    for entry in &publication.entries {
                        protected.insert((entry.kind.clone(), entry.id.clone(), entry.version));
                    }
                }
            }
            let mut eligible = Vec::new();
            for ((resource_scope, kind, id), records) in &inner.versions {
                if resource_scope != scope_id {
                    continue;
                }
                let current_version = inner
                    .resources
                    .get(&(resource_scope.clone(), kind.clone(), id.clone()))
                    .and_then(|state| state.current_version);
                let mut sorted: Vec<&VersionedRecord<Value>> = records.iter().collect();
                sorted.sort_by_key(|record| std::cmp::Reverse(record.version));
                let keep_last = policy.keep_last_versions.unwrap_or(0);
                let mut historical_kept: u64 = 0;
                for record in sorted {
                    if Some(record.version) == current_version {
                        continue;
                    }
                    if protected.contains(&(kind.clone(), id.clone(), record.version)) {
                        continue;
                    }
                    if let Some(window) = policy.keep_younger_than_ms
                        && now_ms.saturating_sub(record.created_at_ms) < window
                    {
                        continue;
                    }
                    if historical_kept < keep_last {
                        historical_kept += 1;
                        continue;
                    }
                    eligible.push(VersionRef {
                        kind: kind.clone(),
                        id: id.clone(),
                        version: record.version,
                    });
                }
            }
            eligible.sort_by(|a, b| {
                a.kind
                    .cmp(&b.kind)
                    .then_with(|| a.id.cmp(&b.id))
                    .then_with(|| a.version.cmp(&b.version))
            });
            eligible
        };

        if !dry_run && !plan.is_empty() {
            let mut inner = self.write_inner()?;
            for version in &plan {
                let resource_key = (
                    scope_id.to_string(),
                    version.kind.clone(),
                    version.id.clone(),
                );
                if let Some(records) = inner.versions.get_mut(&resource_key) {
                    records.retain(|record| record.version != version.version);
                }
            }
        }
        Ok(plan)
    }
}

impl InMemoryVersionedRegistryStore {
    fn read_inner(&self) -> Result<std::sync::RwLockReadGuard<'_, Inner>, VersionedRegistryError> {
        self.inner
            .read()
            .map_err(|error| VersionedRegistryError::Backend(error.to_string()))
    }

    fn write_inner(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, Inner>, VersionedRegistryError> {
        self.inner
            .write()
            .map_err(|error| VersionedRegistryError::Backend(error.to_string()))
    }
}

fn validate_publication_request(
    publication_id: &str,
    entries: &[VersionRef],
) -> Result<(), VersionedRegistryError> {
    if publication_id.trim().is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication_id cannot be empty".to_string(),
        ));
    }
    if entries.is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication entries cannot be empty".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for entry in entries {
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
    publication_id: &str,
    resources: &[RegistryResourcePublish],
) -> Result<(), VersionedRegistryError> {
    if publication_id.trim().is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication_id cannot be empty".to_string(),
        ));
    }
    if resources.is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication resources cannot be empty".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for resource in resources {
        if resource.kind.trim().is_empty() || resource.id.trim().is_empty() {
            return Err(VersionedRegistryError::InvalidRequest(
                "publication resource kind and id cannot be empty".to_string(),
            ));
        }
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
    inner: &Inner,
    scope_id: &str,
    entries: &[VersionRef],
) -> Result<(), VersionedRegistryError> {
    for entry in entries {
        let resource_key = key(scope_id, &entry.kind, &entry.id);
        let state = inner.resources.get(&resource_key).ok_or_else(|| {
            VersionedRegistryError::NotFound(resource_name(&entry.kind, &entry.id))
        })?;
        if state.archived_at_ms.is_some() {
            return Err(VersionedRegistryError::Archived {
                kind: entry.kind.clone(),
                id: entry.id.clone(),
            });
        }
        let exists = inner
            .versions
            .get(&resource_key)
            .is_some_and(|records| records.iter().any(|record| record.version == entry.version));
        if !exists {
            return Err(VersionedRegistryError::NotFound(version_name(
                &entry.kind,
                &entry.id,
                entry.version,
            )));
        }
    }
    Ok(())
}

fn current_record(inner: &Inner, key: &ResourceKey) -> Option<VersionedRecord<Value>> {
    let current_version = inner.resources.get(key)?.current_version?;
    inner
        .versions
        .get(key)?
        .iter()
        .find(|record| record.version == current_version)
        .cloned()
}

fn upsert_resource_state(
    inner: &mut Inner,
    scope_id: &str,
    kind: &str,
    id: &str,
    version: u64,
    now: u64,
    metadata: Value,
) {
    let resource_key = key(scope_id, kind, id);
    inner
        .resources
        .entry(resource_key)
        .and_modify(|state| {
            state.current_version = Some(version);
            state.updated_at_ms = now;
            state.metadata = metadata.clone();
        })
        .or_insert_with(|| VersionedResourceState {
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

fn key(scope_id: &str, kind: &str, id: &str) -> ResourceKey {
    (scope_id.to_string(), kind.to_string(), id.to_string())
}

fn resource_name(kind: &str, id: &str) -> String {
    format!("{kind}/{id}")
}

fn version_name(kind: &str, id: &str, version: u64) -> String {
    format!("{kind}/{id}@{version}")
}
