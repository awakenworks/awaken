//! Run resolution: turn an executable snapshot into a validated `ResolvedRun`.
//!
//! Resolution is the runtime's fail-closed gate (G4/G22). Both execution inputs
//! — an inline `ExecutableAgentSnapshot` and an `ExecutableAgentSnapshotId` —
//! converge here through [`Runtime::load_snapshot`] and are validated against
//! the active catalog fingerprint before any model call (G28).

use awaken_runtime_contract::resolved::ResolvedRun;
use awaken_runtime_contract::resolver::{
    AgentSnapshotCatalog, AgentSnapshotResolver, Error, RunResolver,
};
use awaken_runtime_contract::snapshot::{
    AgentSnapshotInput, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

use crate::runtime::Runtime;

impl Runtime {
    /// Converge an `AgentSnapshotInput` into one executable snapshot. Inline
    /// data is taken as-is; a by-id input is looked up through the registry.
    /// The result still flows through [`RunResolver::resolve`] for validation.
    pub fn load_snapshot(
        &self,
        input: &AgentSnapshotInput,
    ) -> Result<ExecutableAgentSnapshot, Error> {
        match input {
            AgentSnapshotInput::Inline(snapshot) => Ok(snapshot.clone()),
            AgentSnapshotInput::ById(id) => self.snapshot_by_id(id).ok_or(Error::SnapshotNotFound),
        }
    }
}

impl RunResolver for Runtime {
    fn resolve(&self, snapshot: &ExecutableAgentSnapshot) -> Result<ResolvedRun, Error> {
        let active = self.active_fingerprint().ok_or(Error::NoActiveCatalog)?;

        // Fail closed unless every fingerprint the snapshot carries matches the
        // installed catalog: the snapshot identity and its resolved spec must
        // both descend from the active publication (G4).
        if snapshot.fingerprint != active || snapshot.resolved_spec.catalog_fingerprint != active {
            return Err(Error::FingerprintMismatch);
        }

        Ok(ResolvedRun {
            snapshot_id: snapshot.id.clone(),
            spec: snapshot.resolved_spec.clone(),
        })
    }
}

impl AgentSnapshotResolver for Runtime {
    fn get_snapshot(
        &self,
        id: &ExecutableAgentSnapshotId,
    ) -> Result<Option<ExecutableAgentSnapshot>, Error> {
        Ok(self.snapshot_by_id(id))
    }
}

impl AgentSnapshotCatalog for Runtime {
    fn list_snapshots(&self) -> Vec<ExecutableAgentSnapshotId> {
        self.snapshot_ids()
    }
}
