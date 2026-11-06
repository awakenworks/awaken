// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The existing Session aggregate owns atomic activity admission and retained raw
// @formal reason: Run identities before execution. Its application commits this candidate through the
// @formal reason: original root CAS; scalar-plan proofs retain their stated scope, while new history,
// @formal reason: replay and legacy preservation are covered by adjacent native tests, not a full-stop proof.
use super::{
    SessionActivityOperations, SessionEventBatchError, SessionEventDispatchSettlementEffect,
    SessionEventProjectionAnchor, SessionExecutionState, SessionExecutionTransitionError,
    SessionRuntimeInterval,
};
use awaken_lease_contract::fence::Epoch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionActivityAdmissionPlan {
    Open(u64),
    Terminal,
    NotReady,
    BudgetReached,
    EpochExhausted,
    Conflict,
}

pub const fn session_activity_open_plan(
    execution: SessionExecutionState,
    current_epoch: u64,
    exact_epoch: Option<u64>,
    allow_external_realization: bool,
    continue_committed_await: bool,
    budget_admits: bool,
) -> SessionActivityAdmissionPlan {
    if execution.is_terminal() {
        return SessionActivityAdmissionPlan::Terminal;
    }
    let ready = execution.admits_activity()
        || (allow_external_realization
            && matches!(
                execution,
                SessionExecutionState::Preparing | SessionExecutionState::Activating
            ));
    if !ready {
        return SessionActivityAdmissionPlan::NotReady;
    }
    if !continue_committed_await && !budget_admits {
        return SessionActivityAdmissionPlan::BudgetReached;
    }
    match exact_epoch {
        Some(epoch) if epoch != 0 && epoch > current_epoch => {
            SessionActivityAdmissionPlan::Open(epoch)
        }
        Some(_) => SessionActivityAdmissionPlan::Conflict,
        None => match current_epoch.checked_add(1) {
            Some(epoch) => SessionActivityAdmissionPlan::Open(epoch),
            None => SessionActivityAdmissionPlan::EpochExhausted,
        },
    }
}

impl SessionActivityOperations<'_> {
    /// Admit all activity coordinates as one validated working-state change.
    /// The application maps the closed plan to its existing transport errors.
    pub fn open(
        &mut self,
        exact_epoch: Option<u64>,
        allow_external: bool,
        continue_await: bool,
        now: u64,
        admitted_after: u64,
        source: Option<(
            &str,
            &awaken_agent_contract::ThreadId,
            &awaken_agent_contract::RunId,
        )>,
    ) -> SessionActivityAdmissionPlan {
        let state = &mut self.aggregate.state;
        if state.is_terminal() {
            return SessionActivityAdmissionPlan::Terminal;
        }
        let plan = session_activity_open_plan(
            state.execution,
            state.activity_epoch,
            exact_epoch,
            allow_external,
            continue_await,
            state.budget.can_admit_model_request(),
        );
        let SessionActivityAdmissionPlan::Open(epoch) = plan else {
            return plan;
        };
        let mut next = state.clone();
        if !next.begin_activity_epoch_at(epoch, source) {
            return SessionActivityAdmissionPlan::Conflict;
        }
        next.environment.mark_active();
        if next.execution == SessionExecutionState::Idle
            && next
                .transition_execution(SessionExecutionState::Running)
                .is_err()
        {
            return SessionActivityAdmissionPlan::NotReady;
        }
        next.begin_runtime_interval(now, admitted_after);
        *state = next;
        plan
    }

    /// Close the final Running activity together with its Environment idle
    /// observation. The mutation compiler derives the exact interval fact.
    pub fn finish_running(
        &mut self,
        now: u64,
    ) -> Result<Option<SessionRuntimeInterval>, SessionExecutionTransitionError> {
        let state = &mut self.aggregate.state;
        if state.has_active_activities() {
            return Err(SessionExecutionTransitionError {
                from: state.execution,
                to: SessionExecutionState::Idle,
            });
        }
        state.transition_execution(SessionExecutionState::Idle)?;
        state.environment.mark_idle(now);
        Ok(state.close_runtime_interval(now))
    }

    /// Publish physical readiness together with the admitted activity's
    /// execution state and its first runtime interval.
    pub fn realization_ready(&mut self, now: u64) -> Result<bool, SessionExecutionTransitionError> {
        let state = &mut self.aggregate.state;
        let initial_ready = !state.has_active_activities()
            && state.activity_epoch == 0
            && state.execution != SessionExecutionState::Idle;
        let mut next = state.clone();
        if next.has_active_activities() {
            if next.execution != SessionExecutionState::Running {
                if next.execution != SessionExecutionState::Idle {
                    next.transition_execution(SessionExecutionState::Idle)?;
                }
                next.transition_execution(SessionExecutionState::Running)?;
            }
            let admitted_after = next.recorded_runtime_commit_cursor();
            next.begin_runtime_interval(now, admitted_after);
        } else if next.execution != SessionExecutionState::Running {
            next.transition_execution(SessionExecutionState::Idle)?;
        }
        *state = next;
        // Readiness proves the selected owner converged: earlier uncounted
        // failures no longer describe a repeating fault.
        state.realization_progress.uncounted_failures = 0;
        Ok(initial_ready)
    }

    /// Apply the canonical PersistedSession::install_initial_event_plan law.
    pub fn install_initial_event_plan(
        &mut self,
        plan: crate::state::SessionInitialEventPlan,
    ) -> Result<(), crate::state::SessionEventBatchError> {
        self.aggregate.state.install_initial_event_plan(plan)
    }

    /// Apply the canonical PersistedSession::settle_activity_epoch law.
    pub fn settle_activity_epoch(&mut self, expected_epoch: impl TryInto<Epoch>) -> Option<bool> {
        self.aggregate
            .state
            .settle_activity_epoch(expected_epoch.try_into().ok()?.get())
    }

    /// Apply the canonical PersistedSession::settle_user_event_dispatch law.
    pub fn settle_user_event_dispatch(
        &mut self,
        run_id: &awaken_agent_contract::agent::run::Id,
        boundary: crate::state::SessionAgentRunBoundary,
        expected_epoch: impl TryInto<Epoch>,
        projection_anchor: SessionEventProjectionAnchor,
        processed_at_unix_ms: u64,
    ) -> Result<SessionEventDispatchSettlementEffect, SessionEventBatchError> {
        self.aggregate.state.settle_user_event_dispatch(
            run_id,
            boundary,
            expected_epoch
                .try_into()
                .map_err(|_| SessionEventBatchError::ProgressMismatch)?
                .get(),
            projection_anchor,
            processed_at_unix_ms,
        )
    }

    /// Apply the canonical PersistedSession::begin_runtime_interval law.
    pub fn begin_runtime_interval(
        &mut self,
        started_at_unix_ms: u64,
        admitted_after_commit_cursor: u64,
    ) -> bool {
        self.aggregate
            .state
            .begin_runtime_interval(started_at_unix_ms, admitted_after_commit_cursor)
    }

    /// Apply the canonical PersistedSession::observe_runtime_interval law.
    pub fn observe_runtime_interval(
        &mut self,
        observation: crate::state::SessionRuntimeIntervalObservation,
    ) -> bool {
        self.aggregate.state.observe_runtime_interval(observation)
    }
}
