// @formal surface requirement=runtime.outcomes.outcome_lifecycle
// @formal reason: Ordinary-Run dispatch/observation shares the existing queue and completion owners;
// @formal reason: DispatchPreparation alone lowers frozen inputs and binds the execution format.
//! Ordinary Runs dispatched to registered Workers and observed through committed truth.

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::DispatchEnqueue as _;
use awaken_runtime_contract::execution::{Error as ExecutionError, Result as ExecutionResult};
use awaken_runtime_contract::{RunActivation, RuntimeRunContext};
use awaken_session_types::RunError;
use std::sync::Arc;

use super::SETTLED_RECONCILIATION_INTERVAL;
use crate::session_run_activation::read_settled_phase;
use crate::{SessionCommit, SessionLowering as _, await_completion_state};

/// Runs of one Thread, dispatched to registered Workers and observed until
/// they settle.
pub(super) struct DispatchedRuns<'a> {
    projections: &'a crate::SessionProjections,
    owner: &'a awaken_dispatch_erasure::AnyCoordinatorDispatch,
    completion: &'a Arc<crate::CompletionRegistry>,
    thread: ThreadId,
    commit: Arc<SessionCommit>,
    /// An Outcome's Worker Runs extend the Session's Thread; a Grader Run is a
    /// fresh Run of its own Thread.
    extension: bool,
}

impl<'a> DispatchedRuns<'a> {
    pub(super) fn new(
        projections: &'a crate::SessionProjections,
        owner: &'a awaken_dispatch_erasure::AnyCoordinatorDispatch,
        completion: &'a Arc<crate::CompletionRegistry>,
        thread: ThreadId,
        commit: Arc<SessionCommit>,
        extension: bool,
    ) -> Self {
        Self {
            projections,
            owner,
            completion,
            thread,
            commit,
            extension,
        }
    }

    async fn dispatch(&self, activation: RunActivation) -> Result<RunState, RunError> {
        if activation.thread_id != self.thread {
            return Err(RunError::internal(
                "Run activation does not match its bound Thread",
            ));
        }
        let projections = self.projections;
        let run_id = activation.run_id.clone();
        let (settled, _waiter_guard) = self.completion.register(&run_id, None)?;
        let preparation = projections.prepare(activation);
        let request = if self.extension {
            preparation.extension()
        } else {
            preparation.fresh()
        }?;
        self.owner
            .enqueue(request)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        await_completion_state(settled, SETTLED_RECONCILIATION_INTERVAL, || {
            read_settled_phase(&self.commit, &self.thread, &run_id, None)
        })
        .await
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for DispatchedRuns<'_> {
    async fn execute(
        &self,
        activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.dispatch(activation)
            .await
            .map_err(|error| ExecutionError::Execution(error.to_string()))
    }
}
