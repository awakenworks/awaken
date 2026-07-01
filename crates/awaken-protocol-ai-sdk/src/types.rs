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
    },
    Error {
        #[serde(rename = "errorText")]
        error_text: String,
    },
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

/// An AI SDK v6 `UIMessage`. Parts are kept as raw JSON so tool/data parts (which
/// are UI state, not LLM input) can be inspected without a closed schema.
#[derive(Debug, Clone, Deserialize)]
pub struct UIMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub parts: Vec<Value>,
}

/// The `messages` list echoed by the history endpoint, as UI messages.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryResponse {
    pub messages: Vec<Value>,
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
