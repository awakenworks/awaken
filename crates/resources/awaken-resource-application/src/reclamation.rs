//! Resources-owned lifecycle predicates and idempotent physical cleanup.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_resource_contract::{
    FileStore, MemoryRepository, ResourceCatalog, ResourceKind, ResourcePhysicalReclaimer,
    ResourcePurgeError, ResourcePurgeEvidence, ResourcePurgeGuard, ResourceReference,
    ResourceReferenceKind, ResourceState, ResourceTarget, SkillStore,
};

pub struct ResourceLifecycleGuard {
    catalog: Arc<dyn ResourceCatalog>,
}

impl ResourceLifecycleGuard {
    #[must_use]
    pub fn new(catalog: Arc<dyn ResourceCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl ResourcePurgeGuard for ResourceLifecycleGuard {
    async fn blockers(
        &self,
        target: &ResourceTarget,
        config_version: Option<u64>,
        _now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let reference_id = match target.kind {
            ResourceKind::File => None,
            ResourceKind::MemoryStore => match self
                .catalog
                .memory_store(&target.workspace_id, &target.resource_id)
                .map_err(storage)?
            {
                Some(definition)
                    if definition.state == ResourceState::Deleted
                        && config_version == Some(definition.current_config_version.0) =>
                {
                    None
                }
                Some(definition) => Some(format!(
                    "memory:{:?}:config:{}",
                    definition.state, definition.current_config_version.0
                )),
                None => None,
            },
            ResourceKind::Repository => self
                .catalog
                .repository(&target.workspace_id, &target.resource_id)
                .map_err(storage)?
                .filter(|definition| definition.state != ResourceState::Deleted)
                .map(|definition| format!("repository:{:?}", definition.state)),
            // Active Skill lifecycle is projected into ResourceReferenceIndex by
            // the canonical SkillStore decorator and participates in the atomic
            // reclamation fence; no check-then-delete scan remains here.
            ResourceKind::Skill => None,
        };
        Ok(reference_id
            .into_iter()
            .map(|reference_id| ResourceReference {
                kind: ResourceReferenceKind::LogicalLifecycle,
                reference_id,
            })
            .collect())
    }
}

pub struct ResourcePhysicalCleanup {
    files: Arc<dyn FileStore>,
    memories: Arc<dyn MemoryRepository>,
    skills: Arc<dyn SkillStore>,
}

impl ResourcePhysicalCleanup {
    #[must_use]
    pub fn new(
        files: Arc<dyn FileStore>,
        memories: Arc<dyn MemoryRepository>,
        skills: Arc<dyn SkillStore>,
    ) -> Self {
        Self {
            files,
            memories,
            skills,
        }
    }
}

#[async_trait]
impl ResourcePhysicalReclaimer for ResourcePhysicalCleanup {
    async fn purge(
        &self,
        target: &ResourceTarget,
        _config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
        match target.kind {
            ResourceKind::File => Ok(ResourcePurgeEvidence::File {
                blob_deleted: self
                    .files
                    .delete(&target.resource_id)
                    .await
                    .map_err(storage)?,
            }),
            ResourceKind::Repository => Ok(ResourcePurgeEvidence::Repository {
                local_realizations_deleted: 0,
            }),
            ResourceKind::MemoryStore => {
                let summary = self
                    .memories
                    .purge_store(&target.resource_id)
                    .await
                    .map_err(storage)?;
                Ok(ResourcePurgeEvidence::MemoryStore {
                    heads_deleted: summary.heads_deleted,
                    versions_deleted: summary.versions_deleted,
                })
            }
            ResourceKind::Skill => Ok(ResourcePurgeEvidence::Skill {
                versions_deleted: self
                    .skills
                    .purge_skill(&target.workspace_id, &target.resource_id)
                    .await
                    .map_err(storage)?,
            }),
        }
    }
}

fn storage(error: impl std::fmt::Display) -> ResourcePurgeError {
    ResourcePurgeError::Storage(error.to_string())
}
