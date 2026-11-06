//! The terminal fact of a durable Session activation failure.

use awaken_agent_contract::RunLifecycleEvent;
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_session_contract::{PersistedSession, SessionExecutionState};

use crate::{
    DerivedSessionEvent, EventIdentity, FactProvenance, ManagedEventType, ProjectionError,
    SessionEventKind,
};

/// One ownership decision for both the activation error and parent terminal.
/// A sourced failure cannot become a public prefix until that Run has a
/// committed terminal: the Session failure is recorded before the Worker can
/// commit its Run failure, and publishing either parent fact in that gap would
/// require retracting or inserting before an already-issued Event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationFailureProjection {
    NotFailed,
    AwaitingSourceRun,
    Ready { error: Option<DerivedSessionEvent> },
}

/// Derive the terminal projection from one durable Session root and accepted
/// lifecycle prefix. A root source Run's own error replaces the broader
/// Session error; a child or non-error source terminal leaves the Session
/// failure independently visible. An empty required cause fails closed.
///
/// Consumer: the protocol adapter's sole committed-prefix candidate builder.
pub fn activation_failure_projection(
    persisted: &PersistedSession,
    lifecycle: &[RunLifecycleEvent],
) -> Result<ActivationFailureProjection, ProjectionError> {
    if persisted.execution != SessionExecutionState::ActivationFailed {
        return Ok(ActivationFailureProjection::NotFailed);
    }
    let source = persisted.realization_progress().failure_source_run_id();
    let source_terminal = source.and_then(|run_id| {
        lifecycle
            .iter()
            .find(|event| &event.run_id == run_id && matches!(event.state, RunState::Ended(_)))
    });
    if source.is_some() && source_terminal.is_none() {
        return Ok(ActivationFailureProjection::AwaitingSourceRun);
    }
    if source_terminal.is_some_and(|event| {
        event.thread_id.0 == persisted.session_id
            && matches!(event.state, RunState::Ended(EndCause::Error(_)))
    }) {
        return Ok(ActivationFailureProjection::Ready { error: None });
    }
    let message = persisted
        .realization_progress()
        .last_error()
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| {
            ProjectionError::corrupt(
                "activation_failed Session has no durable realization failure cause",
            )
        })?;
    Ok(ActivationFailureProjection::Ready {
        error: Some(DerivedSessionEvent::new(
            EventIdentity::new(
                persisted.session_id.as_str(),
                ManagedEventType::ActivationFailedError,
                FactProvenance::ParentTerminal,
            ),
            SessionEventKind::ActivationFailed {
                message: message.to_string(),
            },
        )),
    })
}
