//! Run resolution: turn an executable snapshot into a validated `ResolvedRun`.
//!
//! Resolution is the runtime's fail-closed gate (G4/G22). Both execution inputs
//! — an inline `ExecutableAgentSnapshot` and an `ExecutableAgentSnapshotId` —
//! converge here through [`Runtime::load_snapshot`] and are validated for
//! content-address consistency before any model call (G28).

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
            AgentSnapshotInput::Inline(snapshot) => Ok(snapshot.as_ref().clone()),
            AgentSnapshotInput::ById(id) => self.snapshot_by_id(id).ok_or(Error::SnapshotNotFound),
        }
    }
}

impl RunResolver for Runtime {
    fn resolve(&self, snapshot: &ExecutableAgentSnapshot) -> Result<ResolvedRun, Error> {
        // The configuration plane resolved and signed off one immutable value.
        // Execution validates that value; node topology and mutable node state do
        // not alter its meaning.
        if snapshot.fingerprint.0.trim().is_empty()
            || snapshot.resolved_spec.catalog_fingerprint != snapshot.fingerprint
        {
            return Err(Error::FingerprintMismatch);
        }

        Ok(ResolvedRun {
            snapshot_id: snapshot.id.clone(),
            agent_id: snapshot.root_agent_id.clone(),
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
