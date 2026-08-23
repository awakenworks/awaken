//! Atomic A2A Run commits and terminal-observer delivery.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{CommittedTerminalRun, deliver_committed_terminal};

use super::task_projection::assistant_message;
use super::task_state::clear_task_reference_state;

pub(super) async fn finish_invalid_task_reference(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    error: Error,
) -> Result<RunState> {
    let message = error.to_string();
    finish_terminal(
        context,
        activation,
        vec![assistant_message(context, activation, message.clone())],
        EndCause::Error(Failure::Inference {
            code: "a2a_durable_state_invalid".to_string(),
            message,
        }),
    )
    .await
}

pub(super) async fn finish_terminal(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    messages: Vec<Message>,
    cause: EndCause,
) -> Result<RunState> {
    commit_boundary(
        context,
        activation,
        RunDisposition::ended(activation.run_id.clone(), cause.clone()),
        messages,
        vec![clear_task_reference_state()],
    )
    .await?;
    Ok(RunState::Ended(cause))
}

/// Commit one remote lifecycle boundary through the same atomic Run boundary as
/// Native and ACP. The task reference is therefore never ahead of its Run state.
pub(super) async fn commit_boundary(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    disposition: RunDisposition,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
) -> Result<()> {
    let coordinator = context
        .commit
        .as_ref()
        .ok_or_else(|| Error::Commit("A2A execution requires a CommitCoordinator".to_string()))?;
    let terminal_cause = match disposition.state() {
        RunState::Ended(cause) => Some(cause),
        RunState::Running | RunState::Awaiting => None,
    };
    awaken_agent_contract::thread::commit::commit_run(
        coordinator.as_ref(),
        &activation.thread_id,
        disposition,
        messages,
        state,
    )
    .await
    .map_err(|error| Error::Commit(error.to_string()))?;

    if let Some(cause) = terminal_cause {
        let terminal = CommittedTerminalRun {
            run_id: activation.run_id.clone(),
            thread_id: activation.thread_id.clone(),
            cause,
        };
        // Observer failures remain isolated from the committed remote Run and
        // recovery may redeliver the same terminal boundary.
        let _ = deliver_committed_terminal(&context.terminal_observers, &terminal).await;
    }
    Ok(())
}
