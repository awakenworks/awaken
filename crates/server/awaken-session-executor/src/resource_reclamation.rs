//! Resource-specific safety/physical adapters for the generic reclaimer.
//!
//! The adapter reads only intrinsic catalog/reference state. Authorization has
//! already ended at the API PEP before a tombstone and purge intent are created.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_protocol_managed::resource_plane::{
    ResourceCatalog, ResourceKind, ResourcePhysicalReclaimer, ResourcePurgeError,
    ResourcePurgeEvidence, ResourcePurgeGuard, ResourceReference, ResourceReferenceKind,
    ResourceState, ResourceTarget,
};

use crate::SharedHost;

pub struct HostResourceReclamation {
    host: Arc<SharedHost>,
    catalog: Arc<dyn ResourceCatalog>,
}

impl HostResourceReclamation {
    #[must_use]
    pub fn new(host: Arc<SharedHost>, catalog: Arc<dyn ResourceCatalog>) -> Self {
        Self { host, catalog }
    }
}

#[async_trait]
impl ResourcePurgeGuard for HostResourceReclamation {
    async fn blockers(
        &self,
        target: &ResourceTarget,
        config_version: Option<u64>,
        _now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let lifecycle_blocker = match target.kind {
            ResourceKind::File => None,
            ResourceKind::MemoryStore => match self
                .catalog
                .memory_store(&target.workspace_id, &target.resource_id)
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?
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
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?
                .filter(|definition| definition.state != ResourceState::Deleted)
                .map(|definition| format!("repository:{:?}", definition.state)),
            ResourceKind::Skill => match self
                .host
                .skills
                .definition(&target.workspace_id, &target.resource_id)
                .await
            {
                Some(Ok(Some(_))) => Some("skill:active".into()),
                Some(Err(error)) => return Err(ResourcePurgeError::Storage(error.to_string())),
                Some(Ok(None)) | None => None,
            },
        };
        let records = if target.kind == ResourceKind::File {
            self.host
                .resource_lifecycle
                .references_for_resource(target.kind, &target.resource_id)
                .await?
        } else {
            self.host
                .resource_lifecycle
                .references(target)
                .await?
                .into_iter()
                .map(
                    |reference| awaken_protocol_managed::resource_plane::ResourceReferenceRecord {
                        target: target.clone(),
                        reference,
                    },
                )
                .collect()
        };
        let mut blockers: Vec<_> = records.into_iter().map(|record| record.reference).collect();
        if let Some(reference_id) = lifecycle_blocker {
            blockers.push(ResourceReference {
                kind: ResourceReferenceKind::LogicalLifecycle,
                reference_id,
            });
        }
        if let Some(config_service) = &self.host.config_service {
            let agents = match target.kind {
                ResourceKind::File => config_service.agents_referencing_input(
                    &target.workspace_id,
                    &awaken_config_resolver::InputResourceId::File(
                        awaken_config_resolver::FileId::from(target.resource_id.clone()),
                    ),
                ),
                ResourceKind::MemoryStore => config_service.agents_referencing_input(
                    &target.workspace_id,
                    &awaken_config_resolver::InputResourceId::MemoryStore(
                        awaken_config_resolver::MemoryStoreId::from(target.resource_id.clone()),
                    ),
                ),
                ResourceKind::Repository => config_service.agents_referencing_input(
                    &target.workspace_id,
                    &awaken_config_resolver::InputResourceId::Repository(
                        awaken_config_resolver::RepositoryId::from(target.resource_id.clone()),
                    ),
                ),
                ResourceKind::Skill => config_service
                    .agents_referencing_skill(&target.workspace_id, &target.resource_id),
            };
            blockers.extend(agents.into_iter().map(|agent_id| ResourceReference {
                kind: ResourceReferenceKind::AgentBinding,
                reference_id: agent_id,
            }));
        }
        if target.kind == ResourceKind::MemoryStore {
            let extractions = self
                .host
                .memory
                .extraction_repository()
                .recoverable_extractions(usize::MAX)
                .await
                .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
            blockers.extend(
                extractions
                    .into_iter()
                    .filter(|intent| {
                        intent.workspace_id == target.workspace_id
                            && intent.memory_store_id == target.resource_id
                    })
                    .map(|intent| ResourceReference {
                        kind: ResourceReferenceKind::ExtractionIntent,
                        reference_id: intent.intent_id,
                    }),
            );
        }
        Ok(blockers)
    }
}

#[async_trait]
impl ResourcePhysicalReclaimer for HostResourceReclamation {
    async fn purge(
        &self,
        target: &ResourceTarget,
        _config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
        match target.kind {
            ResourceKind::File => {
                // The generic coordinator owns the durable resource-plane fence;
                // this adapter only performs the idempotent physical operation.
                let blob_deleted = self
                    .host
                    .file_store()
                    .delete(&target.resource_id)
                    .await
                    .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
                Ok(ResourcePurgeEvidence::File { blob_deleted })
            }
            ResourceKind::Repository => Ok(ResourcePurgeEvidence::Repository {
                local_realizations_deleted: 0,
            }),
            ResourceKind::MemoryStore => {
                let summary = self
                    .host
                    .memory_stores
                    .fs()
                    .purge_store(&target.resource_id)
                    .await
                    .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
                Ok(ResourcePurgeEvidence::MemoryStore {
                    heads_deleted: summary.heads_deleted,
                    versions_deleted: summary.versions_deleted,
                })
            }
            ResourceKind::Skill => {
                let versions_deleted = self
                    .host
                    .skills
                    .purge(&target.workspace_id, &target.resource_id)
                    .await
                    .ok_or_else(|| {
                        ResourcePurgeError::Storage("Skill repository is not installed".into())
                    })?
                    .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
                Ok(ResourcePurgeEvidence::Skill { versions_deleted })
            }
        }
    }
}
