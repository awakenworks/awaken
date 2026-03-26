//! AG-UI Protocol event types.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// AG-UI message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// AG-UI multimodal input content source (draft spec).
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
        #[serde(rename = "mimeType")]
        mime_type: Option<String>,
    },
}

/// AG-UI multimodal input content part (draft spec).
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
}

/// AG-UI interrupt payload (draft spec).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InterruptPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

/// Common fields for all AG-UI events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BaseEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    #[serde(rename = "rawEvent", skip_serializing_if = "Option::is_none")]
    pub raw_event: Option<Value>,
}

/// AG-UI Protocol Events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "RUN_STARTED")]
    RunStarted {
        #[serde(rename = "threadId")]
        thread_id: String,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "parentRunId", skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<String>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "RUN_FINISHED")]
    RunFinished {
        #[serde(rename = "threadId")]
        thread_id: String,
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupt: Option<InterruptPayload>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "RUN_ERROR")]
    RunError {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "STEP_STARTED")]
    StepStarted {
        #[serde(rename = "stepName")]
        step_name: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "STEP_FINISHED")]
    StepFinished {
        #[serde(rename = "stepName")]
        step_name: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TEXT_MESSAGE_START")]
    TextMessageStart {
        #[serde(rename = "messageId")]
        message_id: String,
        role: Role,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TEXT_MESSAGE_CONTENT")]
    TextMessageContent {
        #[serde(rename = "messageId")]
        message_id: String,
        delta: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TEXT_MESSAGE_END")]
    TextMessageEnd {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "REASONING_MESSAGE_START")]
    ReasoningMessageStart {
        #[serde(rename = "messageId")]
        message_id: String,
        role: Role,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "REASONING_MESSAGE_CONTENT")]
    ReasoningMessageContent {
        #[serde(rename = "messageId")]
        message_id: String,
        delta: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "REASONING_MESSAGE_END")]
    ReasoningMessageEnd {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "REASONING_ENCRYPTED_VALUE")]
    ReasoningEncryptedValue {
        #[serde(rename = "entityId")]
        entity_id: String,
        #[serde(rename = "encryptedValue")]
        encrypted_value: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TOOL_CALL_START")]
    ToolCallStart {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolCallName")]
        tool_call_name: String,
        #[serde(rename = "parentMessageId", skip_serializing_if = "Option::is_none")]
        parent_message_id: Option<String>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TOOL_CALL_ARGS")]
    ToolCallArgs {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        delta: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TOOL_CALL_END")]
    ToolCallEnd {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "TOOL_CALL_RESULT")]
    ToolCallResult {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        role: Option<Role>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "STATE_SNAPSHOT")]
    StateSnapshot {
        snapshot: Value,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "STATE_DELTA")]
    StateDelta {
        delta: Vec<Value>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "MESSAGES_SNAPSHOT")]
    MessagesSnapshot {
        messages: Vec<Value>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "ACTIVITY_SNAPSHOT")]
    ActivitySnapshot {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "activityType")]
        activity_type: String,
        content: HashMap<String, Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        replace: Option<bool>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "ACTIVITY_DELTA")]
    ActivityDelta {
        #[serde(rename = "messageId")]
        message_id: String,
        #[serde(rename = "activityType")]
        activity_type: String,
        patch: Vec<Value>,
        #[serde(flatten)]
        base: BaseEvent,
    },

    #[serde(rename = "CUSTOM")]
    Custom {
        name: String,
        value: Value,
        #[serde(flatten)]
        base: BaseEvent,
    },
}

impl Event {
    pub fn run_started(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        parent_run_id: Option<String>,
    ) -> Self {
        Self::RunStarted {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            parent_run_id,
            base: BaseEvent::default(),
        }
    }

    pub fn run_finished(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        result: Option<Value>,
    ) -> Self {
        Self::RunFinished {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            result,
            outcome: None,
            interrupt: None,
            base: BaseEvent::default(),
        }
    }

    pub fn run_interrupted(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        interrupt: InterruptPayload,
    ) -> Self {
        Self::RunFinished {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            result: None,
            outcome: Some("interrupt".into()),
            interrupt: Some(interrupt),
            base: BaseEvent::default(),
        }
    }

    pub fn run_error(message: impl Into<String>, code: Option<String>) -> Self {
        Self::RunError {
            message: message.into(),
            code,
            base: BaseEvent::default(),
        }
    }

    pub fn step_started(step_name: impl Into<String>) -> Self {
        Self::StepStarted {
            step_name: step_name.into(),
            base: BaseEvent::default(),
        }
    }

    pub fn step_finished(step_name: impl Into<String>) -> Self {
        Self::StepFinished {
            step_name: step_name.into(),
            base: BaseEvent::default(),
        }
    }

    pub fn text_message_start(message_id: impl Into<String>) -> Self {
        Self::TextMessageStart {
            message_id: message_id.into(),
            role: Role::Assistant,
            base: BaseEvent::default(),
        }
    }

    pub fn text_message_content(message_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::TextMessageContent {
            message_id: message_id.into(),
            delta: delta.into(),
            base: BaseEvent::default(),
        }
    }

    pub fn text_message_end(message_id: impl Into<String>) -> Self {
        Self::TextMessageEnd {
            message_id: message_id.into(),
            base: BaseEvent::default(),
        }
    }

    pub fn tool_call_start(
        tool_call_id: impl Into<String>,
        tool_call_name: impl Into<String>,
        parent_message_id: Option<String>,
    ) -> Self {
        Self::ToolCallStart {
            tool_call_id: tool_call_id.into(),
            tool_call_name: tool_call_name.into(),
            parent_message_id,
            base: BaseEvent::default(),
        }
    }

    pub fn tool_call_args(tool_call_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self::ToolCallArgs {
            tool_call_id: tool_call_id.into(),
            delta: delta.into(),
            base: BaseEvent::default(),
        }
    }

    pub fn tool_call_end(tool_call_id: impl Into<String>) -> Self {
        Self::ToolCallEnd {
            tool_call_id: tool_call_id.into(),
            base: BaseEvent::default(),
        }
    }

    pub fn tool_call_result(
        message_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::ToolCallResult {
            message_id: message_id.into(),
            tool_call_id: tool_call_id.into(),
            content: content.into(),
            role: Some(Role::Tool),
            base: BaseEvent::default(),
        }
    }

    pub fn state_snapshot(snapshot: Value) -> Self {
        Self::StateSnapshot {
            snapshot,
            base: BaseEvent::default(),
        }
    }

    pub fn state_delta(delta: Vec<Value>) -> Self {
        Self::StateDelta {
            delta,
            base: BaseEvent::default(),
        }
    }

    pub fn messages_snapshot(messages: Vec<Value>) -> Self {
        Self::MessagesSnapshot {
            messages,
            base: BaseEvent::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_started_serde_roundtrip() {
        let event = Event::run_started("t1", "r1", None);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("RUN_STARTED"));
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn run_finished_serde_roundtrip() {
        let event = Event::run_finished("t1", "r1", Some(json!({"ok": true})));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn text_message_events_serde() {
        let start = Event::text_message_start("m1");
        let content = Event::text_message_content("m1", "hello");
        let end = Event::text_message_end("m1");

        for event in [start, content, end] {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn tool_call_events_serde() {
        let start = Event::tool_call_start("c1", "search", None);
        let args = Event::tool_call_args("c1", r#"{"q":"rust"}"#);
        let end = Event::tool_call_end("c1");
        let result = Event::tool_call_result("m1", "c1", "42");

        for event in [start, args, end, result] {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn state_events_serde() {
        let snapshot = Event::state_snapshot(json!({"key": "val"}));
        let delta = Event::state_delta(vec![json!({"op": "add", "path": "/x", "value": 1})]);

        for event in [snapshot, delta] {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn run_error_serde() {
        let event = Event::run_error("something failed", Some("E001".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("RUN_ERROR"));
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}
