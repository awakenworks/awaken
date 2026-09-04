//! Terminal Managed projection for durable Session activation failures.

use super::*;

impl ManagedState {
    /// Derive the public terminal error from the Session root's one durable
    /// activation-failure fact. A missing/empty cause violates the application
    /// invariant and fails projection closed instead of inventing public bytes.
    pub(super) fn append_activation_failure_error_projection(
        &self,
        record: &mut SessionRecord,
        persisted: &awaken_session_contract::PersistedSession,
        lifecycle_events: &[RunLifecycleEvent],
    ) -> Result<(), StateError> {
        if persisted.execution != awaken_session_contract::SessionExecutionState::ActivationFailed {
            return Ok(());
        }
        if persisted
            .realization_progress
            .failure_source_run_id
            .as_deref()
            .is_some_and(|source_run_id| {
                lifecycle_events.iter().any(|event| {
                    event.thread_id.0 == persisted.session_id
                        && &event.run_id == source_run_id
                        && matches!(
                            event.state,
                            awaken_agent_contract::agent::run::RunState::Ended(
                                awaken_agent_contract::agent::run::EndCause::Error(_)
                            )
                        )
                })
            })
        {
            return Ok(());
        }
        let message = persisted
            .realization_progress
            .last_error
            .as_deref()
            .filter(|message| !message.trim().is_empty())
            .ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "activation_failed Session has no durable realization failure cause",
                ))
            })?;
        let id = managed_multiagent_event_id(
            &record.session.id,
            &record.session.id,
            "activation-failed-error",
            ManagedMultiagentEventProvenance::ParentTerminal,
        );
        if !record.events.iter().any(|event| event.id == id) {
            record.events.push(Event {
                id,
                kind: OutboundKind::SessionError {
                    error: SessionError::classify(persisted.execution.as_str(), message),
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        Ok(())
    }
}
