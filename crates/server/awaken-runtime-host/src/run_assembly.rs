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

use awaken_runtime_contract::delegation::RunDelegationService;

use crate::HostError;
use crate::host::capability;

/// Session context assembly for one process. Consumers: the Session Run
/// components (run, resume, coordination, Outcomes), the dispatch resolver's
/// claimed-Run construction, and the realization roles' executor part.
pub struct RunAssembly {
    pub(crate) local_workspace: String,
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
    pub(crate) hub: Arc<crate::ThreadEventHub>,
    pub(crate) authority: Arc<dyn crate::SessionAuthority>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) background_supervisor: Arc<awaken_ext_background_task::BackgroundTaskSupervisor>,
    pub(crate) completion: Arc<crate::host::CompletionRegistry>,
    pub(crate) session_execution: Arc<crate::SessionExecutionDependencies>,
    pub(crate) execution: crate::ExecutionTopology,
    pub(crate) worker: Arc<capability::WorkerTransports>,
    pub(crate) agent: Arc<capability::AgentExecution>,
    pub(crate) exec: Arc<capability::AttemptBackends>,
    pub(crate) realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    pub(crate) thread_reads: Arc<crate::ThreadReads>,
    pub(crate) tools: Arc<capability::ToolOffering>,
    pub(crate) resources: Arc<capability::ResourcePorts>,
    pub(crate) mcp: awaken_session_mcp::McpSessions,
    pub(crate) capture: Arc<capability::ContentCaptureCapability>,
    pub(crate) session_application: crate::SessionApplicationBindings,
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

    pub(crate) fn register_thread_workspace(&self, thread: &str, workspace: &str) {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.workspace = Some(workspace.to_string()));
    }

    #[cfg(test)]
    pub(crate) fn register_thread_agent_projection(&self, thread: &str, agent_id: &str) {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.agent_id = Some(agent_id.to_string()));
    }

    /// Rebuildable Agent projection used only as input to Session admission.
    /// Runtime never treats this cache as an executable publication authority.
    pub fn thread_agent_projection(&self, thread: &str) -> Option<String> {
        let realizations = self.installed_sessions.realizations();
        realizations
            .frozen(thread)
            .expect("session runtime slots mutex poisoned")
            .agent_id
            .clone()
    }

    /// Workspace context supplied to the Session application admission boundary.
    /// Unknown flat-protocol threads use the process's persisted local workspace.
    pub fn thread_workspace(&self, thread: &str) -> String {
        self.installed_sessions
            .realizations()
            .workspace(thread, &self.local_workspace)
            .expect("session runtime slots mutex poisoned")
    }

    /// Workspace for a prepared thread; unlike `thread_workspace`, unknown ids
    /// do not inherit local ownership and therefore cannot enumerate artifacts.
    pub fn registered_thread_workspace(&self, thread: &str) -> Option<String> {
        self.installed_sessions
            .realizations()
            .registered_workspace(thread)
            .expect("session runtime slots mutex poisoned")
    }

    /// Bind the exact published delegation targets to a prepared Session.
    pub fn register_thread_delegates(&self, thread: &str, delegates: Vec<String>) {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.delegates = delegates);
    }

    pub(crate) fn thread_delegate_ids(&self, thread: &str) -> Option<Vec<String>> {
        self.installed_sessions
            .read(thread, |slot| slot.inputs.delegates.clone())
    }

    /// Route every run's tool calls through `hand` — a remote `ToolExecutor`
    /// Build the per-session delegation executor. `sandbox` is the calling thread's
    /// live environment: a native delegate shares it, so the parent
    /// and its native child collaborate in one Session-owned workspace.
    pub(crate) fn run_delegation(
        &self,
        thread: &str,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        commit: Arc<crate::store::HostCommit>,
        parent_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        publication_source: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
        tool_configuration: crate::delegate::CallerToolConfiguration,
    ) -> Result<Option<Arc<dyn RunDelegationService>>, HostError> {
        let Some(parent_snapshot) = parent_snapshot else {
            return Ok(None);
        };
        let agent_bindings = &parent_snapshot.resolved_spec.plugin_config.agent;
        if agent_bindings.delegates.is_empty() && agent_bindings.advisor.is_none() {
            return Ok(None);
        }
        let workspace = self.thread_workspace(thread);
        // Use the same topology decision as pool startup. Ephemeral and durable
        // Runtime authorities both own dispatch; gating children on persistence
        // durability leaves a claim-bound parent executing its child with the
        // parent's checkpoint/commit projection instead of a child claim.
        let scheduler = if self.runs_local_dispatch_pool() {
            Some(crate::agent_runner::RunScheduler {
                clock: self.clock.clone(),
                binding: self.authority.dispatch_binding().ok_or_else(|| {
                    HostError::internal("child scheduling requires a dispatch binding")
                })?,
                commit: commit.clone(),
                reader: commit,
                owner: self.dispatch_owner().to_owned(),
                session_resources: self
                    .realization_executor
                    .staging()
                    .thread_resource_manifest(thread)?,
                publication_source: Some(publication_source.clone()),
                claimed_model_request_gate: self.coordination_endpoint().map(|endpoint| {
                    crate::coordination::claimed_model_request_gate_factory(endpoint, thread)
                }),
            })
        } else {
            None
        };
        let adapters = self.child_execution_adapters(thread, sandbox.clone());
        let service = crate::delegate::HostRunDelegationService::new(
            self.agent.llm.clone(),
            sandbox,
            parent_snapshot,
            adapters,
            Some(publication_source),
            workspace,
        )
        .map_err(|error| HostError::bad_request(error.to_string()))?
        .with_scheduler(scheduler)
        .with_tool_configuration(tool_configuration);
        Ok(Some(Arc::new(service)))
    }
}
