//! A2A request decoding: resolve the context (thread) id and convert the inbound
//! message to neutral runtime input. All A2A-specific parsing lives here so the
//! router only routes.

use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

use crate::types::SendMessageRequest;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn next(prefix: &str) -> String {
    format!("{prefix}-{}", SEQ.fetch_add(1, Ordering::SeqCst))
}

/// A decoded `message:send` request ready for the runtime: the A2A `contextId`
/// maps to the neutral thread id, the message text is the turn input (or, when the
/// thread has a parked run, the tool answer the router delivers on resume).
pub struct Processed {
    pub thread_id: String,
    pub agent_id: Option<String>,
    pub text: String,
    /// The neutral user message for a fresh turn.
    pub message: Message,
}

/// Decode a `message:send` request: the `contextId` (or `taskId`) is the thread;
/// absent, a fresh one is minted. The message's text parts become the turn input.
pub fn process(req: SendMessageRequest, path_agent: Option<String>) -> Processed {
    let thread_id = req
        .message
        .context_id
        .clone()
        .or_else(|| req.message.task_id.clone())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| next("thread"));

    let text = req.message.text();
    let message_id = req
        .message
        .message_id
        .clone()
        .unwrap_or_else(|| next("msg"));
    let message = Message::text(MessageId(message_id), Role::User, text.clone());

    Processed {
        thread_id,
        // The path agent (e.g. `/v1/a2a/agents/{agent}/...`) wins over a body
        // selector; either falls back to the host default.
        agent_id: path_agent.or(req.agent_id),
        text,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageRole, Part, SendMessage};

    fn req(context: Option<&str>, text: &str) -> SendMessageRequest {
        SendMessageRequest {
            agent_id: None,
            message: SendMessage {
                message_id: Some("m1".into()),
                context_id: context.map(Into::into),
                task_id: None,
                role: MessageRole::User,
                parts: vec![Part::text(text)],
            },
        }
    }

    #[test]
    fn context_id_becomes_the_thread() {
        let p = process(req(Some("ctx-1"), "hi"), None);
        assert_eq!(p.thread_id, "ctx-1");
        assert_eq!(p.text, "hi");
        assert_eq!(p.message.role, Role::User);
    }

    #[test]
    fn absent_context_mints_a_thread() {
        let p = process(req(None, "hi"), None);
        assert!(p.thread_id.starts_with("thread-"));
    }

    #[test]
    fn path_agent_wins_over_body_selector() {
        let mut r = req(Some("c"), "hi");
        r.agent_id = Some("body-agent".into());
        let p = process(r, Some("path-agent".into()));
        assert_eq!(p.agent_id.as_deref(), Some("path-agent"));
    }
}
