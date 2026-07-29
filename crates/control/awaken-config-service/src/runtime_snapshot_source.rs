//! Runtime-facing projection of the installed Agent publication catalog.
//!
//! The config domain remains publication authority. This adapter exposes its
//! immutable current/exact lookup contract to execution without moving runtime
//! concerns into the authoring service.

use awaken_config_store::ExecutableAgentSnapshot;
use awaken_runtime_contract::resolved::CatalogFingerprint;
use awaken_runtime_contract::resolver::PublishedAgentSnapshotSource;
use awaken_runtime_contract::snapshot::AgentId;

use crate::ConfigService;

impl PublishedAgentSnapshotSource for ConfigService {
    fn current(&self, workspace: &str, agent_id: &AgentId) -> Option<ExecutableAgentSnapshot> {
        self.installed.snapshot_in(workspace, &agent_id.0)
    }

    fn exact(
        &self,
        workspace: &str,
        fingerprint: &CatalogFingerprint,
    ) -> Option<ExecutableAgentSnapshot> {
        self.installed
            .snapshot_by_fingerprint(workspace, &fingerprint.0)
    }

    fn at_revision(
        &self,
        workspace: &str,
        agent_id: &AgentId,
        source_revision: u64,
    ) -> Option<ExecutableAgentSnapshot> {
        self.installed
            .snapshot_at_revision(workspace, &agent_id.0, source_revision)
    }
}
