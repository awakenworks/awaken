// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the Session coordination component. It admits root and child Runs through
// @formal reason: the canonical dispatch owner, validates replies against committed tickets, interrupts and
// @formal reason: archives coordinated Threads; this is a product-requirement classification, not a proof.
//! [`ChildRunCoordination`] — coordinates a Session's root and child Runs.
//!
//! It reserves and activates Session Runs, admits coordinated child Runs and
//! child reports, validates and delivers tool replies against the committed
//! ticket, and interrupts and archives coordinated Threads. The Session
//! application selects coordination reads directly from `CoordinatedThreads`. Runs are
//! delivered through [`crate::SessionRuns`]; committed truth is read through
//! [`CommittedReads`](crate::CommittedReads).

mod roles;

use std::sync::Arc;

/// Session coordination for one process. Consumers: the Session application's
/// `CoordinationRuntime` role. Its read role selects the original shared Threads.
///
/// ```compile_fail,E0277
/// use std::sync::Arc;
/// use awaken_session_executor::ChildRunCoordination;
/// use awaken_session_execution_contract::SessionCoordinationReads;
/// fn duplicate_read_role(commands: Arc<ChildRunCoordination>) -> Arc<dyn SessionCoordinationReads> {
///     commands
/// }
/// ```
pub struct ChildRunCoordination {
    pub(crate) session_runs: Arc<crate::SessionRuns>,
    pub(crate) run_assembly: Arc<crate::RunAssembly>,
    pub(crate) thread_reads: Arc<crate::CommittedReads>,
    pub(crate) capabilities: Arc<crate::RunCapabilities>,
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
    pub(crate) realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    pub(crate) hub: Arc<crate::ThreadEventHub>,
}

impl ChildRunCoordination {
    /// Coordination over the Runs `session_runs` delivers.
    pub(crate) fn new(session_runs: Arc<crate::SessionRuns>) -> Self {
        let run_assembly = session_runs.run_assembly.clone();
        Self {
            thread_reads: session_runs.thread_reads.clone(),
            capabilities: session_runs.capabilities.clone(),
            installed_sessions: session_runs.installed_sessions.clone(),
            realization_executor: run_assembly.realization_executor.clone(),
            hub: session_runs.hub.clone(),
            run_assembly,
            session_runs,
        }
    }

    /// Read side of the sole durable dispatch authority. Composition supplies
    /// it to the Session application without exposing a second queue.
    pub fn dispatch_observation_source(
        &self,
    ) -> Result<
        Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>,
        crate::AgentCoordinationInstallError,
    > {
        self.coordinator_dispatch()
            .map(|source| source as Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>)
            .map_err(|_| crate::AgentCoordinationInstallError::DispatchObservationUnavailable)
    }

    /// The Agent coordination of this process's Sessions.
    pub(crate) fn threads(&self) -> &awaken_session_run_delivery::CoordinatedThreads {
        &self.session_runs.threads
    }

    /// The Coordinator dispatch owner Session Runs are admitted through.
    pub fn coordinator_dispatch(
        &self,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>, crate::SessionExecutionError>
    {
        self.session_runs.coordinator_dispatch()
    }
}
