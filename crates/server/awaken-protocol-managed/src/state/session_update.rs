//! Managed wire projection around the canonical Session update command.

use super::*;

impl ManagedState {
    /// MCP projection recovery entry point. It consumes the same canonical
    /// repository recovery index as Resource activation, then drives only the
    /// MCP aggregate state machine through the application owner.
    pub async fn reconcile_session_realizations(&self) -> usize {
        let report = self.application.reconcile_session_realizations().await;
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

    fn project_update_outcome(
        &self,
        outcome: &awaken_session_application::SessionUpdateOutcome,
    ) -> Result<(), StateError> {
        self.publish_persisted_session(&outcome.session)?;
        if !outcome.command_applied || !outcome.changes.any() {
            return Ok(());
        }
        let event_id = self.next_event_id();
        let predecessor = outcome
            .session
            .event_batches
            .iter()
            .rev()
            .flat_map(|batch| batch.events.iter().rev())
            .next()
            .map(|entry| {
                durable_inbound_event_id(&outcome.session.session_id, entry.event.operation_id())
            });
        self.publish_projection_update(&outcome.session.session_id, |record| {
            let event = Event {
                id: event_id.clone(),
                kind: OutboundKind::SessionUpdated {
                    title: outcome.changes.title.then(|| record.session.title.clone()),
                    metadata: if outcome.changes.metadata {
                        record.session.metadata.clone()
                    } else {
                        Default::default()
                    },
                    agent: outcome
                        .changes
                        .agent()
                        .then(|| record.session.agent.clone()),
                    budget: outcome
                        .changes
                        .budget
                        .then(|| record.session.budget.clone()),
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            };
            if let Some(predecessor) = predecessor.as_ref() {
                record
                    .overlay
                    .anchors
                    .insert(event.id.clone(), predecessor.clone());
                if record
                    .events
                    .iter()
                    .any(|candidate| candidate.id == *predecessor)
                {
                    record.events.push(event);
                } else {
                    record
                        .overlay
                        .pending_events
                        .push((event, predecessor.clone()));
                }
            } else {
                record.events.push(event);
            }
            Ok(())
        })
    }

    /// Drive the protocol-neutral update and project its committed result into
    /// the disposable Managed record/event cache.
    pub(crate) async fn update_session(
        &self,
        id: &str,
        command: awaken_session_application::SessionUpdateCommand,
    ) -> Result<(Session, awaken_session_contract::SessionRevision), StateError> {
        let outcome = match self.application.update_session(id, command).await {
            Ok(outcome) => outcome,
            Err(awaken_session_application::SessionUpdateError::ProjectionAfterCommit {
                outcome,
                source,
            }) => {
                self.project_update_outcome(&outcome)?;
                return Err(StateError::Run(source));
            }
            Err(error) => return Err(Self::map_update_error(error)),
        };
        self.project_update_outcome(&outcome)?;
        Ok((self.get_session(id)?, outcome.command_revision))
    }
}
