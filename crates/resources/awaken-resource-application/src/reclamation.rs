//! Resources-owned lifecycle predicates and idempotent physical cleanup.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_resource_contract::{
    FileStore, MemoryContentPurge, ResourceKind, ResourcePhysicalReclaimer, ResourcePurgeError,
    ResourcePurgeEvidence, ResourcePurgeGuard, ResourceReference, ResourceReferenceKind,
    ResourceRegistry, ResourceRegistryError, ResourceState, ResourceTarget, SkillStore,
    SkillStoreError,
};

pub struct ResourceLifecycleGuard {
    registry: Arc<dyn ResourceRegistry>,
}

impl ResourceLifecycleGuard {
    #[must_use]
    pub fn new(registry: Arc<dyn ResourceRegistry>) -> Self {
        Self { registry }
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
                .registry
                .find_memory_store(&target.workspace_id, &target.resource_id)
                .await
                .map_err(registry_guard_error)?
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
                .registry
                .find_repository(&target.workspace_id, &target.resource_id)
                .await
                .map_err(registry_guard_error)?
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
    memories: Arc<dyn MemoryContentPurge>,
    skills: Arc<dyn SkillStore>,
}

impl ResourcePhysicalCleanup {
    #[must_use]
    pub fn new(
        files: Arc<dyn FileStore>,
        memories: Arc<dyn MemoryContentPurge>,
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
                    .map_err(ResourcePurgeError::from)?,
            }),
            ResourceKind::Repository => Ok(ResourcePurgeEvidence::Repository {
                local_realizations_deleted: 0,
            }),
            ResourceKind::MemoryStore => {
                let summary = self
                    .memories
                    .purge_store(&target.resource_id)
                    .await
                    .map_err(indeterminate)?;
                Ok(ResourcePurgeEvidence::MemoryStore {
                    heads_deleted: summary.heads_deleted,
                    versions_deleted: summary.versions_deleted,
                    copy_conflicts_deleted: summary.copy_conflicts_deleted,
                })
            }
            ResourceKind::Skill => Ok(ResourcePurgeEvidence::Skill {
                versions_deleted: self
                    .skills
                    .purge_skill(&target.workspace_id, &target.resource_id)
                    .await
                    .map_err(skill_cleanup_error)?,
            }),
        }
    }
}

fn indeterminate(error: impl std::fmt::Display) -> ResourcePurgeError {
    ResourcePurgeError::Indeterminate {
        detail: error.to_string(),
    }
}

fn registry_guard_error(error: ResourceRegistryError) -> ResourcePurgeError {
    // This read is an admission witness, not a cleanup result. A failed
    // dependency observation cannot authorize physical reclamation.
    match error {
        ResourceRegistryError::CorruptData(detail) => ResourcePurgeError::CorruptData { detail },
        other => ResourcePurgeError::IncompleteReferenceObservation {
            detail: other.to_string(),
        },
    }
}

fn skill_cleanup_error(error: SkillStoreError) -> ResourcePurgeError {
    // Keep known domain causes. IO/unavailable or unexpected dependency
    // failures provide no exact whole-Skill cleanup receipt; even a missing
    // internal file must not be reinterpreted as successful reclamation.
    match error {
        SkillStoreError::Reference(error) => error,
        SkillStoreError::CorruptData { detail } => ResourcePurgeError::CorruptData { detail },
        SkillStoreError::EncodingFailure { detail } => {
            ResourcePurgeError::EncodingFailure { detail }
        }
        SkillStoreError::InvalidDefinition { detail }
        | SkillStoreError::InvalidVersion { detail }
        | SkillStoreError::InvalidSelection { detail } => {
            ResourcePurgeError::InvalidIntent { detail }
        }
        SkillStoreError::BundleMismatch { detail } => ResourcePurgeError::CorruptData { detail },
        SkillStoreError::LifecycleConflict { detail } => {
            ResourcePurgeError::AdmissionIncomplete { detail }
        }
        other => indeterminate(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_dependency_observations_never_authorize_cleanup() {
        // G1 unavailable/unknown registry => incomplete reference evidence;
        // G2 corrupt registry => corruption. Neither is an empty blocker set.
        for error in [
            ResourceRegistryError::AuthorityUnavailable {
                detail: "same".into(),
            },
            ResourceRegistryError::Indeterminate {
                detail: "same".into(),
            },
        ] {
            assert!(matches!(
                registry_guard_error(error),
                ResourcePurgeError::IncompleteReferenceObservation { .. }
            ));
        }
        assert!(matches!(
            registry_guard_error(ResourceRegistryError::CorruptData("same".into())),
            ResourcePurgeError::CorruptData { .. }
        ));
        // P1 known corruption/encoding/refusal keep their cause; P2 IO and a
        // missing dependency-internal item are no proof of whole-store absence.
        assert!(matches!(
            skill_cleanup_error(SkillStoreError::CorruptData {
                detail: "same".into()
            }),
            ResourcePurgeError::CorruptData { .. }
        ));
        assert!(matches!(
            skill_cleanup_error(SkillStoreError::EncodingFailure {
                detail: "same".into()
            }),
            ResourcePurgeError::EncodingFailure { .. }
        ));
        assert!(matches!(
            skill_cleanup_error(SkillStoreError::Reference(ResourcePurgeError::StaleClaim)),
            ResourcePurgeError::StaleClaim
        ));
        for error in [
            SkillStoreError::NotFound("internal item".into()),
            SkillStoreError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "same")),
            SkillStoreError::Indeterminate {
                detail: "same".into(),
            },
        ] {
            assert!(matches!(
                skill_cleanup_error(error),
                ResourcePurgeError::Indeterminate { .. }
            ));
        }
    }
}
