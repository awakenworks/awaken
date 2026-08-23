//! Projection between committed Awaken Runs and remote A2A task boundaries.

use awaken_agent_contract::agent::awaiting::{AwaitTarget, RemoteInputReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result};
use awaken_runtime_contract::resume::{PermissionDecision, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

/// The Run's prompt: the concatenated text of the activation's input.
pub(super) fn prompt_of(input: &[Message]) -> String {
    input
        .iter()
        .map(Message::text_content)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The Agent's reply from a returned task: its durable artifacts, else the
/// terminal status message, else the last history message.
pub(super) fn task_reply(task: &Task) -> String {
    let artifacts: Vec<String> = task
        .artifacts
        .iter()
        .map(|artifact| artifact.text())
        .filter(|text| !text.is_empty())
        .collect();
    if !artifacts.is_empty() {
        return artifacts.join("\n");
    }
    if let Some(message) = &task.status.message {
        let text = message.text();
        if !text.is_empty() {
            return text;
        }
    }
    task.history
        .last()
        .map(|message| message.text())
        .unwrap_or_default()
}

/// Mint an A2A response through the same committed Run/Step identity owner as
/// the native Runtime. The committed reader is optional only for a fresh direct
/// Run; every durable resume supplies it and advances from the authoritative
/// transcript prefix.
pub(super) fn assistant_message(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    text: impl Into<String>,
) -> Message {
    let step = context.reader.as_ref().map_or(0, |reader| {
        awaken_agent_contract::agent::message::next_assistant_step(
            &reader.committed_messages(&activation.thread_id),
            &activation.run_id,
        )
    });
    Message::text(
        MessageId::assistant(&activation.run_id, step),
        Role::Assistant,
        text,
    )
}

/// Derive the Run end from an A2A task at a lifecycle boundary. Pollable states
/// remain indeterminate until the shared task driver reaches a boundary.
pub(super) fn end_cause_of(state: &TaskState) -> EndCause {
    match state {
        TaskState::Completed => EndCause::NaturalEnd,
        TaskState::Failed => EndCause::Error(Failure::Inference {
            code: "a2a_task_failed".to_string(),
            message: "remote A2A task ended in the failed state".to_string(),
        }),
        TaskState::Canceled => EndCause::Cancelled,
        TaskState::Rejected => EndCause::Error(Failure::Inference {
            code: "a2a_task_rejected".to_string(),
            message: "remote A2A task ended in the rejected state".to_string(),
        }),
        TaskState::Submitted
        | TaskState::Working
        | TaskState::InputRequired
        | TaskState::AuthRequired
        | TaskState::Unknown => EndCause::Indeterminate,
    }
}

pub(super) fn resume_text(result: &ResumeResult) -> Result<String> {
    match result {
        ResumeResult::ToolResult(output) => Ok(output.text()),
        ResumeResult::Input(text) => Ok(text.clone()),
        ResumeResult::Permission(PermissionDecision::Allow { note }) => {
            Ok(note.clone().unwrap_or_else(|| "allow".to_string()))
        }
        ResumeResult::Permission(PermissionDecision::Deny { reason }) => {
            Ok(reason.clone().unwrap_or_else(|| "deny".to_string()))
        }
        ResumeResult::Continue => Err(Error::Execution(
            "A2A continuation requires a committed input or decision boundary".to_string(),
        )),
    }
}

pub(super) fn awaiting_ticket(activation: &RunActivation, task: &Task) -> ResumeTicket {
    ResumeTicket::new(
        format!("a2a:{}:{:?}", task.id, task.status.state),
        activation.run_id.clone(),
        activation.thread_id.clone(),
        &activation.snapshot.id.0,
        &activation.snapshot.resolved_spec.catalog_fingerprint.0,
        AwaitTarget::RemoteInput {
            reason: match task.status.state {
                TaskState::InputRequired => RemoteInputReason::UserInput,
                TaskState::AuthRequired => RemoteInputReason::ExternalEvent,
                _ => unreachable!("only input/auth-required tasks await"),
            },
            call_id: task.id.clone(),
        },
    )
    .with_delegation_origin(activation.delegation_origin.clone())
    .with_data_subject(
        activation
            .data_subject_id
            .as_ref()
            .map(|subject| subject.0.clone()),
    )
}
