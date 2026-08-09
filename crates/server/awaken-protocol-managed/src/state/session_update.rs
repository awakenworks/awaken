//! Managed wire projection around the canonical Session update command.

use super::*;

impl ManagedState {
    #[must_use]
    pub(crate) fn update_operation_id(id: &str, idempotency_key: &str) -> String {
        awaken_session_application::SessionApplication::update_operation_id(id, idempotency_key)
    }

    /// MCP projection recovery entry point. It consumes the same canonical
    /// repository recovery index as Resource activation, then drives only the
    /// MCP aggregate state machine through the application owner.
    pub async fn reconcile_mcp_attachments(&self) -> usize {
        let report = self.application.reconcile_mcp_attachments().await;
        for session in &report.settled {
            if let Err(error) = self.refresh_cached_projection(session) {
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

    fn project_update_outcome(
        &self,
        outcome: &awaken_session_application::SessionUpdateOutcome,
        title_in_request: bool,
    ) -> Result<(), StateError> {
        self.refresh_cached_projection(&outcome.session)?;
        if !outcome.command_applied || !outcome.changes.any() {
            return Ok(());
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions
            .get_mut(&outcome.session.session_id)
            .ok_or(StateError::NotFound)?;
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionUpdated {
                title: title_in_request
                    .then(|| record.session.title.clone())
                    .flatten(),
                metadata: record.session.metadata.clone(),
                agent: outcome
                    .changes
                    .agent()
                    .then(|| record.session.agent.clone()),
            },
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        Ok(())
    }

    /// Drive the protocol-neutral update and project its committed result into
    /// the disposable Managed record/event cache.
    pub(crate) async fn update_session(
        &self,
        id: &str,
        command: awaken_session_application::SessionUpdateCommand,
    ) -> Result<(Session, awaken_session_contract::SessionRevision), StateError> {
        let title_in_request = command.title.is_some();
        let outcome = match self.application.update_session(id, command).await {
            Ok(outcome) => outcome,
            Err(awaken_session_application::SessionUpdateError::ProjectionAfterCommit {
                outcome,
                source,
            }) => {
                self.project_update_outcome(&outcome, title_in_request)?;
                return Err(StateError::Run(source));
            }
            Err(error) => return Err(Self::map_update_error(error)),
        };
        self.project_update_outcome(&outcome, title_in_request)?;
        Ok((self.get_session(id)?, outcome.command_revision))
    }
}
