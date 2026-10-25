// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: capability assembly for Runs: tools, Web tools, ACP adapters, credential
// @formal reason: realization profiles, per-thread model routing and the Session inputs those read. It
// @formal reason: decides no lifecycle; this is a product-requirement classification, not a proof claim.
//! [`RunCapabilities`] — the capabilities a Run is assembled with.
//!
//! It owns the tool offering, the Web tools and their credentials, the ACP
//! adapter policy, the installed credential-realization profiles, child
//! execution adapters, per-thread model routing, and the frozen Session
//! inputs (Workspace, Agent, delegates) capability assembly reads. It serves
//! the Session application's `CapabilityRuntime` role.

mod role;

use std::sync::Arc;

use crate::host::capability;

/// Capability assembly for one process. Consumers: Session context assembly,
/// child Run launching, claimed-Run admission and the Session application's
/// capability role.
pub struct RunCapabilities {
    pub(crate) local_workspace: String,
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) background_supervisor: Arc<awaken_ext_background_task::BackgroundTaskSupervisor>,
    pub(crate) agent: Arc<capability::AgentExecution>,
    pub(crate) exec: Arc<capability::AttemptBackends>,
    pub(crate) tools: Arc<capability::ToolOffering>,
    pub(crate) worker: Arc<capability::WorkerTransports>,
    pub(crate) session_application: crate::SessionApplicationBindings,
}

impl RunCapabilities {
    pub(crate) fn register_thread_workspace(
        &self,
        thread: &str,
        workspace: &str,
    ) -> Result<(), crate::realization_error::RealizationLock> {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.workspace = Some(workspace.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn register_thread_agent_projection(
        &self,
        thread: &str,
        agent_id: &str,
    ) -> Result<(), crate::realization_error::RealizationLock> {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.agent_id = Some(agent_id.to_string()))
    }

    /// Rebuildable Agent projection used only as input to Session admission.
    /// Runtime never treats this cache as an executable publication authority.
    /// A poisoned installed-Session registry is refused, never read as absent.
    pub fn thread_agent_projection(
        &self,
        thread: &str,
    ) -> Result<Option<String>, crate::realization_error::RealizationError> {
        let realizations = self.installed_sessions.realizations();
        Ok(realizations.frozen(thread)?.agent_id.clone())
    }

    /// Workspace context supplied to the Session application admission boundary.
    /// Unknown flat-protocol threads use the process's persisted local workspace.
    /// A poisoned installed-Session registry is refused: no caller can fall
    /// back to the local workspace for a Session it cannot read.
    pub fn thread_workspace(
        &self,
        thread: &str,
    ) -> Result<String, crate::realization_error::RealizationError> {
        self.installed_sessions
            .realizations()
            .workspace(thread, &self.local_workspace)
    }

    /// Workspace for a prepared thread; unlike `thread_workspace`, unknown ids
    /// do not inherit local ownership and therefore cannot enumerate artifacts.
    /// A poisoned registry is refused, never read as an unregistered thread.
    pub fn registered_thread_workspace(
        &self,
        thread: &str,
    ) -> Result<Option<String>, crate::realization_error::RealizationError> {
        self.installed_sessions
            .realizations()
            .registered_workspace(thread)
    }

    /// Bind the exact published delegation targets to a prepared Session.
    pub fn register_thread_delegates(
        &self,
        thread: &str,
        delegates: Vec<String>,
    ) -> Result<(), crate::realization_error::RealizationLock> {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.delegates = delegates)
    }

    pub(crate) fn thread_delegate_ids(
        &self,
        thread: &str,
    ) -> Result<Option<Vec<String>>, crate::realization_error::RealizationLock> {
        self.installed_sessions
            .read(thread, |installed| installed.inputs.delegates.clone())
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first Run.
    pub fn register_thread_model(
        &self,
        thread: &str,
        model_ref: impl Into<String>,
    ) -> Result<(), crate::realization_error::RealizationLock> {
        self.agent.inference_routing.register(thread, model_ref)
    }

    /// Read-only provider discovery for an embedding capability endpoint. The
    /// returned registry is the same value Session dispatch uses.
    #[must_use]
    pub fn web_search_provider_registry(
        &self,
    ) -> &awaken_ext_builtin_tools::WebSearchProviderRegistry {
        &self.tools.web_search_providers
    }

    pub(crate) fn acp_adapter_policy(
        &self,
    ) -> Result<
        &dyn awaken_acp_contract::AcpAdapterPolicy<
            awaken_runtime_contract::resolved::ProviderServerTool,
        >,
        crate::SessionExecutionError,
    > {
        self.exec
            .acp
            .as_ref()
            .map(|factory| factory.adapter_policy())
            .or(self.exec.acp_projection_policy)
            .ok_or_else(|| {
                crate::SessionExecutionError::bad_request("ACP backend is not installed")
            })
    }

    /// The model id echoed by adapters in their session/agent objects.
    pub fn model(&self) -> String {
        self.agent.model_ref.clone()
    }

    /// The primary model currently selected for `thread`, including a Managed
    /// per-Session/per-Run override. This is a read-only projection of the same
    /// inference-routing source that execution consumes.
    pub fn model_for_thread(
        &self,
        thread: &str,
    ) -> Result<String, crate::realization_error::RealizationError> {
        self.agent
            .inference_routing
            .model_ref(thread, &self.agent.model_ref)
    }

    /// The default Agent's published delegation targets in one Workspace.
    pub fn delegate_ids_in(&self, workspace: &str) -> Vec<String> {
        self.agent
            .agent_publications
            .as_ref()
            .and_then(|source| {
                source.current(
                    workspace,
                    &awaken_runtime_contract::snapshot::AgentId("assistant".into()),
                )
            })
            .map(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .delegates
                    .into_iter()
                    .map(|binding| binding.agent_id.0)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The set of client-executed tool ids (model-visible, host-unregistered).
    pub fn client_tools(&self) -> &std::collections::HashSet<String> {
        &self.tools.client_tools
    }

    /// The registered built-in tools advertised on a managed session's agent object:
    /// each hand-tool id and whether its calls require confirmation. Folded into the
    /// public `agent_toolset` by the adapter. Deterministic from host config.
    pub fn builtin_tools(&self) -> Vec<(String, bool)> {
        awaken_session_run_delivery::builtin_hand_tools()
    }

    /// The client-executed (custom) tools advertised on a managed session: their
    /// descriptors, so the adapter can shape each as a `custom` tool definition.
    pub fn custom_tools(&self) -> Vec<awaken_runtime_contract::resolved::ToolDescriptor> {
        self.tools
            .client_tools
            .iter()
            .map(|id| crate::config::client_tool_descriptor(id))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn register_thread_backend_projection(&self, thread: &str, backend_ref: &str) {
        self.installed_sessions
            .replace_frozen_with(thread, |next| {
                next.backend_ref = crate::frozen_inputs::backend_projection(backend_ref);
            })
            .expect("installed Sessions");
    }
}
