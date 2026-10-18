// @formal entrypoint delegation.independent_child_scheduling_lease
//! [`ChildRunLauncher`] — launches the child Runs a parent Run delegates.
//!
//! It builds the per-parent [`RunDelegationService`] a Session context installs:
//! native and remote child execution over the parent's Session environment,
//! and, where this process runs the local dispatch claimer, the independent
//! child scheduling lease and the claimed model-request gate.

use std::sync::Arc;

use awaken_runtime_contract::delegation::RunDelegationService;

use crate::HostError;

/// Child-Run launching for one process. Consumers: Session context assembly
/// and claimed child-Run construction.
pub struct ChildRunLauncher {
    capabilities: Arc<crate::RunCapabilities>,
    authority: Arc<dyn crate::SessionAuthority>,
    clock: Arc<dyn awaken_time::WallClock>,
    execution: crate::ExecutionTopology,
    realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
}

impl ChildRunLauncher {
    pub(crate) fn new(
        capabilities: Arc<crate::RunCapabilities>,
        authority: Arc<dyn crate::SessionAuthority>,
        clock: Arc<dyn awaken_time::WallClock>,
        execution: crate::ExecutionTopology,
        realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    ) -> Self {
        Self {
            capabilities,
            authority,
            clock,
            execution,
            realization_executor,
        }
    }

    /// Route every run's tool calls through `hand` — a remote `ToolExecutor`
    /// Build the per-session delegation executor. `sandbox` is the calling thread's
    /// live environment: a native delegate shares it, so the parent
    /// and its native child collaborate in one Session-owned workspace.
    pub(crate) fn delegation_service(
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
        let workspace = self.capabilities.thread_workspace(thread)?;
        // Use the same topology decision as pool startup. Ephemeral and durable
        // Runtime authorities both own dispatch; gating children on persistence
        // durability leaves a claim-bound parent executing its child with the
        // parent's checkpoint/commit projection instead of a child claim.
        let scheduler = if self.execution.local.runs_local_pool() {
            Some(crate::agent_runner::RunScheduler {
                clock: self.clock.clone(),
                binding: self.authority.dispatch_binding().ok_or_else(|| {
                    HostError::internal("child scheduling requires a dispatch binding")
                })?,
                commit: commit.clone(),
                reader: commit,
                owner: self.execution.lease_owner.as_str().to_owned(),
                session_resources: self
                    .realization_executor
                    .staging()
                    .thread_resource_manifest(thread)?,
                publication_source: Some(publication_source.clone()),
                claimed_model_request_gate: self.capabilities.coordination_endpoint().map(
                    |endpoint| {
                        crate::coordination::claimed_model_request_gate_factory(endpoint, thread)
                    },
                ),
            })
        } else {
            None
        };
        let adapters = self
            .capabilities
            .child_execution_adapters(thread, sandbox.clone())?;
        let service = crate::delegate::HostRunDelegationService::new(
            self.capabilities.agent.llm.clone(),
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
