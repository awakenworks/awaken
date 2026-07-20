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
