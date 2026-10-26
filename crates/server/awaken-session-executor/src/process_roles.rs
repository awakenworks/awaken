//! Claiming-Worker roles over the executor's selected components.

use std::sync::Arc;

use awaken_dispatch_contract::CompletionSink;
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_dispatch_execution::{ProcessDispatch, WorkerResolver};
use awaken_session_realization::peers::EnvironmentPeer as _;
use awaken_session_run_delivery::SessionDispatchLifecycle;

/// The execution roles a database-less Worker composition selects.
#[derive(Clone)]
pub struct WorkerRoles {
    lifecycle: SessionDispatchLifecycle,
    assembly: Arc<crate::RunAssembly>,
    runs: Arc<crate::SessionRuns>,
}

/// Select the shared process lifecycle over the executor's original authority,
/// lazy resolver and auxiliary registry. Coordinator-only roots construct the
/// same component directly, without an executor.
#[must_use]
pub fn session_dispatch_lifecycle(
    host: &crate::ExecutorComponents,
    completion: Arc<dyn CompletionSink>,
) -> SessionDispatchLifecycle {
    let assembly = host.run_assembly.clone();
    let authority = assembly.authority.as_ref();
    let terminal_owner = authority
        .scans_committed_terminals()
        .then(|| crate::host::capability::local_dispatch_owner(authority))
        .flatten();
    SessionDispatchLifecycle::new(
        host.dispatch.clone(),
        authority.dispatch_binding(),
        completion,
        terminal_owner,
        host.resources.memory.background(),
        move |dispatch| {
            crate::host::session_worker_resolver(&assembly, dispatch)
                .map(|resolver| Arc::new(resolver) as Arc<dyn WorkerResolver<AnyDispatchStore>>)
                .map_err(|error| {
                    awaken_dispatch_execution::session_execution::execution_error(error.message)
                })
        },
    )
}

impl WorkerRoles {
    /// The Worker role set over `host`'s components.
    #[must_use]
    pub fn of(host: &crate::ExecutorComponents) -> Self {
        Self {
            lifecycle: session_dispatch_lifecycle(host, host.worker_completion_sink()),
            assembly: host.run_assembly.clone(),
            runs: host.session_runs.clone(),
        }
    }

    /// This Worker's dispatch: pool, admission and readiness.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<ProcessDispatch> {
        self.lifecycle.dispatch()
    }

    /// Session context assembly: realization renewal, revocation and
    /// preparation the Worker lifecycle drives.
    #[must_use]
    pub fn run_assembly(&self) -> &Arc<crate::RunAssembly> {
        &self.assembly
    }

    /// Session Run delivery: the resident Runs a revocation interrupts.
    #[must_use]
    pub fn session_runs(&self) -> &Arc<crate::SessionRuns> {
        &self.runs
    }

    /// The warm Environment capacity of this Worker's providers.
    #[must_use]
    pub fn environment_capacity(&self) -> &crate::environment_providers::EnvironmentProviders {
        self.assembly
            .realization_executor
            .environments()
            .providers()
    }

    /// Observe whether the original source of one Session this Worker holds
    /// is retained, for the retention report a draining Worker sends. Only
    /// the Worker role set observes Sandboxes; the root supplies preparation
    /// status, and this read installs no slot and grants no preparation,
    /// disposal, retirement or cleanup receipt.
    pub async fn observe_source_retention(
        &self,
        session_id: &awaken_session_types::SessionId,
        projection: &awaken_session_realization_contract::FrozenSessionProjection,
        source_prepared: bool,
    ) -> Result<bool, awaken_session_types::RunError> {
        self.assembly
            .realization_executor
            .environments()
            .observe_source_retention(session_id, projection, source_prepared)
            .await
            .map_err(awaken_session_types::RunError::from)
    }

    /// The process wall clock this Worker drives its claims on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.assembly.clock()
    }

    /// Spawn the Worker's dispatch pool once its upstream authority is bound.
    pub fn start_pool(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.lifecycle.start_local_pool()
    }

    /// Drain claimed Runs, then the auxiliary work they created, by `timeout`.
    pub async fn drain(&self, timeout: std::time::Duration) -> bool {
        self.lifecycle.drain(timeout).await
    }
}
