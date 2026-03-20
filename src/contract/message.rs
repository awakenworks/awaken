//! Core types for agent messages and tool calls.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Message visibility — controls whether a message is exposed to external API consumers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Visible to both the user and the LLM.
    #[default]
    All,
    /// Only visible to the LLM, hidden from external API consumers.
    Internal,
}

impl Visibility {
    /// Returns `true` if this is the default visibility (`All`).
    pub fn is_default(&self) -> bool {
        *self == Visibility::All
    }
}

/// Optional metadata associating a message with a run and step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMetadata {
    /// The run that produced this message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Step (round) index within the run (0-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
}

/// Generate a time-ordered UUID v7 message identifier.
pub fn gen_message_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Stable message identifier (UUID v7, auto-generated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: Role,
    pub content: String,
    /// Tool calls made by the assistant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Tool call ID this message responds to (for tool role).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Message visibility. Defaults to `All` (visible everywhere).
    /// Internal messages (e.g. system reminders) are only sent to the LLM.
    #[serde(default, skip_serializing_if = "Visibility::is_default")]
    pub visibility: Visibility,
    /// Optional run/step association metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MessageMetadata>,
}

impl Message {
    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::System,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an internal system message (visible only to LLM, hidden from API consumers).
    pub fn internal_system(content: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::System,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::Internal,
            metadata: None,
        }
    }

    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an assistant message with tool calls.
    pub fn assistant_with_tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Assistant,
            content: content.into(),
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a tool response message.
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Override the auto-generated message ID.
    #[must_use]
    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    /// Attach run/step metadata to this message.
    #[must_use]
    pub fn with_metadata(mut self, metadata: MessageMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Name of the tool to call.
    pub name: String,
    /// Arguments for the tool as JSON.
    pub arguments: Value,
}

impl ToolCall {
    /// Create a new tool call.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_user_message() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "Hello");
        assert!(msg.id.is_some());
        assert!(msg.tool_calls.is_none());
        assert!(msg.tool_call_id.is_none());
        assert!(msg.metadata.is_none());
    }

    #[test]
    fn test_all_constructors_generate_uuid_v7_id() {
        let msgs = vec![
            Message::system("sys"),
            Message::internal_system("internal"),
            Message::user("usr"),
            Message::assistant("asst"),
            Message::assistant_with_tool_calls("tc", vec![]),
            Message::tool("c1", "result"),
        ];
        for msg in &msgs {
            let id = msg.id.as_ref().expect("message should have an id");
            assert_eq!(id.len(), 36, "id should be UUID format: {}", id);
            assert_eq!(&id[14..15], "7", "UUID version should be 7: {}", id);
        }
        let ids: std::collections::HashSet<&str> =
            msgs.iter().map(|m| m.id.as_deref().unwrap()).collect();
        assert_eq!(ids.len(), msgs.len());
    }

    #[test]
    fn test_assistant_with_tool_calls() {
        let calls = vec![ToolCall::new("call_1", "search", json!({"query": "rust"}))];
        let msg = Message::assistant_with_tool_calls("Let me search", calls);

        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "Let me search");
        assert!(msg.tool_calls.is_some());
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_tool_message() {
        let msg = Message::tool("call_1", "Result: 42");

        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.content, "Result: 42");
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message::user("test");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(!json.contains("tool_calls"));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn test_message_with_metadata_serialization() {
        let msg = Message::user("test").with_metadata(MessageMetadata {
            run_id: Some("run-1".to_string()),
            step_index: Some(3),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"run_id\":\"run-1\""));
        assert!(json.contains("\"step_index\":3"));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        let meta = parsed.metadata.unwrap();
        assert_eq!(meta.run_id.as_deref(), Some("run-1"));
        assert_eq!(meta.step_index, Some(3));
    }

    #[test]
    fn test_message_without_metadata_deserializes() {
        let json = r#"{"id":"abc","role":"user","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(msg.metadata.is_none());
        assert_eq!(msg.visibility, Visibility::All);
    }

    #[test]
    fn test_tool_call_serialization() {
        let call = ToolCall::new("id_1", "calculator", json!({"expr": "2+2"}));
        let json = serde_json::to_string(&call).unwrap();
        let parsed: ToolCall = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "id_1");
        assert_eq!(parsed.name, "calculator");
        assert_eq!(parsed.arguments["expr"], "2+2");
    }

    #[test]
    fn test_with_id_overrides_auto_generated() {
        let msg = Message::user("hi").with_id("custom-id".to_string());
        assert_eq!(msg.id.as_deref(), Some("custom-id"));
    }

    #[test]
    fn test_gen_message_id_is_public_and_uuid_v7() {
        let id = gen_message_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
    }
}
