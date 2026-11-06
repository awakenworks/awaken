// @formal surface obligation=resource_reclamation.physical_purge_requires_zero_references_leases_and_fence
// @formal surface obligation=resource_reclamation.reference_and_lease_races_fail_closed
// @formal reason: Static owner: the read-only Session reference projection consumed by the existing
// @formal reason: Resource reclamation authority; it owns no purge command, lease, fence, or receipt.
// @formal reason: Dynamic path: it resolves every active and pending Session Resource reference to
// @formal reason: canonical targets, reports exact blockers, and turns quarantine or catalog uncertainty
// @formal reason: into a fail-closed error before the reclaimer may physically purge; the linked
// @formal reason: obligations also retain the separate lease and effect-fence requirements owned
// @formal reason: downstream.
//! Read-only Session reference projection for physical Resource reclamation.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, ResourceKind, ResourcePurgeError, ResourcePurgeGuard, ResourceReference,
    ResourceReferenceKind, ResourceTarget,
};
use awaken_session_configuration_contract::resource::ResolvedInputSource;
use awaken_session_contract::SessionRepository;
use awaken_session_contract::SessionResourceReferences;

pub(crate) async fn resource_targets(
    files: &dyn FileCatalog,
    workspace: &str,
    resources: &SessionResourceReferences,
) -> Result<std::collections::BTreeSet<ResourceTarget>, ResourcePurgeError> {
    let mut targets = std::collections::BTreeSet::new();
    for input in resources.inputs() {
        let target = match &input.source {
            ResolvedInputSource::File { file_id } => ResourceTarget::new(
                workspace,
                ResourceKind::File,
                files
                    .get_file(workspace, file_id.as_str(), true)
                    .await
                    .map_err(ResourcePurgeError::from)?
                    .ok_or_else(|| ResourcePurgeError::IncompleteReferenceObservation {
                        detail: format!("Session references missing File `{file_id}`"),
                    })?
                    .blob_id,
            ),
            ResolvedInputSource::MemoryStore {
                memory_store_id, ..
            } => ResourceTarget::new(
                workspace,
                ResourceKind::MemoryStore,
                memory_store_id.as_str(),
            ),
            ResolvedInputSource::Repository { repository_id, .. } => {
                ResourceTarget::new(workspace, ResourceKind::Repository, repository_id.as_str())
            }
        };
        targets.insert(target);
    }
    targets.extend(
        resources
            .skills()
            .iter()
            .filter(|skill| skill.kind == awaken_agent_contract::AgentSkillKind::Custom)
            .map(|skill| ResourceTarget::new(workspace, ResourceKind::Skill, &skill.skill_id)),
    );
    Ok(targets)
}

/// Read-only reclamation guard over canonical Session aggregates. The durable
/// reference index closes mutation races; this independent scan prevents a
/// not-yet-realized pending manifest from being mistaken for unused data.
pub struct SessionResourcePurgeGuard {
    sessions: Arc<dyn SessionRepository>,
    files: Arc<dyn FileCatalog>,
}

impl SessionResourcePurgeGuard {
    #[must_use]
    pub fn new(sessions: Arc<dyn SessionRepository>, files: Arc<dyn FileCatalog>) -> Self {
        Self { sessions, files }
    }
}

#[async_trait::async_trait]
impl ResourcePurgeGuard for SessionResourcePurgeGuard {
    async fn blockers(
        &self,
        target: &ResourceTarget,
        _config_version: Option<u64>,
        _now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let (blockers, _) = crate::recovery_scan::fold_reconcilable_sessions(
            self.sessions.as_ref(),
            std::collections::BTreeSet::new(),
            |mut blockers, page| async move {
                if !page.quarantined.is_empty() {
                    return Err(ResourcePurgeError::IncompleteReferenceObservation {
                        detail: "Session recovery quarantine blocks physical Resource purge".into(),
                    });
                }
                for scoped in page.sessions {
                    let workspace = scoped.workspace_id;
                    let session = scoped.session;
                    let references = session.resources.resource_references();
                    let candidates =
                        resource_targets(self.files.as_ref(), &workspace, &references).await?;
                    let matches = candidates.iter().any(|candidate| {
                        if target.kind == ResourceKind::File {
                            candidate.kind == ResourceKind::File
                                && candidate.resource_id == target.resource_id
                        } else {
                            candidate == target
                        }
                    });
                    if matches {
                        blockers.insert(session.session_id);
                    }
                }
                Ok(blockers)
            },
            |error| ResourcePurgeError::Indeterminate {
                detail: error.to_string(),
            },
        )
        .await?;
        // Only EOF can publish a zero-blocker observation. Catalog reads above
        // have no physical effect and a later-page error discards the prefix.
        Ok(blockers
            .into_iter()
            .map(|session_id| ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: String::from(session_id),
            })
            .collect())
    }
}
