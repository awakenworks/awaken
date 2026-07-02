//! AI SDK request decoding: thread id, new-message conversion, and tool-call
//! decision extraction. All AI SDK–specific parsing lives here so the router only
//! routes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use serde_json::Value;

use crate::types::{AiSdkChatRequest, UIMessage};

static THREAD_SEQ: AtomicU64 = AtomicU64::new(0);

/// A client's answer to a parked tool, before binding-specific translation.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionKind {
    /// `output-available`: the tool produced a result.
    Output(Value),
    /// `output-error`: the tool failed.
    Error(String),
    /// `output-denied` or `approval-responded { approved: false }`.
    Denied,
    /// `approval-responded { approved: true }`.
    Approved,
}

/// One extracted tool-call decision keyed by its call id.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub tool_call_id: String,
    pub kind: DecisionKind,
}

/// A decoded request ready for the runtime.
pub struct ProcessedRequest {
    pub thread_id: String,
    pub messages: Vec<Message>,
    pub decisions: Vec<Decision>,
    pub agent_id: Option<String>,
}

impl ProcessedRequest {
    /// True when the request carries only decisions (no new content) — a resume.
    pub fn is_resume_only(&self) -> bool {
        self.messages.is_empty() && !self.decisions.is_empty()
    }
}

/// Decode a chat request: resolve the thread id, convert new user/system messages
/// (deduplicated against `known_ids`), and extract tool-call decisions.
pub fn process_request(payload: AiSdkChatRequest, known_ids: &HashSet<String>) -> ProcessedRequest {
    let thread_id = payload
        .thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("thread-{}", THREAD_SEQ.fetch_add(1, Ordering::SeqCst)));

    let decisions = extract_decisions(&payload.messages);
    let messages = convert_new_messages(&payload.messages, known_ids);

    ProcessedRequest {
        thread_id,
        messages,
        decisions,
        agent_id: payload.agent_id,
    }
}

/// Convert new user/system messages to runtime input. Assistant messages are
/// server-held history and are ignored; messages whose id is already committed
/// are dropped (client history replay).
fn convert_new_messages(messages: &[UIMessage], known_ids: &HashSet<String>) -> Vec<Message> {
    let mut out = Vec::new();
    for message in messages {
        if let Some(id) = &message.id
            && known_ids.contains(id)
        {
            continue;
        }
        let role = match message.role.as_str() {
            "user" => Role::User,
            "system" => Role::System,
            _ => continue,
        };
        let blocks = blocks_of(&message.parts);
        if blocks.is_empty() {
            continue;
        }
        let id = message
            .id
            .clone()
            .unwrap_or_else(|| format!("msg-{}", THREAD_SEQ.fetch_add(1, Ordering::SeqCst)));
        out.push(Message::new(MessageId(id), role, blocks));
    }
    out
}

/// Convert a UI message's parts into neutral content blocks: `text` parts become
/// text; `file` parts with an image media type become an image block (inline
/// `data:` base64 preferred, else a remote URL). Non-content parts (`tool-*`,
/// `step-start`, reasoning) are dropped — they are not user-visible input.
fn blocks_of(parts: &[Value]) -> Vec<ContentBlock> {
    parts.iter().filter_map(part_to_block).collect()
}

fn part_to_block(part: &Value) -> Option<ContentBlock> {
    match part.get("type").and_then(Value::as_str)? {
        "text" => {
            let text = part.get("text").and_then(Value::as_str)?;
            (!text.is_empty()).then(|| ContentBlock::text(text))
        }
        // AI SDK v5 file part: `{ type: "file", mediaType, url }` where `url` is
        // either a `data:` URI or a remote link.
        "file" => {
            let media_type = part.get("mediaType").and_then(Value::as_str)?;
            if !media_type.starts_with("image/") {
                return None;
            }
            let url = part.get("url").and_then(Value::as_str)?;
            Some(match parse_data_uri(url) {
                Some((mime, data)) => ContentBlock::image_base64(mime, data),
                None => ContentBlock::image_url(url),
            })
        }
        _ => None,
    }
}

/// Split a `data:<mime>;base64,<data>` URI into its mime type and base64 payload.
fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime.to_string(), data.to_string()))
}

/// Extract tool-call decisions from assistant `tool-*` parts. A part run by the
/// provider (`providerExecuted: true`) is historical, not a fresh decision.
fn extract_decisions(messages: &[UIMessage]) -> Vec<Decision> {
    let mut decisions = Vec::new();
    for message in messages.iter().filter(|m| m.role == "assistant") {
        for part in &message.parts {
            let Some(part_type) = part.get("type").and_then(Value::as_str) else {
                continue;
            };
            if !part_type.starts_with("tool-") {
                continue;
            }
            if part
                .get("providerExecuted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(state) = part.get("state").and_then(Value::as_str) else {
                continue;
            };
            let Some(tool_call_id) = part.get("toolCallId").and_then(Value::as_str) else {
                continue;
            };
            let kind = match state {
                "output-available" => {
                    DecisionKind::Output(part.get("output").cloned().unwrap_or(Value::Null))
                }
                "output-error" => DecisionKind::Error(
                    part.get("errorText")
                        .and_then(Value::as_str)
                        .unwrap_or("tool execution error")
                        .to_string(),
                ),
                "output-denied" => DecisionKind::Denied,
                "approval-responded" => {
                    let approved = part
                        .get("approval")
                        .and_then(|a| a.get("approved"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if approved {
                        DecisionKind::Approved
                    } else {
                        DecisionKind::Denied
                    }
                }
                _ => continue,
            };
            decisions.push(Decision {
                tool_call_id: tool_call_id.to_string(),
                kind,
            });
        }
    }
    decisions
}

/// Render a decision result value as tool-result text.
pub fn result_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Convert content blocks to a plain string (used to bound tool outputs to text).
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

    fn ui(role: &str, id: &str, parts: Vec<Value>) -> UIMessage {
        UIMessage {
            id: Some(id.to_string()),
            role: role.to_string(),
            parts,
        }
    }

    #[test]
    fn converts_new_user_text_and_skips_assistant() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"hi"})]),
                ui(
                    "assistant",
                    "a1",
                    vec![json!({"type":"text","text":"prior"})],
                ),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert_eq!(p.thread_id, "t");
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, Role::User);
    }

    #[test]
    fn decodes_image_file_part_into_an_image_block() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "user",
                "u1",
                vec![
                    json!({"type":"file","mediaType":"image/png","url":"data:image/png;base64,AAAA"}),
                    json!({"type":"text","text":"what is this"}),
                ],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].content.len(), 2);
        assert!(matches!(
            p.messages[0].content[0],
            ContentBlock::Image {
                source: awaken_agent_contract::agent::content::ImageSource::Base64 { .. }
            }
        ));
        assert_eq!(blocks_text(&p.messages[0].content), "what is this");
    }

    #[test]
    fn dedups_known_ids() {
        let req = AiSdkChatRequest {
            messages: vec![
                ui("user", "u1", vec![json!({"type":"text","text":"old"})]),
                ui("user", "u2", vec![json!({"type":"text","text":"new"})]),
            ],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let known = HashSet::from(["u1".to_string()]);
        let p = process_request(req, &known);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "new");
    }

    #[test]
    fn extracts_client_output_decision() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "assistant",
                "a1",
                vec![json!({
                    "type":"tool-submit_answer",
                    "toolCallId":"c1",
                    "state":"output-available",
                    "output":"42"
                })],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert!(p.is_resume_only());
        assert_eq!(p.decisions.len(), 1);
        assert_eq!(p.decisions[0].tool_call_id, "c1");
        assert_eq!(p.decisions[0].kind, DecisionKind::Output(json!("42")));
    }

    #[test]
    fn skips_provider_executed_history() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "assistant",
                "a1",
                vec![json!({
                    "type":"tool-read",
                    "toolCallId":"c1",
                    "state":"output-available",
                    "output":"x",
                    "providerExecuted": true
                })],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert!(p.decisions.is_empty());
    }
}
