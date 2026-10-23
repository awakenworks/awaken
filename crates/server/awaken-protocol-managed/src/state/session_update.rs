// @formal surface obligation=config.generation_is_monotonic
// @formal surface obligation=session_execution.terminal_state_never_reopens
// @formal reason: Session update mutation over revisioned truth.
//! Managed wire projection around the canonical Session update command.

use super::*;

impl ManagedState {
    /// Fixture driver for one MCP realization sweep followed by the wire
    /// refresh of every settled Session. Product composition runs the same
    /// sweep in the Session application's lifecycle supervisor.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn reconcile_session_realizations(&self) -> usize {
        let report = self
            .application
            .realizations()
            .reconcile_session_realizations()
            .await;
        for session in &report.settled {
            if let Err(error) = self.publish_persisted_session(session) {
                tracing::warn!(
                    session = %session.session_id,
                    error = ?error,
                    "Session MCP wire projection refresh remains pending"
                );
            }
        }
        for failure in report.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session MCP reconciliation remains pending"
            );
        }
        report.settled.len()
    }

    fn map_update_error(error: awaken_session_application::SessionUpdateError) -> StateError {
        match error {
            awaken_session_application::SessionUpdateError::NotFound => StateError::NotFound,
            awaken_session_application::SessionUpdateError::NotIdle => {
                StateError::Run(RunError::bad_request(
                    "session agent updates require an idle session; interrupt the active run first",
                ))
            }
            awaken_session_application::SessionUpdateError::NotFrozen => {
                StateError::Run(RunError::classified(
                    "session_not_frozen",
                    "MCP attachments cannot change while the Session baseline is preparing",
                ))
            }
            awaken_session_application::SessionUpdateError::Conflict => StateError::Conflict,
            awaken_session_application::SessionUpdateError::IdempotencyMismatch => {
                StateError::IdempotencyMismatch
            }
            awaken_session_application::SessionUpdateError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionUpdateError::Realization(error) => {
                Self::map_realization_application_error(error)
            }
            awaken_session_application::SessionUpdateError::ProjectionAfterCommit {
                source,
                ..
            } => StateError::Run(source),
            awaken_session_application::SessionUpdateError::Unavailable(message) => {
                StateError::Run(RunError::internal(message))
            }
        }
    }

    async fn project_update_outcome(
        &self,
        outcome: &awaken_session_application::SessionUpdateOutcome,
    ) -> Result<(), StateError> {
        // Committed update facts share the sole warm/cold reducer. Replay also
        // repairs a commit whose disposable publication was lost.
        self.refresh_committed_events(outcome.session.session_id.as_str())
            .await
    }

    /// Drive the protocol-neutral update and project its committed result into
    /// the disposable Managed record/event cache.
    pub(crate) async fn update_session(
        &self,
        id: &str,
        command: awaken_session_application::SessionUpdateCommand,
    ) -> Result<(Session, awaken_session_types::SessionRevision), StateError> {
        let outcome = match self
            .roles
            .updates
            .update_session(&awaken_session_types::SessionId::from(id), command)
            .await
        {
            Ok(outcome) => outcome,
            Err(awaken_session_application::SessionUpdateError::ProjectionAfterCommit {
                outcome,
                source,
            }) => {
                self.project_update_outcome(&outcome).await?;
                return Err(StateError::Run(source));
            }
            Err(error) => return Err(Self::map_update_error(error)),
        };
        self.project_update_outcome(&outcome).await?;
        self.get_session_with_revision(id)
    }
}
