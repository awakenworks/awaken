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

/// The AG-UI `RunAgentInput` request body. Protocol-defined extensible JSON is
/// confined to tool schemas/metadata, state, resume payloads and forwarded props;
/// the surrounding contract is fully typed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunAgentInput {
    #[serde(rename = "threadId", default)]
    pub thread_id: Option<String>,
    #[serde(rename = "runId", default)]
    pub run_id: Option<String>,
    #[serde(rename = "parentRunId", default)]
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<AgUiMessage>,
    #[serde(default)]
    pub tools: Vec<AgUiTool>,
    #[serde(default)]
    pub context: Vec<AgUiContext>,
    #[serde(default)]
    pub state: serde_json::Value,
    #[serde(rename = "forwardedProps", default)]
    pub forwarded_props: serde_json::Value,
    #[serde(default)]
    pub resume: Vec<AgUiResumeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgUiTool {
    pub name: String,
    pub description: String,
    /// JSON Schema is defined by the caller's tool.
    #[serde(default)]
    pub parameters: serde_json::Value,
    /// AG-UI explicitly defines metadata as an extension record.
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgUiContext {
    pub description: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgUiResumeEntry {
    #[serde(rename = "interruptId")]
    pub interrupt_id: String,
    pub status: AgUiResumeStatus,
    /// Interrupt payload is application-defined.
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgUiResumeStatus {
    Resolved,
    Cancelled,
}

/// An AG-UI message. Content is a plain string in the common case, or a list of
/// typed content parts when the turn is multimodal; a `tool` message carries the
/// `toolCallId` it answers.
#[derive(Debug, Clone, Deserialize)]
pub struct AgUiMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: AgUiRole,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgUiRole {
    Developer,
    System,
    Assistant,
    User,
    Tool,
    Activity,
    Reasoning,
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
    Text {
        text: String,
    },
    Image {
        source: InputContentSource,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    },
    Audio {
        source: InputContentSource,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    },
    Video {
        source: InputContentSource,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    },
    Document {
        source: InputContentSource,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    },
    Binary {
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        data: Option<String>,
        #[serde(default)]
        filename: Option<String>,
    },
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
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
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

    // BYTE-COMPAT PIN: the AG-UI SDK keys the text-message lifecycle on the exact
    // camelCase field `messageId` (plus `role` on START, `delta` on CONTENT). A
    // rename to snake_case (`message_id`) would break every `HttpAgent` consumer, so
    // pin the literal wire shape of all three TEXT_MESSAGE_* frames.
    #[test]
    fn text_message_frames_pin_camel_case_field_names() {
        assert_eq!(
            serde_json::to_value(AgUiEvent::TextMessageStart {
                message_id: "m1".into(),
                role: "assistant".into(),
            })
            .unwrap(),
            json!({ "type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant" })
        );
        assert_eq!(
            serde_json::to_value(AgUiEvent::TextMessageContent {
                message_id: "m1".into(),
                delta: "hi".into(),
            })
            .unwrap(),
            json!({ "type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "hi" })
        );
        assert_eq!(
            serde_json::to_value(AgUiEvent::TextMessageEnd {
                message_id: "m1".into(),
            })
            .unwrap(),
            json!({ "type": "TEXT_MESSAGE_END", "messageId": "m1" })
        );
    }

    // BYTE-COMPAT PIN: the tool-call lifecycle keys on `toolCallId` throughout and
    // `toolCallName` on START (note: NOT `toolName` — AG-UI's spelling differs from
    // the AI SDK's). A snake_case rename must fail these.
    #[test]
    fn tool_call_frames_pin_camel_case_field_names() {
        assert_eq!(
            serde_json::to_value(AgUiEvent::ToolCallStart {
                tool_call_id: "c1".into(),
                tool_call_name: "read".into(),
            })
            .unwrap(),
            json!({ "type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "read" })
        );
        assert_eq!(
            serde_json::to_value(AgUiEvent::ToolCallArgs {
                tool_call_id: "c1".into(),
                delta: "{\"p\":".into(),
            })
            .unwrap(),
            json!({ "type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{\"p\":" })
        );
        assert_eq!(
            serde_json::to_value(AgUiEvent::ToolCallEnd {
                tool_call_id: "c1".into(),
            })
            .unwrap(),
            json!({ "type": "TOOL_CALL_END", "toolCallId": "c1" })
        );
    }

    // Causal graph: official RunAgentInput -> typed wire DTO -> runtime request ACL.
    //
    // Decision table:
    // | SDK field       | static shell | intentionally dynamic leaf |
    // | tools           | typed        | parameters + metadata      |
    // | context         | typed        | none                       |
    // | state           | typed slot   | complete value             |
    // | forwardedProps  | typed slot   | complete value             |
    // | resume          | typed        | payload                    |
    #[test]
    fn run_agent_input_retains_every_official_top_level_field() {
        let input: RunAgentInput = serde_json::from_value(json!({
            "threadId": "t1",
            "runId": "r1",
            "messages": [{ "role": "user", "content": "go" }],
            "parentRunId": "parent-1",
            "tools": [{ "name": "read", "description": "read", "parameters": {"type":"object"}, "metadata":{"ui":"x"} }],
            "context": [{ "description": "d", "value": "v" }],
            "state": { "arbitrary": "blob" },
            "forwardedProps": { "anything": true },
            "resume": [{"interruptId":"i1","status":"resolved","payload":{"answer":42}}],
        }))
        .expect("the official AG-UI input shape parses");
        assert_eq!(input.thread_id.as_deref(), Some("t1"));
        assert_eq!(input.run_id.as_deref(), Some("r1"));
        assert_eq!(input.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(input.messages.len(), 1);
        assert_eq!(input.messages[0].role, AgUiRole::User);
        assert_eq!(input.tools[0].name, "read");
        assert_eq!(input.tools[0].parameters["type"], "object");
        assert_eq!(input.context[0].value, "v");
        assert_eq!(input.state["arbitrary"], "blob");
        assert_eq!(input.forwarded_props["anything"], true);
        assert_eq!(input.resume[0].interrupt_id, "i1");
        assert_eq!(input.resume[0].payload["answer"], 42);
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
