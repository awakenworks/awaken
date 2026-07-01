//! AG-UI request decoding: thread/run ids, new-message conversion, and tool-result
//! extraction. All AG-UI–specific parsing lives here so the router only routes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

use crate::types::{AgUiMessage, RunAgentInput};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn next(prefix: &str) -> String {
    format!("{prefix}-{}", SEQ.fetch_add(1, Ordering::SeqCst))
}

/// A client's tool result (AG-UI delivers these as `role: "tool"` messages).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultInput {
    pub tool_call_id: String,
    pub content: String,
}

/// A decoded `RunAgentInput` ready for the runtime.
pub struct Processed {
    pub thread_id: String,
    pub run_id: String,
    pub messages: Vec<Message>,
    pub tool_results: Vec<ToolResultInput>,
    pub agent_id: Option<String>,
}

impl Processed {
    /// True when the input carries only tool results (no new content) — a resume.
    pub fn is_resume_only(&self) -> bool {
        self.messages.is_empty() && !self.tool_results.is_empty()
    }
}

/// Decode a run input: resolve thread/run ids, convert new user/system messages
/// (deduplicated against `known_ids`), and extract tool results.
pub fn process(
    input: RunAgentInput,
    agent_id: Option<String>,
    known_ids: &HashSet<String>,
) -> Processed {
    let thread_id = input
        .thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| next("thread"));
    let run_id = input
        .run_id
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| next("run"));

    let tool_results = extract_tool_results(&input.messages);
    let messages = convert_new_messages(&input.messages, known_ids);

    Processed {
        thread_id,
        run_id,
        messages,
        tool_results,
        agent_id,
    }
}

/// Convert new user/system messages to runtime input. Assistant messages are
/// server-held history and are ignored; messages already committed are dropped.
fn convert_new_messages(messages: &[AgUiMessage], known_ids: &HashSet<String>) -> Vec<Message> {
    let mut out = Vec::new();
    for message in messages {
        if let Some(id) = &message.id
            && known_ids.contains(id)
        {
            continue;
        }
        let role = match message.role.as_str() {
            "user" => Role::User,
            "system" | "developer" => Role::System,
            _ => continue,
        };
        let text = message.content.clone().unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        let id = message.id.clone().unwrap_or_else(|| next("msg"));
        out.push(Message::text(MessageId(id), role, text));
    }
    out
}

/// Extract tool results from `role: "tool"` messages.
fn extract_tool_results(messages: &[AgUiMessage]) -> Vec<ToolResultInput> {
    messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| {
            let tool_call_id = m.tool_call_id.clone()?;
            Some(ToolResultInput {
                tool_call_id,
                content: m.content.clone().unwrap_or_default(),
            })
        })
        .collect()
}

/// Convert content blocks to a plain string (bounds tool outputs to text).
pub fn blocks_text(content: &[ContentBlock]) -> String {
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
    use serde_json::json;

    fn msg(role: &str, id: &str, content: Option<&str>, tool_call_id: Option<&str>) -> AgUiMessage {
        serde_json::from_value(json!({
            "id": id,
            "role": role,
            "content": content,
            "toolCallId": tool_call_id,
        }))
        .unwrap()
    }

    #[test]
    fn converts_new_user_and_skips_assistant() {
        let input = RunAgentInput {
            thread_id: Some("t".into()),
            run_id: Some("r".into()),
            messages: vec![
                msg("user", "u1", Some("hi"), None),
                msg("assistant", "a1", Some("prior"), None),
            ],
        };
        let p = process(input, None, &HashSet::new());
        assert_eq!(p.thread_id, "t");
        assert_eq!(p.run_id, "r");
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, Role::User);
    }

    #[test]
    fn extracts_tool_result_and_dedups() {
        let input = RunAgentInput {
            thread_id: Some("t".into()),
            run_id: None,
            messages: vec![
                msg("user", "u1", Some("old"), None),
                msg("tool", "tr1", Some("42"), Some("c1")),
            ],
        };
        let known = HashSet::from(["u1".to_string()]);
        let p = process(input, None, &known);
        assert!(p.is_resume_only());
        assert_eq!(
            p.tool_results,
            vec![ToolResultInput {
                tool_call_id: "c1".into(),
                content: "42".into()
            }]
        );
        assert!(p.run_id.starts_with("run-"));
    }
}
