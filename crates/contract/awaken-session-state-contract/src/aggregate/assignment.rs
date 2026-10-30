// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Assignment claim, renewal, expiry and capacity recovery mutate this Session candidate
// @formal reason: under the original epoch/generation laws before root CAS. This classification does
// @formal reason: not extend those laws to external Worker execution or delivery.
use super::realization_lease_generation_authorizes;
use super::{
    McpAttachmentError, SessionAggregateCommandError, SessionAssignmentOperations,
    SessionExecutionState, SessionRealizationLease,
};
use awaken_lease_contract::fence::Epoch;

impl SessionAssignmentOperations<'_> {
    /// Persist the already-admitted initial-preparation request monotonically.
    pub fn request_run_free_preparation(&mut self) -> Result<bool, SessionAggregateCommandError> {
        if self.aggregate.state.is_terminal() || self.aggregate.state.has_active_activities() {
            return Err(SessionAggregateCommandError::PreparationNotReady);
        }
        let previous = self
            .aggregate
            .state
            .realization_progress
            .run_free_initial_preparation;
        self.aggregate
            .state
            .realization_progress
            .run_free_initial_preparation = true;
        Ok(!previous)
    }

    /// Install a new exact successor owner generation. A counted initial
    /// attempt clears its predecessor's failure fields in the same operation.
    pub fn claim(
        &mut self,
        lease: SessionRealizationLease,
        count_initial_attempt: bool,
    ) -> Result<(), SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        let expected_epoch = state
            .realization
            .as_ref()
            .map_or(Some(Epoch::FIRST), |old| {
                Epoch::from_persisted(old.epoch).and_then(Epoch::next)
            });
        let same = state
            .realization
            .as_ref()
            .is_some_and(|old| realization_lease_generation_authorizes(&lease, old));
        if lease.owner.trim().is_empty()
            || lease.runtime_incarnation.trim().is_empty()
            || lease.epoch == 0
            || (same && count_initial_attempt)
            || (same
                && state
                    .realization
                    .as_ref()
                    .is_some_and(|old| lease.expires_at_unix_ms < old.expires_at_unix_ms))
        {
            return Err(SessionAggregateCommandError::InvalidLeaseGeneration);
        }
        if !same && expected_epoch.map(Epoch::get) != Some(lease.epoch) {
            return Err(SessionAggregateCommandError::InvalidLeaseGeneration);
        }
        if count_initial_attempt
            && !matches!(
                state.execution,
                SessionExecutionState::Preparing | SessionExecutionState::Activating
            )
        {
            return Err(SessionAggregateCommandError::PreparationNotReady);
        }
        let attempts = if count_initial_attempt {
            state
                .realization_progress
                .attempts
                .checked_add(1)
                .ok_or(SessionAggregateCommandError::AttemptCounterExhausted)?
        } else {
            state.realization_progress.attempts
        };
        if count_initial_attempt {
            state.realization_progress.attempts = attempts;
            state.realization_progress.last_error = None;
            state.realization_progress.failed_at_unix_ms = None;
            state.realization_progress.failure_source_run_id = None;
            state.realization_progress.uncounted_failures = 0;
        }
        state.realization = Some(lease);
        Ok(())
    }

    /// Renew only the selected generation and never regress its expiry.
    pub fn renew(
        &mut self,
        lease: SessionRealizationLease,
    ) -> Result<(), SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        let Some(current) = &state.realization else {
            return Err(SessionAggregateCommandError::MissingIncumbentLease);
        };
        if !realization_lease_generation_authorizes(&lease, current)
            || lease.expires_at_unix_ms < current.expires_at_unix_ms
        {
            return Err(SessionAggregateCommandError::InvalidLeaseRenewal);
        }
        if !state.is_terminal() {
            let mut mcp = state.mcp.clone();
            mcp.extend_active_realization_leases(
                &lease.runtime_incarnation,
                lease.epoch,
                lease.expires_at_unix_ms,
            )
            .map_err(|error| SessionAggregateCommandError::ProjectionLease(error.to_string()))?;
            state.mcp = mcp;
        }
        state.realization = Some(lease);
        Ok(())
    }

    /// Extend projections from this root's current selected lease, without
    /// accepting caller-authored owner, epoch, or expiry coordinates.
    pub fn refresh_projection_leases(&mut self) -> Result<(), McpAttachmentError> {
        let state = &mut self.aggregate.state;
        let lease = state
            .realization
            .as_ref()
            .ok_or(McpAttachmentError::StaleRealizationClaim)?;
        let mut next = state.mcp.clone();
        next.extend_active_realization_leases(
            &lease.runtime_incarnation,
            lease.epoch,
            lease.expires_at_unix_ms,
        )?;
        state.mcp = next;
        Ok(())
    }

    /// Record a failed effect without changing the selected lease identity.
    pub fn record_failure(
        &mut self,
        reason: String,
        now: u64,
        source_run_id: Option<awaken_agent_contract::agent::run::Id>,
    ) {
        let progress = &mut self.aggregate.state.realization_progress;
        progress.last_error = Some(reason);
        progress.failed_at_unix_ms = Some(now);
        progress.failure_source_run_id = source_run_id.map(Box::new);
    }

    /// Count one failed effect of an assignment that counted no initial
    /// attempt, so Run-free preparation and Running/Rescheduling recovery
    /// are bounded by the same realization budget as initial realization.
    ///
    /// | execution                 | not-ready | effect                 |
    /// |---------------------------|-----------|------------------------|
    /// | Preparing / Activating    | any       | unchanged (claim counted) |
    /// | any other                 | true      | unchanged (deferral)   |
    /// | any other                 | false     | count + 1              |
    ///
    /// Returns the resulting count; overflow is refused, never wrapped.
    pub fn note_uncounted_failure(
        &mut self,
        not_ready: bool,
    ) -> Result<u32, SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        let counted = matches!(
            state.execution,
            SessionExecutionState::Preparing | SessionExecutionState::Activating
        );
        let progress = &mut state.realization_progress;
        if counted || not_ready {
            return Ok(progress.uncounted_failures);
        }
        progress.uncounted_failures = progress
            .uncounted_failures
            .checked_add(1)
            .ok_or(SessionAggregateCommandError::AttemptCounterExhausted)?;
        Ok(progress.uncounted_failures)
    }

    /// Refund exactly one counted capacity refusal, without allowing underflow.
    pub fn refund_capacity_attempt(&mut self) -> Result<(), SessionAggregateCommandError> {
        let progress = &mut self.aggregate.state.realization_progress;
        progress.attempts = progress
            .attempts
            .checked_sub(1)
            .ok_or(SessionAggregateCommandError::MissingCapacityAttempt)?;
        Ok(())
    }

    /// Close the current disposable assignment while retaining its generation.
    pub fn expire(&mut self) {
        if let Some(lease) = &mut self.aggregate.state.realization {
            lease.expires_at_unix_ms = 0;
        }
    }

    /// Compensate only the claimant's exact generation after authority withdrawal.
    pub fn restore_predecessor(
        &mut self,
        attempted: &SessionRealizationLease,
        predecessor: Option<SessionRealizationLease>,
    ) -> Result<(), SessionAggregateCommandError> {
        let state = &mut self.aggregate.state;
        if state.realization.as_ref() != Some(attempted) {
            return Err(SessionAggregateCommandError::StaleCompensation);
        }
        if predecessor
            .as_ref()
            .map_or(Some(Epoch::FIRST), |lease| {
                Epoch::from_persisted(lease.epoch).and_then(Epoch::next)
            })
            .map(Epoch::get)
            != Some(attempted.epoch)
        {
            return Err(SessionAggregateCommandError::InvalidPredecessor);
        }
        state.realization = predecessor;
        Ok(())
    }
}
