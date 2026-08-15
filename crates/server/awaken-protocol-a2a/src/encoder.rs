//! Project a committed step (and thread history) into an A2A `Task`.
//!
//! A2A is a snapshot protocol: `message:send` returns the whole `Task` — its
//! lifecycle `status` plus the conversation `history`. This module folds committed
//! neutral `Message`s into A2A `Message`s and derives the `TaskState` from the step
//! outcome (awaiting → input-required; step-budget exhausted → failed; else
//! completed).

use awaken_agent_contract::agent::message::{Message as AgentMessage, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};

use awaken_session_contract::{StepOutcome, blocks_text};

use crate::types::{Message, MessageRole, Part, Task, TaskState, TaskStatus};

pub(crate) fn working_task(task_id: &str, thread: &str) -> Task {
    Task {
        kind: crate::types::TaskKind::Task,
        id: task_id.to_string(),
        context_id: thread.to_string(),
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: Some(crate::time::now_rfc3339()),
        },
        history: Vec::new(),
        artifacts: Vec::new(),
        metadata: None,
    }
}

/// Total projection from the authoritative runtime lifecycle to A2A state.
///
/// `Running` and an indeterminate terminal observation remain non-terminal on
/// the wire. Only exact successful, failed, or cancelled causes can strengthen
/// the projection into the corresponding terminal A2A state.
#[must_use]
pub(crate) fn task_state_for_run_state(state: &RunState) -> TaskState {
    match state {
        RunState::Running | RunState::Ended(EndCause::Indeterminate) => TaskState::Working,
        RunState::Awaiting => TaskState::InputRequired,
        RunState::Ended(EndCause::Error(_) | EndCause::MaxSteps | EndCause::Stopped(_)) => {
            TaskState::Failed
        }
        RunState::Ended(EndCause::Cancelled) => TaskState::Canceled,
        RunState::Ended(EndCause::NaturalEnd) => TaskState::Completed,
    }
}

/// Build the `Task` returned for a step on `thread`. `history` is the thread's full
/// committed transcript (post-step); `outcome` classifies the terminal state.
pub fn encode_task(thread: &str, history: &[AgentMessage], outcome: &StepOutcome) -> Task {
    let messages: Vec<Message> = history
        .iter()
        .filter_map(|m| to_a2a_message(thread, m))
        .collect();

    let state = task_state_for_run_state(outcome.state());

    // The status message is what the agent last said. A terminal fault carries its
    // message so the task explains why it failed; when awaiting on a tool, it is the
    // prompt for the input the task now requires.
    let status_message = match outcome.state() {
        RunState::Ended(EndCause::Error(failure)) => Some(Message::agent_text(
            format!("{thread}-status"),
            failure.message(),
        )),
        RunState::Awaiting => outcome
            .pending()
            .map(|pending| {
                Message::agent_text(
                    format!("{thread}-status"),
                    format!("input required for tool `{}`", pending.name),
                )
            })
            .or_else(|| {
                messages
                    .iter()
                    .rev()
                    .find(|message| message.role == MessageRole::Agent)
                    .cloned()
            }),
        _ => messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Agent)
            .cloned(),
    };

    Task {
        kind: crate::types::TaskKind::Task,
        id: format!("task-{thread}"),
        context_id: thread.to_string(),
        status: TaskStatus {
            state,
            message: status_message,
            timestamp: Some(crate::time::now_rfc3339()),
        },
        history: messages,
        artifacts: Vec::new(),
        metadata: None,
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use awaken_agent_contract::agent::run::Failure;

    #[kani::proof]
    fn a2a_task_state_projection_is_total_exact_and_non_strengthening() {
        let tag: u8 = kani::any();
        kani::assume(tag <= 7);
        let state = match tag {
            0 => RunState::Running,
            1 => RunState::Awaiting,
            2 => RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
            3 => RunState::Ended(EndCause::MaxSteps),
            4 => RunState::Ended(EndCause::Stopped(String::new())),
            5 => RunState::Ended(EndCause::Cancelled),
            6 => RunState::Ended(EndCause::Indeterminate),
            _ => RunState::Ended(EndCause::NaturalEnd),
        };

        let projected = task_state_for_run_state(&state);
        match projected {
            TaskState::Working => assert!(matches!(
                state,
                RunState::Running | RunState::Ended(EndCause::Indeterminate)
            )),
            TaskState::InputRequired => assert_eq!(state, RunState::Awaiting),
            TaskState::Failed => assert!(matches!(
                state,
                RunState::Ended(EndCause::Error(_) | EndCause::MaxSteps | EndCause::Stopped(_))
            )),
            TaskState::Canceled => {
                assert_eq!(state, RunState::Ended(EndCause::Cancelled))
            }
            TaskState::Completed => {
                assert_eq!(state, RunState::Ended(EndCause::NaturalEnd))
            }
            TaskState::Submitted
            | TaskState::AuthRequired
            | TaskState::Rejected
            | TaskState::Unknown => unreachable!("runtime projection cannot invent this state"),
        }
    }
}

/// Convert a neutral message to an A2A message. User/assistant map to the A2A
/// roles; system and tool messages are internal and dropped from the A2A history.
fn to_a2a_message(thread: &str, message: &AgentMessage) -> Option<Message> {
    let role = match message.role {
        Role::User => MessageRole::User,
        Role::Assistant => MessageRole::Agent,
        Role::System | Role::Tool => return None,
    };
    let text = blocks_text(&message.content);
    if text.trim().is_empty() {
        return None;
    }
    Some(Message {
        kind: crate::types::MessageKind::Message,
        task_id: Some(format!("task-{thread}")),
        context_id: Some(thread.to_string()),
        message_id: message.id.0.clone(),
        role,
        parts: vec![Part::text(text)],
        extensions: Vec::new(),
        metadata: None,
        reference_task_ids: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::run::{EndCause, Failure};
    use awaken_session_contract::Pending;

    fn msg(id: &str, role: Role, text: &str) -> AgentMessage {
        AgentMessage::text(MessageId(id.into()), role, text)
    }

    #[test]
    fn terminal_failure_yields_a_failed_task_with_the_fault_message() {
        // Cause/effect rule F1: an inference failure in authoritative RunState
        // produces a failed A2A task carrying the same neutral fault message.
        let outcome = StepOutcome::ended(
            Vec::new(),
            EndCause::Error(Failure::Inference {
                code: "inference_failed".into(),
                message: "upstream is down".into(),
            }),
            false,
            false,
        );
        let task = encode_task("th", &[], &outcome);
        assert_eq!(task.status.state, TaskState::Failed);
        let text = task
            .status
            .message
            .as_ref()
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(crate::types::Part::text_value)
                    .collect::<String>()
            })
            .unwrap_or_default();
        assert!(
            text.contains("upstream is down"),
            "status carries the fault: {text}"
        );
    }

    fn outcome(awaiting: bool, pending: Option<Pending>) -> StepOutcome {
        if awaiting {
            StepOutcome::awaiting(Vec::new(), pending, false, false)
        } else {
            StepOutcome::ended(Vec::new(), EndCause::NaturalEnd, false, false)
        }
    }

    #[test]
    fn completed_task_carries_history_and_last_agent_status() {
        let history = [
            msg("u1", Role::User, "hi"),
            msg("a1", Role::Assistant, "hello there"),
            msg("t1", Role::Tool, "tool noise"),
        ];
        let task = encode_task("t", &history, &outcome(false, None));
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(task.context_id, "t");
        // Tool/system messages are not part of the A2A history.
        assert_eq!(task.history.len(), 2);
        assert_eq!(task.status.message.unwrap().text(), "hello there");
    }

    #[test]
    fn awaiting_task_is_input_required_with_a_prompt() {
        let history = [msg("u1", Role::User, "answer please")];
        let pending = Pending {
            tool_use_id: "c1".into(),
            name: "submit_answer".into(),
            input: serde_json::Value::Null,
            client_executed: true,
        };
        let task = encode_task("t", &history, &outcome(true, Some(pending)));
        assert_eq!(task.status.state, TaskState::InputRequired);
        assert!(
            task.status
                .message
                .unwrap()
                .text()
                .contains("submit_answer")
        );
    }

    #[test]
    fn exhausted_step_budget_projects_a_failed_task() {
        // A run that hit its step ceiling (not awaiting) is a `failed` A2A task, and
        // still carries the transcript + last agent status.
        let history = [
            msg("u1", Role::User, "do a lot"),
            msg("a1", Role::Assistant, "partial progress"),
        ];
        let exhausted = StepOutcome::ended(Vec::new(), EndCause::MaxSteps, false, false);
        let task = encode_task("t", &history, &exhausted);
        assert_eq!(task.status.state, TaskState::Failed);
        assert_eq!(task.status.message.unwrap().text(), "partial progress");
    }

    #[test]
    fn completed_task_with_no_agent_message_has_no_status_message() {
        // History holding only a user turn (agent produced nothing textual) → a
        // completed task whose status message is absent, not a user message.
        let history = [msg("u1", Role::User, "hi")];
        let task = encode_task("t", &history, &outcome(false, None));
        assert_eq!(task.status.state, TaskState::Completed);
        assert!(task.status.message.is_none());
    }

    #[test]
    fn history_projection_keeps_only_text_from_mixed_content() {
        let history = [AgentMessage {
            id: MessageId("a1".into()),
            role: Role::Assistant,
            content: vec![
                ContentBlock::image_url("https://x/y.png"),
                ContentBlock::text("described"),
            ],
        }];
        let task = encode_task("t", &history, &outcome(false, None));
        // The image is dropped from the A2A projection; the text survives.
        assert_eq!(task.status.message.unwrap().text(), "described");
        assert_eq!(task.history[0].parts.len(), 1);
    }

    #[test]
    fn input_required_without_a_pending_falls_back_to_the_last_agent_message() {
        // Cause/effect rule A2: Awaiting + no Pending + prior agent message ->
        // InputRequired + that message. A1 (Pending present) owns the generated
        // tool prompt; A3 (neither source) yields no status message.
        let history = [
            msg("u1", Role::User, "question"),
            msg("a1", Role::Assistant, "still thinking"),
        ];
        let task = encode_task("t", &history, &outcome(true, None));
        assert_eq!(task.status.state, TaskState::InputRequired);
        assert_eq!(task.status.message.unwrap().text(), "still thinking");
    }

    #[test]
    fn history_maps_user_and_assistant_to_a2a_roles() {
        let history = [
            msg("u1", Role::User, "hi"),
            msg("a1", Role::Assistant, "hello"),
        ];
        let task = encode_task("t", &history, &outcome(false, None));
        assert_eq!(task.history[0].role, MessageRole::User);
        assert_eq!(task.history[1].role, MessageRole::Agent);
    }
}
