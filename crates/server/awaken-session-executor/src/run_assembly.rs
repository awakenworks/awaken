// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the Session context assembly component. It builds a Session's Run context
// @formal reason: from the frozen projection under the Session lifecycle guard and installs the projection a
// @formal reason: claimed or prepared Run executes from; this is a product-requirement classification, not
// @formal reason: a direct proof claim.
//! [`RunAssembly`] — assembles the context a Session's Runs execute in.
//!
//! It owns Session context construction (`ctx_for*`), the installation of the
//! frozen Session projection a Run executes from, Run dispatch preparation,
//! the claimed-Run and boundary Worker construction the dispatch resolver asks
//! for, and the capability wiring (Memory, Skills, Web tools, MCP, ACP,
//! credentials) a context composes. It holds the components and capability
//! groups it composes; nothing it holds refers back to it.

mod mcp_attachment;
mod roles;
mod session_preparation;

use std::sync::Arc;

use crate::host::capability;

/// Session context assembly for one process. Consumers: the Session Run
/// components (run, resume, coordination, Outcomes), the dispatch resolver's
/// claimed-Run construction, and the realization roles' executor part.
#[derive(Clone)]
pub struct RunAssembly {
    pub(crate) capabilities: Arc<crate::RunCapabilities>,
    pub(crate) child_runs: Arc<crate::ChildRunLauncher>,
    pub(crate) local_workspace: String,
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
    pub(crate) hub: Arc<crate::ThreadEventHub>,
    pub(crate) authority: Arc<dyn crate::SessionAuthority>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) completion: Arc<crate::host::CompletionRegistry>,
    pub(crate) session_execution: Arc<crate::SessionExecutionDependencies>,
    pub(crate) execution: crate::ExecutionTopology,
    pub(crate) realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    pub(crate) thread_reads: Arc<crate::ThreadReads>,
    pub(crate) resources: Arc<capability::ResourcePorts>,
    pub(crate) mcp: awaken_session_mcp::McpSessions,
    pub(crate) capture: Arc<capability::ContentCaptureCapability>,
    pub(crate) durable_run_cancellation: crate::host::DurableRunCancellation,
    pub(crate) mcp_root_authority: Arc<crate::mcp_host::McpRootAuthority>,
    pub(crate) mcp_application_inputs: Arc<crate::mcp_host::McpApplicationInputs>,
}

impl RunAssembly {
    /// The process wall clock this assembly drives its Workers on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
    }

    /// The platform-managed local workspace coordinate.
    pub fn local_workspace(&self) -> &str {
        &self.local_workspace
    }

    /// Whether this process owns a co-located dispatch pool.
    #[must_use]
    pub fn runs_local_dispatch_pool(&self) -> bool {
        self.execution.local.runs_local_pool()
    }

    /// Stable logical identity used by both local dispatch claims and local
    /// Session realization. The process incarnation is deliberately separate.
    #[must_use]
    pub fn dispatch_owner(&self) -> &str {
        self.execution.lease_owner.as_str()
    }

    /// The shared per-thread live observation hub.
    pub fn hub(&self) -> &Arc<crate::ThreadEventHub> {
        &self.hub
    }
}
