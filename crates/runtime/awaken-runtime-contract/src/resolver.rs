use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("snapshot fingerprint is empty or internally inconsistent")]
    FingerprintMismatch,
    #[error("snapshot not found")]
    SnapshotNotFound,
}

pub trait RunResolver {
    fn resolve(
        &self,
        snapshot: &crate::snapshot::ExecutableAgentSnapshot,
    ) -> Result<crate::resolved::ResolvedRun, Error>;
}

pub trait AgentSnapshotResolver {
    fn get_snapshot(
        &self,
        id: &crate::snapshot::ExecutableAgentSnapshotId,
    ) -> Result<Option<crate::snapshot::ExecutableAgentSnapshot>, Error>;
}

pub trait AgentSnapshotCatalog {
    fn list_snapshots(&self) -> Vec<crate::snapshot::ExecutableAgentSnapshotId>;
}

/// Immutable execution lookup for published Agent snapshots.
///
/// New Runs resolve the current publication by Agent identity. Recovery resolves
/// the exact immutable publication recorded in the Run's resume ticket, so a
/// later publish cannot change the behavior of an already-created Run.
pub trait PublishedAgentSnapshotSource: Send + Sync {
    fn current(
        &self,
        workspace: &str,
        agent_id: &crate::snapshot::AgentId,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot>;

    fn exact(
        &self,
        workspace: &str,
        fingerprint: &crate::resolved::CatalogFingerprint,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot>;
}

/// Immutable publication source for embedded hosts and tests that do not run the
/// authoring plane. Callers still provide complete executable snapshots; this is
/// not a second config compiler.
pub struct StaticPublishedAgentSnapshots {
    current: HashMap<crate::snapshot::AgentId, crate::snapshot::ExecutableAgentSnapshot>,
    exact: HashMap<crate::resolved::CatalogFingerprint, crate::snapshot::ExecutableAgentSnapshot>,
}

impl StaticPublishedAgentSnapshots {
    pub fn try_new(
        snapshots: impl IntoIterator<Item = crate::snapshot::ExecutableAgentSnapshot>,
    ) -> Result<Self, Error> {
        let mut current = HashMap::new();
        let mut exact = HashMap::new();
        for snapshot in snapshots {
            if snapshot.root_agent_id.0.is_empty()
                || snapshot.fingerprint.0.is_empty()
                || snapshot.fingerprint != snapshot.resolved_spec.catalog_fingerprint
            {
                return Err(Error::FingerprintMismatch);
            }
            if current
                .insert(snapshot.root_agent_id.clone(), snapshot.clone())
                .is_some()
                || exact
                    .insert(snapshot.fingerprint.clone(), snapshot)
                    .is_some()
            {
                return Err(Error::FingerprintMismatch);
            }
        }
        Ok(Self { current, exact })
    }
}

impl PublishedAgentSnapshotSource for StaticPublishedAgentSnapshots {
    fn current(
        &self,
        _workspace: &str,
        agent_id: &crate::snapshot::AgentId,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot> {
        self.current.get(agent_id).cloned()
    }

    fn exact(
        &self,
        _workspace: &str,
        fingerprint: &crate::resolved::CatalogFingerprint,
    ) -> Option<crate::snapshot::ExecutableAgentSnapshot> {
        self.exact.get(fingerprint).cloned()
    }
}
