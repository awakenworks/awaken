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
        _config_version: Option<u64>,
        _now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let lifecycle_blocker = match target.kind {
            ResourceKind::File => None,
            ResourceKind::MemoryStore => self
                .catalog
                .memory_store(&target.workspace_id, &target.resource_id)
                .filter(|definition| definition.state != ResourceState::Deleted)
                .map(|definition| format!("memory:{:?}", definition.state)),
            ResourceKind::Repository => self
                .catalog
                .repository(&target.workspace_id, &target.resource_id)
                .filter(|definition| definition.state != ResourceState::Deleted)
                .map(|definition| format!("repository:{:?}", definition.state)),
            // Skill tombstoning is wired in the next per-kind adapter slice.
            ResourceKind::Skill => Some("skill lifecycle is not tombstoned".into()),
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
                // Serialize grant creation with the final reference recheck and
                // blob delete. The durable local adapter is single-host SQLite;
                // distributed adapters provide the equivalent transaction/lock.
                let _guard = self.host.resource_lifecycle_gate.lock().await;
                if !self
                    .host
                    .resource_lifecycle
                    .references_for_resource(ResourceKind::File, &target.resource_id)
                    .await?
                    .is_empty()
                {
                    return Err(ResourcePurgeError::Storage(
                        "File became referenced during reclamation".into(),
                    ));
                }
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
            ResourceKind::MemoryStore | ResourceKind::Skill => Err(ResourcePurgeError::Storage(
                "physical adapter is not installed for this resource kind".into(),
            )),
        }
    }
}
