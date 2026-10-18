// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: the Session Run component. It drives run, resume, streaming, interruption
// @formal reason: and step finalization over the context RunAssembly builds and the committed truth
// @formal reason: ThreadReads reads; this is a product-requirement classification, not a proof claim.
//! [`SessionRuns`] — drives a Session's Runs.
//!
//! It owns run, resume and streaming delivery, interruption, durable
//! foreground submission and settlement observation, step finalization,
//! resident Run control and the durable-ingress operation verbs. It executes
//! over the contexts [`crate::RunAssembly`] builds and reads committed truth
//! through [`crate::ThreadReads`]; nothing it holds refers back to it.

mod roles;
mod run_application;

use std::sync::Arc;

use crate::HostError;
use crate::host::capability;

/// Session Run delivery for one process. Consumers: the Session application's
/// `SessionRuntime` and `StreamingRunRuntime` roles, the protocol-neutral
/// `RunApplication` (AI SDK, AG-UI, A2A), the durable-ingress verbs, and the
/// Run-driving parts of coordination, budgets and Outcomes.
pub struct SessionRuns {
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
    pub(crate) authority: Arc<dyn crate::SessionAuthority>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) hub: Arc<crate::ThreadEventHub>,
    pub(crate) completion: Arc<crate::host::CompletionRegistry>,
    pub(crate) agent: Arc<capability::AgentExecution>,
    pub(crate) exec: Arc<capability::AttemptBackends>,
    pub(crate) execution: crate::ExecutionTopology,
    pub(crate) capabilities: Arc<crate::RunCapabilities>,
    pub(crate) run_assembly: Arc<crate::RunAssembly>,
    pub(crate) thread_reads: Arc<crate::ThreadReads>,
    pub(crate) local_pool: capability::LocalDispatchPool,
    pub(crate) dispatch_hint: awaken_dispatch_execution::DispatchHint,
}

impl SessionRuns {
    /// Session Run delivery over the contexts `run_assembly` builds, the
    /// completion relay foreground Runs settle through, and the local pool.
    pub(crate) fn new(
        run_assembly: Arc<crate::RunAssembly>,
        completion: Arc<crate::host::CompletionRegistry>,
        local_pool: capability::LocalDispatchPool,
        dispatch_hint: awaken_dispatch_execution::DispatchHint,
    ) -> Self {
        Self {
            installed_sessions: run_assembly.installed_sessions.clone(),
            authority: run_assembly.authority.clone(),
            clock: run_assembly.clock.clone(),
            hub: run_assembly.hub.clone(),
            agent: run_assembly.capabilities.agent.clone(),
            exec: run_assembly.capabilities.exec.clone(),
            execution: run_assembly.execution.clone(),
            thread_reads: run_assembly.thread_reads.clone(),
            capabilities: run_assembly.capabilities.clone(),
            completion,
            local_pool,
            dispatch_hint,
            run_assembly,
        }
    }

    /// The process wall clock Runs are driven on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
    }

    /// The model id echoed for Runs that select no other model.
    pub fn model(&self) -> String {
        self.capabilities.model()
    }

    /// The Coordinator dispatch owner, when this process's authority holds it.
    pub(crate) fn optional_dispatch_store(
        &self,
    ) -> Option<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>> {
        capability::local_dispatch_owner(self.authority.as_ref())
    }

    pub fn coordinator_dispatch(
        &self,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>, HostError> {
        self.optional_dispatch_store()
            .ok_or_else(|| HostError::internal("operation requires a Coordinator dispatch owner"))
    }

    /// The process dispatch pool, or a fail-closed error when durable ingress
    /// (and thus the pool) is not enabled.
    pub(crate) fn dispatch_pool_or_err(
        &self,
    ) -> Result<
        &Arc<awaken_dispatch_execution::DispatchPool<awaken_dispatch_erasure::AnyDispatchStore>>,
        HostError,
    > {
        self.local_pool.get().ok_or_else(|| {
            HostError::bad_request(
                "durable dispatch not enabled (set typed durable ingress to run the pool)",
            )
        })
    }

    /// Hint the local pool after a durable dispatch change, when one runs.
    pub(crate) async fn notify_local_pool(&self) {
        if self.local_pool.get().is_some() {
            self.dispatch_hint.publish().await;
        }
    }
}
