//! Project a committed step (and thread history) into an A2A `Task`.
//!
//! A2A is a snapshot protocol: `message:send` returns the whole `Task` — its
//! lifecycle `status` plus the conversation `history`. This module folds committed
//! neutral `Message`s into A2A `Message`s and derives the `TaskState` from the step
//! outcome (parked → input-required; step-budget exhausted → failed; else
//! completed).

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message as AgentMessage, Role};

use awaken_protocol_transport::{StepOutcome, Terminal};

use crate::types::{Message, MessageRole, Part, Task, TaskState, TaskStatus};

/// Build the `Task` returned for a step on `thread`. `history` is the thread's full
/// committed transcript (post-step); `outcome` classifies the terminal state.
pub fn encode_task(thread: &str, history: &[AgentMessage], outcome: &StepOutcome) -> Task {
    let messages: Vec<Message> = history
        .iter()
        .filter_map(|m| to_a2a_message(thread, m))
        .collect();

    let state = match &outcome.terminal {
        Terminal::Waiting { .. } => TaskState::InputRequired,
        Terminal::Failed(_) | Terminal::Exhausted => TaskState::Failed,
        Terminal::Finished => TaskState::Completed,
    };

    // The status message is what the agent last said. A terminal fault carries its
    // message so the task explains why it failed; when parked on a tool, it is the
    // prompt for the input the task now requires.
    let status_message = if let Terminal::Failed(failure) = &outcome.terminal {
        Some(Message::agent_text(
            format!("{thread}-status"),
            failure.message.clone(),
        ))
    } else {
        match (&state, outcome.pending()) {
            (TaskState::InputRequired, Some(pending)) => Some(Message::agent_text(
                format!("{thread}-status"),
                format!("input required for tool `{}`", pending.name),
            )),
            _ => messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::Agent)
                .cloned(),
        }
    };

    Task {
        kind: Some("task".to_string()),
        id: format!("task-{thread}"),
        context_id: thread.to_string(),
        status: TaskStatus {
            state,
            message: status_message,
        },
        history: messages,
        artifacts: Vec::new(),
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
    let text = block_text(&message.content);
    if text.trim().is_empty() {
        return None;
    }
    Some(Message {
        kind: Some("message".to_string()),
        task_id: Some(format!("task-{thread}")),
        context_id: Some(thread.to_string()),
        message_id: message.id.0.clone(),
        role,
        parts: vec![Part::text(text)],
    })
}

fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_protocol_transport::{Pending, StepFailure};

    fn msg(id: &str, role: Role, text: &str) -> AgentMessage {
        AgentMessage::text(MessageId(id.into()), role, text)
    }

    #[test]
    fn terminal_failure_yields_a_failed_task_with_the_fault_message() {
        let outcome = StepOutcome {
            terminal: Terminal::Failed(StepFailure {
                code: "inference_failed".into(),
                message: "upstream is down".into(),
            }),
            ..Default::default()
        };
        let task = encode_task("th", &[], &outcome);
        assert_eq!(task.status.state, TaskState::Failed);
        let text = task
            .status
            .message
            .as_ref()
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| p.text.as_deref())
                    .collect::<String>()
            })
            .unwrap_or_default();
        assert!(
            text.contains("upstream is down"),
            "status carries the fault: {text}"
        );
    }

    fn outcome(waiting: bool, pending: Option<Pending>) -> StepOutcome {
        StepOutcome {
            new_messages: Vec::new(),
            terminal: if waiting {
                Terminal::Waiting { pending }
            } else {
                Terminal::Finished
            },
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
    fn parked_task_is_input_required_with_a_prompt() {
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
        // A run that hit its step ceiling (not parked) is a `failed` A2A task, and
        // still carries the transcript + last agent status.
        let history = [
            msg("u1", Role::User, "do a lot"),
            msg("a1", Role::Assistant, "partial progress"),
        ];
        let exhausted = StepOutcome {
            new_messages: Vec::new(),
            terminal: Terminal::Exhausted,
        };
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
        // `waiting` with no `pending` (an edge the prompt branch can't cover): the
        // task is input-required but the status message is the last agent turn.
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
