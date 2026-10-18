// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! [`CoordinatorRoles`] and [`WorkerRoles`] — the two typed role sets a
//! composition root selects from the executor components.
//!
//! A process is a Coordinator (it hosts the Session application, admits Runs,
//! and either drains them in a co-located pool or leaves them to registered
//! Workers) or a database-less Worker (it drains claimed Runs and holds warm
//! Environment capacity). The deployment difference is expressed once, by the
//! root choosing one of these sets; each set carries exactly the roles and the
//! process operations that deployment uses.

use std::sync::Arc;

use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_dispatch_execution::{ProcessDispatch, WorkerResolver};
use awaken_session_realization::peers::EnvironmentPeer as _;

/// The executor roles a Coordinator process composes. Consumers: the
/// Coordinator and server composition roots.
#[derive(Clone)]
pub struct CoordinatorRoles {
    session_execution: awaken_session_execution_contract::SessionExecutionRoles,
    process: ProcessOperations,
}

/// The executor roles a database-less Worker process composes. Consumer: the
/// Worker composition root.
#[derive(Clone)]
pub struct WorkerRoles {
    process: ProcessOperations,
}

/// The process-level operations both role sets share: starting the dispatch
/// pool over the executor's claimed-Run resolver and draining claimed Runs
/// before the auxiliary work they created.
#[derive(Clone)]
struct ProcessOperations {
    dispatch: Arc<ProcessDispatch>,
    assembly: Arc<crate::RunAssembly>,
    runs: Arc<crate::SessionRuns>,
    resources: Arc<crate::host::capability::ResourcePorts>,
}

impl ProcessOperations {
    fn of(host: &crate::SharedHost) -> Self {
        Self {
            dispatch: host.dispatch.clone(),
            assembly: host.run_assembly.clone(),
            runs: host.session_runs.clone(),
            resources: host.resources.clone(),
        }
    }

    fn resolver(&self) -> Result<Arc<dyn WorkerResolver<AnyDispatchStore>>, crate::HostError> {
        let resolver = crate::host::session_worker_resolver(&self.assembly, &self.dispatch)?;
        Ok(Arc::new(resolver))
    }

    /// Spawn the process dispatch pool, once, when this process executes Runs
    /// and its dispatch authority is bound. Idempotent.
    fn start_local_pool(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.dispatch
            .ensure_pool(self.assembly.authority.dispatch_binding(), || {
                self.resolver().map_err(|error| {
                    awaken_dispatch_execution::session_execution::execution_error(error.message)
                })
            })
    }

    /// Stop local durable admission, await every already-claimed Run, then join
    /// auxiliary work created by those Runs.  The ordering is significant: a
    /// terminal Run may enqueue Memory extraction immediately before releasing
    /// its pool capacity, so draining auxiliary work first can miss that task.
    async fn drain(&self, timeout: std::time::Duration) -> bool {
        let deadline = awaken_time::mono::MonoDeadline::after(timeout);
        let Some(_claimed_runs) = self.dispatch.drain_claimed_runs(&deadline).await else {
            return false;
        };
        let remaining = deadline.remaining();
        !remaining.is_zero() && self.resources.memory.drain(remaining).await
    }
}

impl CoordinatorRoles {
    /// The Coordinator role set over `host`'s components.
    #[must_use]
    pub fn of(host: &crate::SharedHost) -> Self {
        Self {
            session_execution: host.session_execution_roles(),
            process: ProcessOperations::of(host),
        }
    }

    /// The Session application's executor roles, each served by its owner.
    #[must_use]
    pub fn session_execution(&self) -> awaken_session_execution_contract::SessionExecutionRoles {
        self.session_execution.clone()
    }

    /// This process's dispatch: admission, co-located pool, reconciliation.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<ProcessDispatch> {
        &self.process.dispatch
    }

    /// Spawn the co-located pool when the selected topology runs one; a
    /// coordinator-only process stays admission-only. Idempotent.
    pub fn start_local_pool(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.process.start_local_pool()
    }

    /// Start the terminal reconciliation loop when this process owns durable
    /// dispatch and committed truth but runs no pool. Returns whether this call
    /// started it.
    pub fn start_terminal_reconciliation(&self) -> bool {
        let authority = self.process.assembly.authority.as_ref();
        self.process.dispatch.ensure_terminal_reconciliation(
            authority.scans_committed_terminals(),
            crate::host::capability::local_dispatch_owner(authority),
            || self.process.resolver().ok(),
        )
    }

    /// Drain claimed Runs, then the auxiliary work they created, by `timeout`.
    pub async fn drain(&self, timeout: std::time::Duration) -> bool {
        self.process.drain(timeout).await
    }
}

impl WorkerRoles {
    /// The Worker role set over `host`'s components.
    #[must_use]
    pub fn of(host: &crate::SharedHost) -> Self {
        Self {
            process: ProcessOperations::of(host),
        }
    }

    /// This Worker's dispatch: pool, admission and readiness.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<ProcessDispatch> {
        &self.process.dispatch
    }

    /// Session context assembly: realization renewal, revocation and
    /// preparation the Worker lifecycle drives.
    #[must_use]
    pub fn run_assembly(&self) -> &Arc<crate::RunAssembly> {
        &self.process.assembly
    }

    /// Session Run delivery: the resident Runs a revocation interrupts.
    #[must_use]
    pub fn session_runs(&self) -> &Arc<crate::SessionRuns> {
        &self.process.runs
    }

    /// The warm Environment capacity of this Worker's providers.
    #[must_use]
    pub fn environment_capacity(&self) -> &crate::environment_providers::EnvironmentProviders {
        self.process
            .assembly
            .realization_executor
            .environments()
            .providers()
    }

    /// The process wall clock this Worker drives its claims on.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.process.assembly.clock()
    }

    /// Spawn the Worker's dispatch pool once its upstream authority is bound.
    pub fn start_pool(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.process.start_local_pool()
    }

    /// Drain claimed Runs, then the auxiliary work they created, by `timeout`.
    pub async fn drain(&self, timeout: std::time::Duration) -> bool {
        self.process.drain(timeout).await
    }
}
