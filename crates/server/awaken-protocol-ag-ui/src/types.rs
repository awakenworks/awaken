//! The AG-UI protocol wire types (anti-corruption boundary).
//!
//! The only place AG-UI (Agent User Interaction Protocol) vocabulary lives.
//! Outbound events are the `type`-tagged SCREAMING_SNAKE_CASE events an AG-UI
//! `HttpAgent` consumes over SSE; inbound is the `RunAgentInput` request body.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One AG-UI event. Serializes as `{ "type": "RUN_STARTED", ... }`, matching the
/// AG-UI SDK event schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum AgUiEvent {
    #[serde(rename = "RUN_STARTED")]
    RunStarted {
        #[serde(rename = "threadId")]
        thread_id: String,
        #[serde(rename = "runId")]
        run_id: String,
    },
    #[serde(rename = "RUN_FINISHED")]
    RunFinished {
        #[serde(rename = "threadId")]
        thread_id: String,
        #[serde(rename = "runId")]
        run_id: String,
    },
    #[serde(rename = "RUN_ERROR")]
    RunError { message: String },
    #[serde(rename = "TEXT_MESSAGE_START")]
    TextMessageStart {
        #[serde(rename = "messageId")]
        message_id: String,
        role: String,
    },
    #[serde(rename = "TEXT_MESSAGE_CONTENT")]
    TextMessageContent {
        #[serde(rename = "messageId")]
        message_id: String,
        delta: String,
    },
    #[serde(rename = "TEXT_MESSAGE_END")]
    TextMessageEnd {
        #[serde(rename = "messageId")]
        message_id: String,
    },
    #[serde(rename = "TOOL_CALL_START")]
    ToolCallStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolCallName")]
        tool_call_name: String,
    },
    #[serde(rename = "TOOL_CALL_ARGS")]
    ToolCallArgs {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        delta: String,
    },
    #[serde(rename = "TOOL_CALL_END")]
    ToolCallEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
    },
    #[serde(rename = "TOOL_CALL_RESULT")]
    ToolCallResult {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: String,
    },
}

impl AgUiEvent {
    pub fn error(message: impl Into<String>) -> Self {
        AgUiEvent::RunError {
            message: message.into(),
        }
    }
}

/// The AG-UI `RunAgentInput` request body (only the fields the runtime slice
/// reads; unknown fields — `tools`, `context`, `state`, `forwardedProps` — are
/// ignored so the full input is accepted).
#[derive(Debug, Clone, Deserialize)]
pub struct RunAgentInput {
    #[serde(rename = "threadId", default)]
    pub thread_id: Option<String>,
    #[serde(rename = "runId", default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<AgUiMessage>,
}

/// An AG-UI message. Content is a plain string in the common case, or a list of
/// typed content parts when the turn is multimodal; a `tool` message carries the
/// `toolCallId` it answers.
#[derive(Debug, Clone, Deserialize)]
pub struct AgUiMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub content: Option<AgUiContent>,
    #[serde(rename = "toolCallId", default)]
    pub tool_call_id: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "toolCalls", default)]
    pub tool_calls: Vec<Value>,
}

/// Message content: the plain-string form, or a multimodal list of typed parts.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AgUiContent {
    Text(String),
    Parts(Vec<InputContentPart>),
}

/// One inbound multimodal content part.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputContentPart {
    Text { text: String },
    Image { source: InputContentSource },
}

/// Where an image part's bytes come from: inline base64 or a remote URL.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputContentSource {
    Data {
        value: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Url {
        value: String,
    },
}
