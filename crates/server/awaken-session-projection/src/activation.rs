//! The terminal fact of a durable Session activation failure.

use awaken_agent_contract::RunLifecycleEvent;
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_session_state_contract::{PersistedSession, SessionExecutionState};

use crate::{FactKey, FactKind, FactProvenance, FactRole, ProjectionError, TimelineFact};

/// Derive the public terminal error from the Session root's one durable
/// activation-failure fact. A failure whose source Run already ended in an
/// error on the root Thread is published by that Run's own lifecycle. A
/// missing or empty cause violates the application invariant and fails the
/// projection closed instead of inventing public bytes.
///
/// Consumer: the protocol adapter's terminal projection.
pub fn activation_failure_fact(
    persisted: &PersistedSession,
    lifecycle: &[RunLifecycleEvent],
) -> Result<Option<TimelineFact>, ProjectionError> {
    if persisted.execution != SessionExecutionState::ActivationFailed {
        return Ok(None);
    }
    if persisted
        .realization_progress
        .failure_source_run_id
        .as_deref()
        .is_some_and(|source_run_id| {
            lifecycle.iter().any(|event| {
                event.thread_id.0 == persisted.session_id
                    && &event.run_id == source_run_id
                    && matches!(event.state, RunState::Ended(EndCause::Error(_)))
            })
        })
    {
        return Ok(None);
    }
    let message = persisted
        .realization_progress
        .last_error
        .as_deref()
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| {
            ProjectionError::corrupt(
                "activation_failed Session has no durable realization failure cause",
            )
        })?;
    Ok(Some(TimelineFact::new(
        FactKey::new(
            persisted.session_id.as_str(),
            FactRole::ActivationFailedError,
            FactProvenance::ParentTerminal,
        ),
        FactKind::ActivationFailed {
            message: message.to_string(),
        },
    )))
}
