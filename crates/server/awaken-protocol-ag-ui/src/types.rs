//! The AG-UI protocol wire types (anti-corruption boundary).
//!
//! The only place AG-UI (Agent User Interaction Protocol) vocabulary lives.
//! Outbound events are the `type`-tagged SCREAMING_SNAKE_CASE events an AG-UI
//! `HttpAgent` consumes over SSE; inbound is the `RunAgentInput` request body.

use serde::{Deserialize, Serialize};

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
    /// The AG-UI `ToolMessage.error` field (`error?: string`): the failure message
    /// when a client-executed tool failed, or a denied built-in tool approval.
    /// Absent for a plain result / an approval.
    #[serde(default)]
    pub error: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_error_wire_shape() {
        assert_eq!(
            serde_json::to_value(AgUiEvent::error("boom")).unwrap(),
            json!({ "type": "RUN_ERROR", "message": "boom" })
        );
    }

    #[test]
    fn run_started_wire_shape() {
        let ev = AgUiEvent::RunStarted {
            thread_id: "t1".into(),
            run_id: "r1".into(),
        };
        assert_eq!(
            serde_json::to_value(ev).unwrap(),
            json!({ "type": "RUN_STARTED", "threadId": "t1", "runId": "r1" })
        );
    }

    #[test]
    fn tool_call_result_wire_shape() {
        let ev = AgUiEvent::ToolCallResult {
            message_id: "m1".into(),
            tool_call_id: "c1".into(),
            content: "42".into(),
        };
        assert_eq!(
            serde_json::to_value(ev).unwrap(),
            json!({ "type": "TOOL_CALL_RESULT", "messageId": "m1", "toolCallId": "c1", "content": "42" })
        );
    }
}
