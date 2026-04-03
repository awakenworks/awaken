//! AI SDK v6 request processing: wire-format parsing, message deduplication,
//! tool-call decision extraction, and conversion to runtime types.
//!
//! All AI SDK–specific request logic lives here so that `http.rs` can focus
//! purely on routing and SSE wiring.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

// ── AI SDK v6 wire types ────────────────────────────────────────────
//
// Maps directly to the TypeScript `UIMessage` / `UIMessagePart` types
// from the `ai` npm package (v6). No legacy `content` field.
//
// Reference: https://ai-sdk.dev/docs/ai-sdk-ui/chatbot

/// Top-level request body sent by `DefaultChatTransport`.
#[derive(Debug, Deserialize)]
pub(crate) struct AiSdkChatRequest {
    #[serde(default)]
    pub messages: Vec<UIMessage>,
    #[serde(rename = "threadId", alias = "thread_id", default)]
    pub thread_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub state: Option<Value>,
}

/// A single part of a `UIMessage`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum UIPart {
    Text {
        text: String,
    },
    Reasoning {
        #[allow(dead_code)]
        text: String,
    },
    File {
        url: String,
        #[serde(rename = "mediaType")]
        media_type: String,
    },
    SourceUrl {
        #[allow(dead_code)]
        #[serde(rename = "sourceId")]
        source_id: String,
        #[allow(dead_code)]
        url: String,
        #[allow(dead_code)]
        #[serde(default)]
        title: Option<String>,
    },
    SourceDocument {
        #[allow(dead_code)]
        #[serde(rename = "sourceId")]
        source_id: String,
        #[allow(dead_code)]
        #[serde(rename = "mediaType")]
        media_type: String,
        #[allow(dead_code)]
        #[serde(default)]
        title: Option<String>,
        #[allow(dead_code)]
        #[serde(default)]
        filename: Option<String>,
    },
    StepStart,
    /// Catch-all for tool invocations (`tool-*`), dynamic tools, and data parts (`data-*`).
    /// These don't map to LLM content blocks — they are UI state, not input.
    #[serde(other)]
    Other,
}

/// AI SDK v6 `UIMessage` — the wire format sent by `DefaultChatTransport`.
#[derive(Debug, Deserialize)]
pub(crate) struct UIMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub parts: Vec<Value>,
}

// ── Result of processing an AI SDK request ──────────────────────────

/// Processed AI SDK request ready for the runtime.
pub(crate) struct ProcessedRequest {
    pub thread_id: String,
    pub messages: Vec<Message>,
    pub decisions: Vec<(String, ToolCallResume)>,
    pub state: Option<Value>,
    pub agent_id: Option<String>,
}

impl ProcessedRequest {
    /// True when the request carries only tool-call decisions (no new content).
    pub(crate) fn is_resume_only(&self) -> bool {
        self.messages.is_empty() && !self.decisions.is_empty()
    }
}

// ── Public entry point ──────────────────────────────────────────────

/// Process a raw AI SDK chat request: resolve thread ID, deduplicate
/// messages against the persisted store, extract tool-call decisions,
/// and convert new messages to runtime format.
pub(crate) async fn process_chat_request(
    store: &dyn ThreadRunStore,
    payload: AiSdkChatRequest,
) -> Result<ProcessedRequest, String> {
    let agent_id = payload.agent_id;
    let state = payload.state;

    // Extract decisions from ALL messages (including old ones with tool responses).
    // This must happen before dedup because decisions live in historical assistant
    // messages that the store already knows about.
    let decisions = extract_tool_call_decisions(&payload.messages);

    let thread_id = payload
        .thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // Load persisted message IDs to deduplicate incoming messages.
    let known_ids: HashSet<String> = store
        .load_messages(&thread_id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.id)
        .collect();

    // Only convert NEW messages (not already in store) for the runtime.
    let new_ui_messages = dedup_messages(payload.messages, &known_ids);
    let messages = convert_messages(new_ui_messages);

    if messages.is_empty() && decisions.is_empty() {
        return Err("no new messages or interaction responses".to_string());
    }

    Ok(ProcessedRequest {
        thread_id,
        messages,
        decisions,
        state,
        agent_id,
    })
}

// ── Internal helpers ────────────────────────────────────────────────

/// Parse a data-URI of the form `data:<media_type>;base64,<data>`.
fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(",")?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type.to_string(), data.to_string()))
}

pub(crate) fn part_kind(part: &Value) -> Option<&str> {
    part.get("type").and_then(Value::as_str)
}

/// Convert a raw UI part to a `ContentBlock`.
fn part_to_content_block(part: &Value) -> Option<ContentBlock> {
    match serde_json::from_value::<UIPart>(part.clone()).ok()? {
        UIPart::Text { text } => Some(ContentBlock::text(text.as_str())),
        UIPart::File { url, media_type } => {
            if let Some((mime, data)) = parse_data_uri(&url) {
                if mime.starts_with("image/") {
                    Some(ContentBlock::image_base64(mime, data))
                } else if mime.starts_with("audio/") {
                    Some(ContentBlock::audio_base64(mime, data))
                } else if mime.starts_with("video/") {
                    Some(ContentBlock::video_base64(mime, data))
                } else {
                    Some(ContentBlock::document_base64(mime, data, None))
                }
            } else if media_type.starts_with("image/") {
                Some(ContentBlock::image_url(url.as_str()))
            } else if media_type.starts_with("audio/") {
                Some(ContentBlock::audio_url(url.as_str()))
            } else if media_type.starts_with("video/") {
                Some(ContentBlock::video_url(url.as_str()))
            } else {
                Some(ContentBlock::document_url(url.as_str(), None))
            }
        }
        _ => None,
    }
}

/// Convert `UIMessage` list to runtime input messages.
///
/// Assistant history is already persisted server-side, so incoming assistant
/// UI messages are intentionally ignored here. This keeps AI SDK's client-side
/// synthesized assistant history from being written back into the thread on
/// later turns.
fn convert_messages(msgs: Vec<UIMessage>) -> Vec<Message> {
    msgs.into_iter()
        .filter_map(|m| {
            let blocks: Vec<ContentBlock> =
                m.parts.iter().filter_map(part_to_content_block).collect();
            if blocks.is_empty() {
                return None;
            }
            let mut msg = match m.role.as_str() {
                "user" => Message::user_with_content(blocks),
                "system" => {
                    Message::system(awaken_contract::contract::content::extract_text(&blocks))
                }
                _ => return None,
            };
            // Preserve frontend ID for deduplication; fall back to generated UUID.
            if let Some(id) = m.id {
                msg.id = Some(id);
            }
            Some(msg)
        })
        .collect()
}

/// Filter out messages whose IDs are already persisted in the store.
/// Messages without an ID are always kept (treated as new).
fn dedup_messages(msgs: Vec<UIMessage>, known_ids: &HashSet<String>) -> Vec<UIMessage> {
    if known_ids.is_empty() {
        return msgs;
    }
    msgs.into_iter()
        .filter(|m| match &m.id {
            Some(id) => !known_ids.contains(id),
            None => true,
        })
        .collect()
}

/// Extract tool-call decisions from assistant message parts.
fn extract_tool_call_decisions(msgs: &[UIMessage]) -> Vec<(String, ToolCallResume)> {
    msgs.iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.parts.iter())
        .filter_map(|part| {
            let part_type = part_kind(part)?;
            if !part_type.starts_with("tool-") {
                return None;
            }

            // Skip tool results already executed by the provider (server-side).
            // AI SDK v6 marks completed tool calls with providerExecuted: true
            // in the message history. Without this check, historical tool results
            // are misidentified as new resume decisions, breaking multi-turn chat.
            let provider_executed = part
                .get("providerExecuted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if provider_executed {
                return None;
            }

            let state = part.get("state").and_then(Value::as_str)?;
            let tool_call_id = part
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned)?;

            let (action, result) = match state {
                "output-available" => (
                    ResumeDecisionAction::Resume,
                    part.get("output").cloned().unwrap_or(Value::Null),
                ),
                "output-error" => (
                    ResumeDecisionAction::Resume,
                    json!({
                        "error": part
                            .get("errorText")
                            .and_then(Value::as_str)
                            .unwrap_or("tool execution error")
                    }),
                ),
                "output-denied" => (ResumeDecisionAction::Cancel, json!({ "approved": false })),
                "approval-responded" => {
                    let approval = part.get("approval")?;
                    let approved = approval.get("approved").and_then(Value::as_bool);
                    let action = if approved == Some(false) {
                        ResumeDecisionAction::Cancel
                    } else {
                        ResumeDecisionAction::Resume
                    };
                    (action, approval.clone())
                }
                _ => return None,
            };

            Some((
                tool_call_id,
                ToolCallResume {
                    decision_id: uuid::Uuid::now_v7().to_string(),
                    action,
                    result,
                    reason: None,
                    updated_at: awaken_contract::now_ms(),
                },
            ))
        })
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
    use serde_json::json;

    fn raw_part(value: Value) -> Value {
        value
    }

    // ── UIMessage deserialization ──

    #[test]
    fn deserialize_user_text_message() {
        let raw = json!({
            "id": "msg-1",
            "role": "user",
            "parts": [{"type": "text", "text": "hello"}]
        });
        let msg: UIMessage = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.parts.len(), 1);
        assert_eq!(part_kind(&msg.parts[0]), Some("text"));
    }

    #[test]
    fn deserialize_message_with_file_part() {
        let raw = json!({
            "id": "msg-2",
            "role": "user",
            "parts": [
                {"type": "text", "text": "Look at this"},
                {"type": "file", "url": "https://example.com/img.png", "mediaType": "image/png"}
            ]
        });
        let msg: UIMessage = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.parts.len(), 2);
        assert_eq!(part_kind(&msg.parts[1]), Some("file"));
    }

    #[test]
    fn deserialize_tool_parts_as_other() {
        let raw = json!({
            "id": "msg-3",
            "role": "assistant",
            "parts": [
                {"type": "text", "text": "Let me search"},
                {"type": "tool-search", "toolCallId": "c1", "state": "output-available", "input": {}, "output": "results"},
                {"type": "step-start"}
            ]
        });
        let msg: UIMessage = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.parts.len(), 3);
        assert_eq!(part_kind(&msg.parts[1]), Some("tool-search"));
        assert_eq!(part_kind(&msg.parts[2]), Some("step-start"));
    }

    // ── convert_messages ──

    #[test]
    fn convert_user_text_message() {
        let msgs = vec![UIMessage {
            id: Some("m1".into()),
            role: "user".into(),
            parts: vec![raw_part(json!({"type": "text", "text": "hello"}))],
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "hello");
        assert_eq!(converted[0].id.as_deref(), Some("m1"));
    }

    #[test]
    fn convert_ignores_assistant_messages() {
        let msgs = vec![
            UIMessage {
                id: Some("u1".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "hello"}))],
            },
            UIMessage {
                id: Some("a1".into()),
                role: "assistant".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "existing"}))],
            },
        ];

        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(
            converted[0].role,
            awaken_contract::contract::message::Role::User
        );
        assert_eq!(converted[0].id.as_deref(), Some("u1"));
    }

    #[test]
    fn convert_user_message_with_image_file() {
        let msgs = vec![UIMessage {
            id: Some("m2".into()),
            role: "user".into(),
            parts: vec![
                raw_part(json!({"type": "text", "text": "Describe"})),
                raw_part(json!({
                    "type": "file",
                    "url": "https://example.com/img.png",
                    "mediaType": "image/png"
                })),
            ],
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].content.len(), 2);
        assert_eq!(converted[0].content[0], ContentBlock::text("Describe"));
        assert_eq!(
            converted[0].content[1],
            ContentBlock::image_url("https://example.com/img.png")
        );
    }

    #[test]
    fn convert_user_message_with_base64_image() {
        let msgs = vec![UIMessage {
            id: None,
            role: "user".into(),
            parts: vec![raw_part(json!({
                "type": "file",
                "url": "data:image/png;base64,iVBOR",
                "mediaType": "image/png"
            }))],
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(
            converted[0].content[0],
            ContentBlock::image_base64("image/png", "iVBOR")
        );
    }

    #[test]
    fn convert_user_message_with_audio_file() {
        let msgs = vec![UIMessage {
            id: None,
            role: "user".into(),
            parts: vec![raw_part(json!({
                "type": "file",
                "url": "https://example.com/audio.mp3",
                "mediaType": "audio/mpeg"
            }))],
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(
            converted[0].content[0],
            ContentBlock::audio_url("https://example.com/audio.mp3")
        );
    }

    #[test]
    fn convert_skips_unknown_roles() {
        let msgs = vec![UIMessage {
            id: None,
            role: "function".into(),
            parts: vec![raw_part(json!({"type": "text", "text": "result"}))],
        }];
        let converted = convert_messages(msgs);
        assert!(converted.is_empty());
    }

    #[test]
    fn convert_skips_messages_with_only_non_content_parts() {
        let msgs = vec![UIMessage {
            id: None,
            role: "assistant".into(),
            parts: vec![
                raw_part(json!({"type": "step-start"})),
                raw_part(json!({"type": "tool-search", "toolCallId": "c1"})),
            ],
        }];
        let converted = convert_messages(msgs);
        assert!(converted.is_empty());
    }

    #[test]
    fn convert_system_message() {
        let msgs = vec![UIMessage {
            id: None,
            role: "system".into(),
            parts: vec![raw_part(json!({"type": "text", "text": "You are helpful"}))],
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "You are helpful");
    }

    #[test]
    fn convert_empty_messages() {
        let converted = convert_messages(vec![]);
        assert!(converted.is_empty());
    }

    // ── extract_tool_call_decisions ──

    #[test]
    fn extract_decisions_from_tool_output_parts() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![
                raw_part(json!({
                    "type": "tool-PermissionConfirm",
                    "toolCallId": "perm_call",
                    "state": "output-denied"
                })),
                raw_part(json!({
                    "type": "tool-askUserQuestion",
                    "toolCallId": "ask_call",
                    "state": "output-available",
                    "output": { "message": "continue" }
                })),
            ],
        }];

        let decisions = extract_tool_call_decisions(&msgs);
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].0, "perm_call");
        assert_eq!(decisions[0].1.action, ResumeDecisionAction::Cancel);
        assert_eq!(decisions[1].0, "ask_call");
        assert_eq!(decisions[1].1.action, ResumeDecisionAction::Resume);
        assert_eq!(decisions[1].1.result, json!({ "message": "continue" }));
    }

    // ── dedup_messages ──

    #[test]
    fn dedup_messages_filters_known_ids() {
        let known_ids: HashSet<String> = ["m1", "m2"].iter().map(|s| s.to_string()).collect();
        let msgs = vec![
            UIMessage {
                id: Some("m1".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "old"}))],
            },
            UIMessage {
                id: Some("m2".into()),
                role: "assistant".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "reply"}))],
            },
            UIMessage {
                id: Some("m3".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "new"}))],
            },
        ];
        let new_msgs = dedup_messages(msgs, &known_ids);
        assert_eq!(new_msgs.len(), 1);
        assert_eq!(new_msgs[0].id.as_deref(), Some("m3"));
    }

    #[test]
    fn dedup_messages_keeps_all_when_no_known_ids() {
        let known_ids: HashSet<String> = HashSet::new();
        let msgs = vec![UIMessage {
            id: Some("m1".into()),
            role: "user".into(),
            parts: vec![raw_part(json!({"type": "text", "text": "hello"}))],
        }];
        let new_msgs = dedup_messages(msgs, &known_ids);
        assert_eq!(new_msgs.len(), 1);
    }

    #[test]
    fn dedup_messages_keeps_messages_without_id() {
        let known_ids: HashSet<String> = ["m1"].iter().map(|s| s.to_string()).collect();
        let msgs = vec![
            UIMessage {
                id: Some("m1".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "old"}))],
            },
            UIMessage {
                id: None,
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "no-id"}))],
            },
        ];
        let new_msgs = dedup_messages(msgs, &known_ids);
        assert_eq!(new_msgs.len(), 1);
        assert_eq!(new_msgs[0].id, None);
    }

    // ── ProcessedRequest::is_resume_only ──

    #[test]
    fn is_resume_only_true_when_no_messages_and_has_decisions() {
        let req = ProcessedRequest {
            thread_id: "t1".into(),
            messages: vec![],
            decisions: vec![(
                "tc1".into(),
                ToolCallResume {
                    decision_id: "d1".into(),
                    action: ResumeDecisionAction::Resume,
                    result: json!({"approved": true}),
                    reason: None,
                    updated_at: 0,
                },
            )],
            state: None,
            agent_id: None,
        };
        assert!(req.is_resume_only());
    }

    #[test]
    fn is_resume_only_false_when_has_messages() {
        let req = ProcessedRequest {
            thread_id: "t1".into(),
            messages: vec![awaken_contract::contract::message::Message::user("hello")],
            decisions: vec![(
                "tc1".into(),
                ToolCallResume {
                    decision_id: "d1".into(),
                    action: ResumeDecisionAction::Resume,
                    result: json!({"approved": true}),
                    reason: None,
                    updated_at: 0,
                },
            )],
            state: None,
            agent_id: None,
        };
        assert!(!req.is_resume_only());
    }

    // ── parse_data_uri ──

    #[test]
    fn parse_data_uri_valid() {
        let (mime, data) = parse_data_uri("data:image/jpeg;base64,/9j/4AA").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, "/9j/4AA");
    }

    #[test]
    fn parse_data_uri_invalid() {
        assert!(parse_data_uri("https://example.com/img.png").is_none());
        assert!(parse_data_uri("data:image/png,raw-data").is_none());
    }

    // ── parse_data_uri (extended) ──

    #[test]
    fn parse_data_uri_missing_data_prefix() {
        assert!(parse_data_uri("image/png;base64,abc123").is_none());
    }

    #[test]
    fn parse_data_uri_missing_base64_marker() {
        // Has "data:" prefix and comma, but no ";base64" suffix on meta
        assert!(parse_data_uri("data:image/png,abc123").is_none());
    }

    #[test]
    fn parse_data_uri_missing_comma() {
        assert!(parse_data_uri("data:image/png;base64abc123").is_none());
    }

    #[test]
    fn parse_data_uri_empty_data() {
        let (mime, data) = parse_data_uri("data:image/png;base64,").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(data, "");
    }

    // ── part_kind (extended) ──

    #[test]
    fn part_kind_returns_type_string() {
        let part = json!({"type": "text", "text": "hello"});
        assert_eq!(part_kind(&part), Some("text"));
    }

    #[test]
    fn part_kind_returns_none_without_type() {
        let part = json!({"text": "hello"});
        assert_eq!(part_kind(&part), None);
    }

    #[test]
    fn part_kind_returns_none_when_type_not_string() {
        let part = json!({"type": 42});
        assert_eq!(part_kind(&part), None);
    }

    // ── part_to_content_block (extended) ──

    #[test]
    fn part_to_content_block_text() {
        let part = json!({"type": "text", "text": "hello"});
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(block, ContentBlock::text("hello"));
    }

    #[test]
    fn part_to_content_block_file_data_uri_image() {
        let part = json!({
            "type": "file",
            "url": "data:image/png;base64,abc123",
            "mediaType": "image/png"
        });
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(block, ContentBlock::image_base64("image/png", "abc123"));
    }

    #[test]
    fn part_to_content_block_file_url_image() {
        let part = json!({
            "type": "file",
            "url": "https://example.com/photo.jpg",
            "mediaType": "image/jpeg"
        });
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(
            block,
            ContentBlock::image_url("https://example.com/photo.jpg")
        );
    }

    #[test]
    fn part_to_content_block_file_url_audio() {
        let part = json!({
            "type": "file",
            "url": "https://example.com/song.mp3",
            "mediaType": "audio/mpeg"
        });
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(
            block,
            ContentBlock::audio_url("https://example.com/song.mp3")
        );
    }

    #[test]
    fn part_to_content_block_file_url_video() {
        let part = json!({
            "type": "file",
            "url": "https://example.com/clip.mp4",
            "mediaType": "video/mp4"
        });
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(
            block,
            ContentBlock::video_url("https://example.com/clip.mp4")
        );
    }

    #[test]
    fn part_to_content_block_file_url_document() {
        let part = json!({
            "type": "file",
            "url": "https://example.com/doc.pdf",
            "mediaType": "application/pdf"
        });
        let block = part_to_content_block(&part).unwrap();
        assert_eq!(
            block,
            ContentBlock::document_url("https://example.com/doc.pdf", None)
        );
    }

    #[test]
    fn part_to_content_block_unknown_type_returns_none() {
        let part = json!({"type": "source-url", "sourceId": "s1", "url": "https://example.com"});
        assert!(part_to_content_block(&part).is_none());
    }

    // ── dedup_messages (extended) ──

    #[test]
    fn dedup_messages_empty_known_ids_returns_all() {
        let known_ids: HashSet<String> = HashSet::new();
        let msgs = vec![
            UIMessage {
                id: Some("m1".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "a"}))],
            },
            UIMessage {
                id: Some("m2".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "b"}))],
            },
        ];
        let result = dedup_messages(msgs, &known_ids);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedup_messages_filters_all_known() {
        let known_ids: HashSet<String> = ["m1", "m2"].iter().map(|s| s.to_string()).collect();
        let msgs = vec![
            UIMessage {
                id: Some("m1".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "a"}))],
            },
            UIMessage {
                id: Some("m2".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "b"}))],
            },
        ];
        let result = dedup_messages(msgs, &known_ids);
        assert!(result.is_empty());
    }

    #[test]
    fn dedup_messages_keeps_no_id_messages() {
        let known_ids: HashSet<String> = ["m1"].iter().map(|s| s.to_string()).collect();
        let msgs = vec![
            UIMessage {
                id: None,
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "no id 1"}))],
            },
            UIMessage {
                id: None,
                role: "user".into(),
                parts: vec![raw_part(json!({"type": "text", "text": "no id 2"}))],
            },
        ];
        let result = dedup_messages(msgs, &known_ids);
        assert_eq!(result.len(), 2);
    }

    // ── extract_tool_call_decisions (extended) ──

    #[test]
    fn extract_decisions_output_available_is_resume() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-run",
                "toolCallId": "tc1",
                "state": "output-available",
                "output": "result data"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].0, "tc1");
        assert_eq!(decisions[0].1.action, ResumeDecisionAction::Resume);
        assert_eq!(decisions[0].1.result, json!("result data"));
    }

    #[test]
    fn extract_decisions_output_denied_is_cancel() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-confirm",
                "toolCallId": "tc2",
                "state": "output-denied"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].0, "tc2");
        assert_eq!(decisions[0].1.action, ResumeDecisionAction::Cancel);
        assert_eq!(decisions[0].1.result, json!({"approved": false}));
    }

    #[test]
    fn extract_decisions_ignores_non_tool_parts() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![
                raw_part(json!({"type": "text", "text": "hello"})),
                raw_part(json!({"type": "step-start"})),
            ],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_ignores_user_messages() {
        let msgs = vec![UIMessage {
            id: Some("u1".into()),
            role: "user".into(),
            parts: vec![raw_part(json!({
                "type": "tool-run",
                "toolCallId": "tc1",
                "state": "output-available",
                "output": "data"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_requires_state_field() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-run",
                "toolCallId": "tc1"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_requires_tool_call_id() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-run",
                "state": "output-available",
                "output": "data"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_output_error_is_resume() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-exec",
                "toolCallId": "tc3",
                "state": "output-error",
                "errorText": "something failed"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].1.action, ResumeDecisionAction::Resume);
        assert_eq!(decisions[0].1.result, json!({"error": "something failed"}));
    }

    #[test]
    fn extract_decisions_approval_responded_approved_true() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-approval",
                "toolCallId": "tc4",
                "state": "approval-responded",
                "approval": {"approved": true}
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].1.action, ResumeDecisionAction::Resume);
        assert_eq!(decisions[0].1.result, json!({"approved": true}));
    }

    #[test]
    fn extract_decisions_approval_responded_approved_false() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-approval",
                "toolCallId": "tc5",
                "state": "approval-responded",
                "approval": {"approved": false}
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].1.action, ResumeDecisionAction::Cancel);
        assert_eq!(decisions[0].1.result, json!({"approved": false}));
    }

    #[test]
    fn extract_decisions_unknown_state_ignored() {
        let msgs = vec![UIMessage {
            id: Some("a1".into()),
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-run",
                "toolCallId": "tc6",
                "state": "pending"
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_skips_provider_executed_tools() {
        // AI SDK v6 marks completed tool calls with providerExecuted: true in
        // message history. These must NOT be treated as new resume decisions.
        let msgs = vec![UIMessage {
            id: None,
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-invocation",
                "toolCallId": "hist_call",
                "state": "output-available",
                "output": {"result": "done"},
                "providerExecuted": true,
            }))],
        }];
        let decisions = extract_tool_call_decisions(&msgs);
        assert!(
            decisions.is_empty(),
            "providerExecuted tool results should be skipped, got {decisions:?}"
        );

        // Same part without providerExecuted should still be picked up.
        let msgs2 = vec![UIMessage {
            id: None,
            role: "assistant".into(),
            parts: vec![raw_part(json!({
                "type": "tool-invocation",
                "toolCallId": "new_call",
                "state": "output-available",
                "output": {"result": "pending"},
            }))],
        }];
        let decisions2 = extract_tool_call_decisions(&msgs2);
        assert_eq!(decisions2.len(), 1);
        assert_eq!(decisions2[0].0, "new_call");
    }

    /// Regression: multi-turn conversation with completed tool calls in history.
    ///
    /// Simulates the exact sequence that caused the bug:
    /// Turn 1: user → assistant (with tool call) → tool result (providerExecuted)
    /// Turn 2: user sends follow-up
    ///
    /// Without the fix, the historical tool result creates a spurious decision,
    /// `is_resume_only()` returns true, and the response is empty.
    #[test]
    fn multi_turn_with_provider_executed_history_produces_no_decisions() {
        let msgs = vec![
            // Turn 1: user message
            UIMessage {
                id: Some("msg-1".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({
                    "type": "text",
                    "text": "Show me the fleet status",
                }))],
            },
            // Turn 1: assistant response with tool call (completed)
            UIMessage {
                id: Some("msg-2".into()),
                role: "assistant".into(),
                parts: vec![
                    raw_part(json!({
                        "type": "text",
                        "text": "Let me check the fleet status.",
                    })),
                    raw_part(json!({
                        "type": "tool-invocation",
                        "toolCallId": "call_fleet_1",
                        "toolName": "get_fleet_status",
                        "args": {},
                        "state": "output-available",
                        "output": [{"id": "ship-1", "status": "active"}],
                        "providerExecuted": true,
                    })),
                ],
            },
            // Turn 2: user follow-up
            UIMessage {
                id: Some("msg-3".into()),
                role: "user".into(),
                parts: vec![raw_part(json!({
                    "type": "text",
                    "text": "Show me the anomalies",
                }))],
            },
        ];

        let decisions = extract_tool_call_decisions(&msgs);
        assert!(
            decisions.is_empty(),
            "historical providerExecuted tool calls must not produce decisions, \
             but got {decisions:?} — this would cause is_resume_only() to return \
             true and the response to be empty"
        );
    }

    /// Verify that is_resume_only is false when there are messages but no decisions.
    #[test]
    fn processed_request_with_messages_is_not_resume_only() {
        let req = ProcessedRequest {
            thread_id: "t1".into(),
            messages: vec![Message::user("hello")],
            decisions: vec![],
            state: None,
            agent_id: None,
        };
        assert!(
            !req.is_resume_only(),
            "request with messages should not be resume-only"
        );
    }

    /// Verify that is_resume_only requires both empty messages AND non-empty decisions.
    #[test]
    fn processed_request_empty_messages_empty_decisions_is_not_resume_only() {
        let req = ProcessedRequest {
            thread_id: "t1".into(),
            messages: vec![],
            decisions: vec![],
            state: None,
            agent_id: None,
        };
        assert!(
            !req.is_resume_only(),
            "request with no messages and no decisions should not be resume-only"
        );
    }
}
