// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the Session coordination component. It admits root and child Runs through
// @formal reason: the canonical dispatch owner, validates replies against committed tickets, interrupts and
// @formal reason: archives coordinated Threads; this is a product-requirement classification, not a proof.
//! [`ChildRunCoordination`] — coordinates a Session's root and child Runs.
//!
//! It reserves and activates Session Runs, admits coordinated child Runs and
//! child reports, validates and delivers tool replies against the committed
//! ticket, interrupts and archives coordinated Threads, and answers the
//! coordination reads the Session application validates against. Runs are
//! delivered through [`crate::SessionRuns`]; committed truth is read through
//! [`CommittedReads`](crate::CommittedReads).

mod roles;

use std::sync::Arc;

/// Session coordination for one process. Consumers: the Session application's
/// `CoordinationRuntime` and `SessionCoordinationReads` roles.
pub struct ChildRunCoordination {
    pub(crate) session_runs: Arc<crate::SessionRuns>,
    pub(crate) run_assembly: Arc<crate::RunAssembly>,
    pub(crate) thread_reads: Arc<crate::CommittedReads>,
    pub(crate) capabilities: Arc<crate::RunCapabilities>,
    pub(crate) installed_sessions: Arc<crate::InstalledSessions>,
    pub(crate) realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    pub(crate) authority: Arc<dyn crate::SessionAuthority>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
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
            authority: session_runs.authority.clone(),
            clock: session_runs.clock.clone(),
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

    /// The Coordinator dispatch owner Session Runs are admitted through.
    pub fn coordinator_dispatch(
        &self,
    ) -> Result<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>, crate::SessionExecutionError>
    {
        self.session_runs.coordinator_dispatch()
    }
}
