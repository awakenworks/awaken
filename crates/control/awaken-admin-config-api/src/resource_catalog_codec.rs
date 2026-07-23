//! Storage codec shared by Resource Catalog adapters.
//!
//! SQL syntax and transaction mechanics remain adapter-specific. Aggregate JSON
//! shape and the retired MemoryStore-row transition live here once.

use std::collections::BTreeMap;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition, ResourceState, ResourceTimestamps,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryRecord {
    pub definition: MemoryStoreDefinition,
    pub configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepositoryRecord {
    pub definition: RepositoryDefinition,
    pub configs: BTreeMap<ConfigVersion, RepositoryConfigVersion>,
}

/// Upgrade-only shape written by the removed `MemoryStoreRegistry`.
#[derive(Debug, Deserialize)]
pub(crate) struct LegacyMemoryStoreDef {
    pub id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub archived: bool,
}

impl LegacyMemoryStoreDef {
    /// Empty-owner rows stay quarantined: a migration cannot invent ownership.
    pub(crate) fn into_catalog_records(
        self,
    ) -> Option<(MemoryStoreDefinition, MemoryStoreConfigVersion)> {
        if self.workspace_id.trim().is_empty() {
            return None;
        }
        let id = self.id;
        let state = if self.archived {
            ResourceState::Archived
        } else {
            ResourceState::Active
        };
        let migrated_at = now_nanos();
        let mut timestamps = ResourceTimestamps::created(migrated_at);
        if state == ResourceState::Archived {
            timestamps.transition_to(state, migrated_at);
        }
        Some((
            MemoryStoreDefinition {
                id: id.clone().into(),
                workspace_id: self.workspace_id,
                name: self.name,
                description: self.description,
                metadata: self.metadata,
                state,
                current_config_version: ConfigVersion::INITIAL,
                timestamps,
            },
            MemoryStoreConfigVersion {
                memory_store_id: id.into(),
                version: ConfigVersion::INITIAL,
                recall_policy: Default::default(),
                extraction_policy: Default::default(),
                retention_policy: Default::default(),
            },
        ))
    }
}

pub(crate) fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_archive_transition_records_real_aggregate_time() {
        let (definition, _) = LegacyMemoryStoreDef {
            id: "memory-1".into(),
            workspace_id: "workspace-a".into(),
            name: "Legacy".into(),
            description: String::new(),
            metadata: BTreeMap::new(),
            archived: true,
        }
        .into_catalog_records()
        .expect("owned row is migratable");

        assert_eq!(definition.state, ResourceState::Archived);
        assert!(definition.timestamps.created_unix_nanos > 0);
        assert_eq!(
            definition.timestamps.archived_unix_nanos,
            Some(definition.timestamps.created_unix_nanos)
        );
        assert_eq!(
            definition.timestamps.updated_unix_nanos,
            definition.timestamps.created_unix_nanos
        );
    }
}
