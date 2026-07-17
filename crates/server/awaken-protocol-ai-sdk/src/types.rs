//! The AI SDK v6 UI Message Stream wire types (anti-corruption boundary).
//!
//! These are the only place Vercel AI SDK protocol vocabulary lives. Outbound
//! frames are the UI Message Stream parts the `useChat` transport consumes;
//! inbound is the `DefaultChatTransport` request body. Message content reuses the
//! neutral [`ContentBlock`], which already serializes SDK-compatibly.

use awaken_agent_contract::agent::content::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One UI Message Stream part. Serializes as `{ "type": "text-delta", ... }`,
/// byte-compatible with the AI SDK v6 `ai` package.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum UIStreamEvent {
    Start,
    StartStep,
    TextStart {
        id: String,
    },
    TextDelta {
        id: String,
        delta: String,
    },
    TextEnd {
        id: String,
    },
    /// A tool call has begun streaming its input, before any argument bytes.
    /// `useChat` opens an `input-streaming` tool part on this frame.
    ToolInputStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    /// An incremental fragment of a tool call's input JSON, as the model streams
    /// it. `inputTextDelta` is a raw text delta the SDK concatenates; the final
    /// authoritative input arrives in the later `ToolInputAvailable`.
    ToolInputDelta {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "inputTextDelta")]
        input_text_delta: String,
    },
    ToolInputAvailable {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        input: Value,
        #[serde(rename = "providerExecuted")]
        provider_executed: bool,
    },
    ToolOutputAvailable {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        output: Value,
    },
    ToolOutputError {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "errorText")]
        error_text: String,
    },
    FinishStep,
    Finish {
        #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        // The UI-message-stream `finish` chunk carries app-defined `messageMetadata`
        // (usage is not a top-level field here — that is `streamText`'s TextStreamPart).
        #[serde(rename = "messageMetadata", skip_serializing_if = "Option::is_none")]
        message_metadata: Option<FinishMetadata>,
    },
    Error {
        #[serde(rename = "errorText")]
        error_text: String,
    },
}

/// The `messageMetadata` we attach to the `finish` chunk: the run's token
/// accounting under `totalUsage`, the key the AI SDK client surfaces on the message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FinishMetadata {
    #[serde(rename = "totalUsage")]
    pub total_usage: Usage,
}

/// Token accounting matching the AI SDK `LanguageModelUsage` shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
}

impl UIStreamEvent {
    pub fn error(message: impl Into<String>) -> Self {
        UIStreamEvent::Error {
            error_text: message.into(),
        }
    }

    pub fn finish(reason: &str) -> Self {
        UIStreamEvent::Finish {
            finish_reason: Some(reason.to_string()),
            message_metadata: None,
        }
    }
}

/// Attach `(input, output)` token usage to the terminal `finish` chunk's
/// `messageMetadata.totalUsage` — the AI-SDK-standard slot for run usage.
pub fn attach_usage(events: &mut [UIStreamEvent], usage: (u64, u64)) {
    for event in events.iter_mut() {
        if let UIStreamEvent::Finish {
            message_metadata, ..
        } = event
        {
            *message_metadata = Some(FinishMetadata {
                total_usage: Usage {
                    input_tokens: usage.0,
                    output_tokens: usage.1,
                    total_tokens: usage.0 + usage.1,
                },
            });
        }
    }
}

/// `DefaultChatTransport` request body (`prepareSendMessagesRequest` sends
/// `{ threadId, messages }`; `agentId` arrives via the route path).
#[derive(Debug, Clone, Deserialize)]
pub struct AiSdkChatRequest {
    #[serde(default)]
    pub messages: Vec<UIMessage>,
    #[serde(rename = "threadId", alias = "thread_id", default)]
    pub thread_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    pub agent_id: Option<String>,
}

/// An AI SDK v6 `UIMessage`. The `parts` list stays raw JSON at the container
/// level because a tool part's `type` is the dynamic `tool-<name>` — not a closed
/// set — but each part is decoded into a typed view ([`UIPart`] for content,
/// [`ToolDecisionPart`] for tool decisions) at the parsing boundary.
#[derive(Debug, Clone, Deserialize)]
pub struct UIMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub parts: Vec<Value>,
}

/// A typed view of a user/system message content part. Non-content kinds
/// (`reasoning`, `step-start`, dynamic `tool-*`) decode to [`UIPart::Other`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum UIPart {
    Text {
        text: String,
    },
    /// AI SDK v5 file part: `{ type: "file", mediaType, url }`, where `url` is a
    /// `data:` URI or a remote link.
    File {
        #[serde(rename = "mediaType")]
        media_type: String,
        url: String,
    },
    #[serde(other)]
    Other,
}

/// A typed view of an assistant tool part carrying a client's decision. The
/// `type` is the dynamic `tool-<name>`, so it is read as a string, not enumerated.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolDecisionPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: Option<String>,
    pub state: Option<String>,
    #[serde(default, rename = "providerExecuted")]
    pub provider_executed: bool,
    pub output: Option<Value>,
    #[serde(rename = "errorText")]
    pub error_text: Option<String>,
    pub approval: Option<ApprovalResponse>,
}

/// A client's answer to a tool-approval request.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalResponse {
    #[serde(default)]
    pub approved: bool,
}

/// A history UI message: `{ id, role, parts }`.
pub fn history_message(id: &str, role: &str, parts: Vec<Value>) -> Value {
    serde_json::json!({ "id": id, "role": role, "parts": parts })
}

/// Convert neutral content blocks to UI text parts (only text survives; images
/// and tool blocks are handled by the caller).
pub fn text_parts(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => {
                Some(serde_json::json!({ "type": "text", "text": text }))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_input_start_wire_shape() {
        let ev = UIStreamEvent::ToolInputStart {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "tool-input-start", "toolCallId": "c1", "toolName": "read" })
        );
    }

    #[test]
    fn tool_input_delta_wire_shape() {
        let ev = UIStreamEvent::ToolInputDelta {
            tool_call_id: "c1".into(),
            input_text_delta: "{\"pa".into(),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "tool-input-delta", "toolCallId": "c1", "inputTextDelta": "{\"pa" })
        );
    }

    #[test]
    fn finish_with_usage_wire_shape() {
        // Usage rides in `messageMetadata.totalUsage` on the UI-message-stream
        // finish chunk (the AI-SDK-standard slot), not as a top-level field.
        let ev = UIStreamEvent::Finish {
            finish_reason: Some("stop".into()),
            message_metadata: Some(FinishMetadata {
                total_usage: Usage {
                    input_tokens: 3,
                    output_tokens: 5,
                    total_tokens: 8,
                },
            }),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({
                "type": "finish",
                "finishReason": "stop",
                "messageMetadata": {
                    "totalUsage": { "inputTokens": 3, "outputTokens": 5, "totalTokens": 8 }
                }
            })
        );
    }

    #[test]
    fn finish_without_usage_omits_the_field() {
        let ev = UIStreamEvent::finish("stop");
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "finish", "finishReason": "stop" })
        );
    }

    // BYTE-COMPAT PIN: `ToolInputAvailable` is the authoritative committed tool
    // call the `useChat` transport keys on. The exact camelCase field spellings —
    // `toolCallId`, `toolName`, and especially `providerExecuted` (the flag that
    // decides whether the client runs the tool or renders a server result) — are
    // load-bearing. A snake_case regression (`provider_executed`, `tool_call_id`)
    // would silently break every AI SDK client, so pin the literal wire shape.
    #[test]
    fn tool_input_available_wire_shape_pins_provider_executed() {
        let ev = UIStreamEvent::ToolInputAvailable {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            input: json!({ "path": "x" }),
            provider_executed: true,
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({
                "type": "tool-input-available",
                "toolCallId": "c1",
                "toolName": "read",
                "input": { "path": "x" },
                "providerExecuted": true,
            })
        );
    }

    // BYTE-COMPAT PIN: `ToolOutputAvailable` carries the tool result under the exact
    // `tool-output-available` tag with camelCase `toolCallId` + `output`. A
    // snake_case rename must fail this.
    #[test]
    fn tool_output_available_wire_shape() {
        let ev = UIStreamEvent::ToolOutputAvailable {
            tool_call_id: "c1".into(),
            output: json!({ "ok": true }),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "tool-output-available", "toolCallId": "c1", "output": { "ok": true } })
        );
    }

    // BYTE-COMPAT PIN: the step-boundary frame serializes as `"finish-step"` (the
    // kebab-case rename of the `FinishStep` variant), NOT `"finish"`. The AI SDK
    // treats `finish-step` (end of one step in a multi-step turn) and `finish` (end
    // of the whole message) as distinct chunks; collapsing them would truncate the
    // stream. Pin both the positive tag and the negative (not `finish`).
    #[test]
    fn finish_step_serializes_as_finish_step_not_finish() {
        let value = serde_json::to_value(UIStreamEvent::FinishStep).unwrap();
        assert_eq!(value, json!({ "type": "finish-step" }));
        assert_ne!(
            value["type"], "finish",
            "finish-step must not collapse to finish"
        );
    }

    #[test]
    fn tool_output_error_wire_shape() {
        let ev = UIStreamEvent::ToolOutputError {
            tool_call_id: "c1".into(),
            error_text: "boom".into(),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "tool-output-error", "toolCallId": "c1", "errorText": "boom" })
        );
    }

    #[test]
    fn error_wire_shape() {
        let ev = UIStreamEvent::error("nope");
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "error", "errorText": "nope" })
        );
    }

    #[test]
    fn text_parts_keeps_only_text_blocks() {
        use awaken_agent_contract::agent::content::ContentBlock;
        let parts = text_parts(&[
            ContentBlock::text("hi"),
            ContentBlock::image_url("https://x/y.png"),
        ]);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], json!({ "type": "text", "text": "hi" }));
    }
}
