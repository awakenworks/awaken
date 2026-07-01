//! Project a committed step (and thread history) into an A2A `Task`.
//!
//! A2A is a snapshot protocol: `message:send` returns the whole `Task` — its
//! lifecycle `status` plus the conversation `history`. This module folds committed
//! neutral `Message`s into A2A `Message`s and derives the `TaskState` from the step
//! outcome (parked → input-required; step-budget exhausted → failed; else
//! completed).

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message as AgentMessage, Role};

use crate::port::StepOutcome;
use crate::types::{Message, MessageRole, Part, Task, TaskState, TaskStatus};

/// Build the `Task` returned for a step on `thread`. `history` is the thread's full
/// committed transcript (post-step); `outcome` classifies the terminal state.
pub fn encode_task(thread: &str, history: &[AgentMessage], outcome: &StepOutcome) -> Task {
    let messages: Vec<Message> = history
        .iter()
        .filter_map(|m| to_a2a_message(thread, m))
        .collect();

    let state = if outcome.waiting {
        TaskState::InputRequired
    } else if outcome.exhausted {
        TaskState::Failed
    } else {
        TaskState::Completed
    };

    // The status message is what the agent last said. When parked on a tool, it is
    // the prompt for the input the task now requires.
    let status_message = match (&state, outcome.pending.as_ref()) {
        (TaskState::InputRequired, Some(pending)) => Some(Message::agent_text(
            format!("{thread}-status"),
            format!("input required for tool `{}`", pending.name),
        )),
        _ => messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Agent)
            .cloned(),
    };

    Task {
        id: format!("task-{thread}"),
        context_id: thread.to_string(),
        status: TaskStatus {
            state,
            message: status_message,
        },
        history: messages,
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
    use crate::port::Pending;
    use awaken_agent_contract::agent::message::Id as MessageId;

    fn msg(id: &str, role: Role, text: &str) -> AgentMessage {
        AgentMessage::text(MessageId(id.into()), role, text)
    }

    fn outcome(waiting: bool, pending: Option<Pending>) -> StepOutcome {
        StepOutcome {
            new_messages: Vec::new(),
            waiting,
            exhausted: false,
            pending,
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
}
