// @formal surface exclude=composition_wiring
// @formal owner crates/platform/execution/awaken-dispatch-execution/src/session_worker_resolver.rs
// @formal reason: Constructors retain the Coordinator and Worker roles selected by composition;
// @formal reason: claim resolution and reconciliation remain in the owning resolver operations.
//! The resolver's constructions: a Coordinator resolves no claimed execution
//! and scans committed terminals; a Worker resolves claimed execution; a
//! co-located process does both. Each role is given once, at construction.

use super::{
    AnyDispatchStore, Arc, ClaimedRunPreparation, ClaimedRunWorkers, ClaimedSessionInstallation,
    CommitBoundaryWorkers, Error, RunMcpAttachments, execution_error,
};
use crate::session_reconciliation::TerminalReconciliationState;
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_runtime_contract::auxiliary_agents::RunPublicationClosure;
use awaken_session_execution_contract::ClaimedSessionRealizationControl;

/// Where a claimed Session's frozen projection comes from on this Worker.
pub enum ProjectionAuthority {
    /// A database-less Worker resumes the projection through remote Control.
    RemoteControl(Arc<dyn ClaimedSessionRealizationControl>),
    /// A co-located pool consumes the projection its Session application
    /// already installed in this process.
    CoLocated,
}

/// The roles a Worker uses to resolve claimed Runs into executable Workers.
pub struct ClaimedExecution {
    pub installation: Arc<dyn ClaimedSessionInstallation>,
    pub preparation: Arc<dyn ClaimedRunPreparation>,
    pub runs: Arc<dyn ClaimedRunWorkers>,
    pub mcp: Arc<dyn RunMcpAttachments>,
    pub projection: ProjectionAuthority,
    /// The publication closure every claimed Run's carried publications must match.
    pub publication_closure: RunPublicationClosure<'static>,
}

/// The Coordinator authority the committed-terminal scan claims through.
pub struct CommittedTerminalScan {
    pub dispatch: Arc<AnyCoordinatorDispatch>,
    /// The logical lease owner that claims maintenance work.
    pub lease_owner: String,
}

/// Routes a claimed Run to the Worker that executes it. It holds only the
/// roles its construction was given; the pool's tasks retain no composing
/// process through it.
pub struct SessionWorkerResolver {
    pub(crate) queue: Arc<AnyDispatchStore>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) commits: Arc<dyn CommitBoundaryWorkers>,
    pub(crate) terminal_state: Arc<TerminalReconciliationState>,
    pub(crate) execution: Option<ClaimedExecution>,
    pub(crate) terminals: Option<CommittedTerminalScan>,
}

impl SessionWorkerResolver {
    /// A Coordinator: no claimed execution. It scans committed terminals when
    /// it owns the Coordinator's committed truth.
    pub fn coordinator(
        queue: Arc<AnyDispatchStore>,
        clock: Arc<dyn awaken_time::WallClock>,
        commits: Arc<dyn CommitBoundaryWorkers>,
        terminal_state: Arc<TerminalReconciliationState>,
        terminals: Option<CommittedTerminalScan>,
    ) -> Self {
        Self {
            queue,
            clock,
            commits,
            terminal_state,
            execution: None,
            terminals,
        }
    }

    /// A Worker: claimed execution, and the committed-terminal scan only when
    /// this process also owns the Coordinator's committed truth.
    pub fn worker(
        queue: Arc<AnyDispatchStore>,
        clock: Arc<dyn awaken_time::WallClock>,
        commits: Arc<dyn CommitBoundaryWorkers>,
        terminal_state: Arc<TerminalReconciliationState>,
        execution: ClaimedExecution,
        terminals: Option<CommittedTerminalScan>,
    ) -> Self {
        Self {
            queue,
            clock,
            commits,
            terminal_state,
            execution: Some(execution),
            terminals,
        }
    }

    pub(super) fn execution(&self) -> Result<&ClaimedExecution, Error> {
        self.execution.as_ref().ok_or_else(|| {
            execution_error("coordinator-only Host cannot resolve claimed execution locally")
        })
    }
}
