//! AI SDK request decoding: thread id, new-message conversion, and tool-call
//! decision extraction. All AI SDK–specific parsing lives here so the router only
//! routes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use serde_json::Value;

use crate::types::{AiSdkChatRequest, ToolDecisionPart, UIMessage, UIPart};

static THREAD_SEQ: AtomicU64 = AtomicU64::new(0);

/// A client's answer to an awaiting tool, before binding-specific translation.
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
    match serde_json::from_value::<UIPart>(part.clone()).ok()? {
        UIPart::Text { text } => (!text.is_empty()).then(|| ContentBlock::text(text)),
        UIPart::File { media_type, url } => {
            if !media_type.starts_with("image/") {
                return None;
            }
            Some(match parse_data_uri(&url) {
                Some((mime, data)) => ContentBlock::image_base64(mime, data),
                None => ContentBlock::image_url(url),
            })
        }
        UIPart::Other => None,
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
            let Ok(tool) = serde_json::from_value::<ToolDecisionPart>(part.clone()) else {
                continue;
            };
            // Only assistant `tool-*` parts carry decisions; a provider-executed
            // part is history, not a fresh decision.
            if !tool.kind.starts_with("tool-") || tool.provider_executed {
                continue;
            }
            let (Some(state), Some(tool_call_id)) = (tool.state.as_deref(), tool.tool_call_id)
            else {
                continue;
            };
            let kind = match state {
                "output-available" => DecisionKind::Output(tool.output.unwrap_or(Value::Null)),
                "output-error" => DecisionKind::Error(
                    tool.error_text
                        .unwrap_or_else(|| "tool execution error".to_string()),
                ),
                "output-denied" => DecisionKind::Denied,
                "approval-responded" => {
                    if tool.approval.map(|a| a.approved).unwrap_or(false) {
                        DecisionKind::Approved
                    } else {
                        DecisionKind::Denied
                    }
                }
                _ => continue,
            };
            decisions.push(Decision { tool_call_id, kind });
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_protocol_transport::blocks_text;
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

    fn decision_part(state: &str, extra: Value) -> Value {
        let mut part = json!({ "type": "tool-probe", "toolCallId": "c1", "state": state });
        if let (Value::Object(dst), Value::Object(src)) = (&mut part, extra) {
            dst.extend(src);
        }
        part
    }

    fn only_decision(state: &str, extra: Value) -> DecisionKind {
        let req = AiSdkChatRequest {
            messages: vec![ui("assistant", "a1", vec![decision_part(state, extra)])],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert_eq!(
            p.decisions.len(),
            1,
            "exactly one decision for state {state}"
        );
        p.decisions.into_iter().next().unwrap().kind
    }

    #[test]
    fn decodes_output_error_decision() {
        assert_eq!(
            only_decision("output-error", json!({ "errorText": "boom" })),
            DecisionKind::Error("boom".into())
        );
    }

    #[test]
    fn output_error_without_text_uses_the_default_message() {
        assert_eq!(
            only_decision("output-error", json!({})),
            DecisionKind::Error("tool execution error".into())
        );
    }

    #[test]
    fn decodes_output_denied_decision() {
        assert_eq!(
            only_decision("output-denied", json!({})),
            DecisionKind::Denied
        );
    }

    #[test]
    fn approval_responded_true_is_approved() {
        assert_eq!(
            only_decision(
                "approval-responded",
                json!({ "approval": { "approved": true } })
            ),
            DecisionKind::Approved
        );
    }

    #[test]
    fn approval_responded_false_is_denied() {
        assert_eq!(
            only_decision(
                "approval-responded",
                json!({ "approval": { "approved": false } })
            ),
            DecisionKind::Denied
        );
    }

    #[test]
    fn approval_responded_without_an_approval_field_is_denied() {
        // Absent `approval` ⇒ `unwrap_or(false)` ⇒ fail closed to Denied.
        assert_eq!(
            only_decision("approval-responded", json!({})),
            DecisionKind::Denied
        );
    }

    #[test]
    fn output_available_without_output_decodes_as_null() {
        assert_eq!(
            only_decision("output-available", json!({})),
            DecisionKind::Output(Value::Null)
        );
    }

    #[test]
    fn unknown_decision_state_is_dropped() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "assistant",
                "a1",
                vec![decision_part("mystery", json!({}))],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        assert!(process_request(req, &HashSet::new()).decisions.is_empty());
    }

    #[test]
    fn non_tool_assistant_part_yields_no_decision() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "assistant",
                "a1",
                vec![json!({ "type": "text", "text": "just prose" })],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        assert!(process_request(req, &HashSet::new()).decisions.is_empty());
    }

    #[test]
    fn system_role_message_converts_to_system() {
        let req = AiSdkChatRequest {
            messages: vec![ui(
                "system",
                "s1",
                vec![json!({"type":"text","text":"be terse"})],
            )],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, Role::System);
    }

    #[test]
    fn a_missing_thread_id_is_minted() {
        let req = AiSdkChatRequest {
            messages: vec![ui("user", "u1", vec![json!({"type":"text","text":"hi"})])],
            thread_id: None,
            agent_id: None,
        };
        let p = process_request(req, &HashSet::new());
        assert!(p.thread_id.starts_with("thread-"));
    }

    fn user_parts(parts: Vec<Value>) -> ProcessedRequest {
        let req = AiSdkChatRequest {
            messages: vec![ui("user", "u1", parts)],
            thread_id: Some("t".into()),
            agent_id: None,
        };
        process_request(req, &HashSet::new())
    }

    #[test]
    fn a_malformed_data_uri_falls_back_to_an_image_url() {
        use awaken_agent_contract::agent::content::ImageSource;
        let p = user_parts(vec![
            json!({"type":"file","mediaType":"image/png","url":"data:garbage-no-comma"}),
        ]);
        assert!(matches!(
            &p.messages[0].content[0],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "data:garbage-no-comma"
        ));
    }

    #[test]
    fn a_remote_image_url_becomes_an_image_url_block() {
        use awaken_agent_contract::agent::content::ImageSource;
        let p = user_parts(vec![
            json!({"type":"file","mediaType":"image/png","url":"https://x/y.png"}),
        ]);
        assert!(matches!(
            &p.messages[0].content[0],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "https://x/y.png"
        ));
    }

    #[test]
    fn an_empty_text_part_is_dropped() {
        let p = user_parts(vec![
            json!({"type":"text","text":""}),
            json!({"type":"text","text":"keep"}),
        ]);
        assert_eq!(p.messages[0].content.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "keep");
    }

    #[test]
    fn a_non_image_file_part_is_dropped() {
        let p = user_parts(vec![
            json!({"type":"file","mediaType":"application/pdf","url":"data:application/pdf;base64,AAAA"}),
            json!({"type":"text","text":"keep"}),
        ]);
        assert_eq!(p.messages[0].content.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "keep");
    }

    #[test]
    fn an_unknown_part_type_is_dropped_from_content() {
        // A non-content part (e.g. reasoning/step-start) is not user input.
        let p = user_parts(vec![
            json!({ "type": "reasoning", "text": "thinking" }),
            json!({ "type": "text", "text": "keep" }),
        ]);
        assert_eq!(p.messages[0].content.len(), 1);
        assert_eq!(blocks_text(&p.messages[0].content), "keep");
    }

    #[test]
    fn result_text_renders_the_three_value_shapes() {
        // A string is unquoted; null is empty; anything else is its JSON form.
        assert_eq!(result_text(&json!("hi")), "hi");
        assert_eq!(result_text(&Value::Null), "");
        assert_eq!(result_text(&json!({ "a": 1 })), "{\"a\":1}");
    }
}
