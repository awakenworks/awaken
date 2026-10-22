// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The Session process owner delegates bounded cleanup to one injected reaper;
// @formal reason: this port classifies the ownership boundary, not a new proof or cleanup ladder.
//! One process cleanup port for Session-owned MCP and Agent processes.

use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract::ProcessHandle;

/// Reap the same owned handle after a Session effect closes admission.
/// Implementations retain their original error rendering because Host MCP
/// reason codes wrap that diagnostic without branching on its message.
/// `Ok` confirms this same handle's observed exit. The boolean reports whether
/// kill escalation was needed; `Ok(false)` is also a completed reap.
#[async_trait]
pub trait ProcessReaper: Send + Sync {
    async fn reap(&self, process: &dyn ProcessHandle, grace: Duration) -> Result<bool, String>;
}
