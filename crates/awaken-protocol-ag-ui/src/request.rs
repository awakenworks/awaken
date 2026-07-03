//! AG-UI request decoding: thread/run ids, new-message conversion, and tool-result
//! extraction. All AG-UI–specific parsing lives here so the router only routes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

use crate::types::{AgUiContent, AgUiMessage, InputContentPart, InputContentSource, RunAgentInput};

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
        let blocks = message
            .content
            .as_ref()
            .map(content_blocks)
            .unwrap_or_default();
        if blocks.is_empty() {
            continue;
        }
        let id = message.id.clone().unwrap_or_else(|| next("msg"));
        out.push(Message::new(MessageId(id), role, blocks));
    }
    out
}

/// Convert AG-UI message content into neutral content blocks: a plain string
/// becomes one text block; a typed part list maps `text` and image parts (inline
/// base64 or remote URL) to their neutral blocks.
fn content_blocks(content: &AgUiContent) -> Vec<ContentBlock> {
    match content {
        AgUiContent::Text(text) if !text.is_empty() => vec![ContentBlock::text(text)],
        AgUiContent::Text(_) => Vec::new(),
        AgUiContent::Parts(parts) => parts.iter().filter_map(part_to_block).collect(),
    }
}

fn part_to_block(part: &InputContentPart) -> Option<ContentBlock> {
    match part {
        InputContentPart::Text { text } => (!text.is_empty()).then(|| ContentBlock::text(text)),
        InputContentPart::Image { source } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::image_base64(mime_type, value)
            }
            InputContentSource::Url { value } => ContentBlock::image_url(value),
        }),
    }
}

/// The text of AG-UI message content, ignoring media (bounds tool results to text).
fn content_text(content: &Option<AgUiContent>) -> String {
    match content {
        Some(AgUiContent::Text(text)) => text.clone(),
        Some(AgUiContent::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                InputContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
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
                content: content_text(&m.content),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_protocol_transport::blocks_text;
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
    fn decodes_image_content_parts_into_blocks() {
        let input = RunAgentInput {
            thread_id: Some("t".into()),
            run_id: Some("r".into()),
            messages: vec![
                serde_json::from_value(json!({
                    "id": "u1",
                    "role": "user",
                    "content": [
                        { "type": "image", "source": { "type": "data", "value": "AAAA", "mimeType": "image/png" } },
                        { "type": "text", "text": "what is this" },
                    ],
                }))
                .unwrap(),
            ],
        };
        let p = process(input, None, &HashSet::new());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].content.len(), 2);
        assert!(matches!(
            p.messages[0].content[0],
            ContentBlock::Image { .. }
        ));
        assert_eq!(blocks_text(&p.messages[0].content), "what is this");
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
