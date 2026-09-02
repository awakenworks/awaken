//! Read-only Session reference projection for physical Resource reclamation.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, ResourceKind, ResourcePurgeError, ResourcePurgeGuard, ResourceReference,
    ResourceReferenceKind, ResourceTarget,
};
use awaken_session_contract::{
    ManagedSessionRepository, ResolvedInputSource, SessionResourceReferences,
};

pub(super) async fn resource_targets(
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
                    .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?
                    .ok_or_else(|| {
                        ResourcePurgeError::Invalid(format!(
                            "Session references missing File `{file_id}`"
                        ))
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
    sessions: Arc<dyn ManagedSessionRepository>,
    files: Arc<dyn FileCatalog>,
}

impl SessionResourcePurgeGuard {
    #[must_use]
    pub fn new(sessions: Arc<dyn ManagedSessionRepository>, files: Arc<dyn FileCatalog>) -> Self {
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
        let mut blockers = std::collections::BTreeSet::new();
        let sessions = crate::scan_all_reconcilable_sessions(self.sessions.as_ref())
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        if !sessions.quarantined.is_empty() {
            return Err(ResourcePurgeError::Storage(
                "Session recovery quarantine blocks physical Resource purge".to_string(),
            ));
        }
        for scoped in sessions.sessions {
            let workspace = scoped.workspace_id;
            let session = scoped.session;
            let references = session.resources.resource_references();
            let candidates = resource_targets(self.files.as_ref(), &workspace, &references).await?;
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
        Ok(blockers
            .into_iter()
            .map(|session_id| ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: session_id,
            })
            .collect())
    }
}
