// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Execution and disposition transitions use the controlled root's activity and terminal
// @formal reason: fences. The application must commit this candidate through its original CAS; this
// @formal reason: product classification does not assert Worker liveness or physical termination.
use super::{
    SessionArchiveWithRepositoryPublicationError, SessionDispositionTransitionError,
    SessionExecutionState, SessionExecutionTransitionError, SessionLifecycleOperations,
};

fn activity_transition_admits(
    root: &super::SessionAggregate,
    next: SessionExecutionState,
) -> Result<(), SessionExecutionTransitionError> {
    if next == SessionExecutionState::Idle && root.has_active_activities() {
        Err(SessionExecutionTransitionError {
            from: root.execution,
            to: next,
        })
    } else {
        Ok(())
    }
}

impl SessionLifecycleOperations<'_> {
    /// Apply the canonical PersistedSession::transition_execution law.
    pub fn transition_execution(
        &mut self,
        next: SessionExecutionState,
    ) -> Result<bool, SessionExecutionTransitionError> {
        activity_transition_admits(self.aggregate, next)?;
        if self.aggregate.state.running_interval.is_some() && next != SessionExecutionState::Running
        {
            return Err(SessionExecutionTransitionError {
                from: self.aggregate.state.execution,
                to: next,
            });
        }
        self.aggregate.state.transition_execution(next)
    }

    /// Transition and close any Running interval as one command.
    pub fn transition_execution_at(
        &mut self,
        next: SessionExecutionState,
        now: u64,
    ) -> Result<bool, SessionExecutionTransitionError> {
        activity_transition_admits(self.aggregate, next)?;
        let changed = self.aggregate.state.transition_execution(next)?;
        if next != SessionExecutionState::Running {
            self.aggregate.state.close_runtime_interval(now);
        }
        Ok(changed)
    }

    /// Apply the canonical PersistedSession::archive law.
    pub fn archive(
        &mut self,
        archived_at: impl Into<String>,
        archived_at_unix_ms: u64,
    ) -> Result<bool, SessionDispositionTransitionError> {
        let changed = self
            .aggregate
            .state
            .archive(archived_at, archived_at_unix_ms)?;
        if changed {
            self.aggregate
                .state
                .close_runtime_interval(archived_at_unix_ms);
        }
        Ok(changed)
    }

    /// Apply the canonical PersistedSession::request_delete law.
    pub fn request_delete(
        &mut self,
        owner_scope: &str,
        requested_at_unix_s: i64,
        now_unix_ms: u64,
    ) -> Result<Option<crate::state::ManagedLifecycleFact>, crate::state::SessionCleanupError> {
        let fact = self
            .aggregate
            .state
            .request_delete(owner_scope, requested_at_unix_s)?;
        if fact.is_some() {
            self.aggregate.state.close_runtime_interval(now_unix_ms);
        }
        Ok(fact)
    }

    /// Apply the canonical PersistedSession::record_repository_publication_rejection law.
    pub fn record_repository_publication_rejection(
        &mut self,
        rejection: crate::state::SessionRepositoryPublicationRejection,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        self.aggregate
            .state
            .record_repository_publication_rejection(rejection)
    }

    /// Apply the canonical PersistedSession::archive_with_repository_publication law.
    pub fn archive_with_repository_publication(
        &mut self,
        archived_at: impl Into<String>,
        archived_at_unix_ms: Option<u64>,
        intent: crate::state::SessionRepositoryPublicationIntent,
    ) -> Result<bool, SessionArchiveWithRepositoryPublicationError> {
        let changed = self.aggregate.state.archive_with_repository_publication(
            archived_at,
            archived_at_unix_ms,
            intent,
        )?;
        if changed && let Some(now) = archived_at_unix_ms {
            self.aggregate.state.close_runtime_interval(now);
        }
        Ok(changed)
    }
}
