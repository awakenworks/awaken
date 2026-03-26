//! Core types for agent messages and tool calls.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::ContentBlock;

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
    /// Multimodal content blocks.
    pub content: Vec<ContentBlock>,
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
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::contract::message::Message;
    ///
    /// let msg = Message::system("You are helpful");
    /// assert_eq!(msg.text(), "You are helpful");
    /// ```
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an internal system message (visible only to LLM, hidden from API consumers).
    pub fn internal_system(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::Internal,
            metadata: None,
        }
    }

    /// Create a user message with text.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::contract::message::Message;
    ///
    /// let msg = Message::user("Hello");
    /// assert_eq!(msg.text(), "Hello");
    /// ```
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a user message with multimodal content blocks.
    pub fn user_with_content(content: Vec<ContentBlock>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::User,
            content,
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create an assistant message with tool calls.
    pub fn assistant_with_tool_calls(text: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a tool response message with text.
    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Tool,
            content: vec![ContentBlock::text(text)],
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Create a tool response message with multimodal content.
    pub fn tool_with_content(call_id: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self {
            id: Some(gen_message_id()),
            role: Role::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            visibility: Visibility::All,
            metadata: None,
        }
    }

    /// Extract concatenated text from content blocks.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::contract::message::Message;
    ///
    /// let msg = Message::user("Hello world");
    /// assert_eq!(msg.text(), "Hello world");
    /// ```
    pub fn text(&self) -> String {
        super::content::extract_text(&self.content)
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
        assert_eq!(msg.text(), "Hello");
        assert!(msg.id.is_some());
    }

    #[test]
    fn test_user_with_multimodal_content() {
        let msg = Message::user_with_content(vec![
            ContentBlock::text("Look at this:"),
            ContentBlock::image_url("https://example.com/img.png"),
        ]);
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.text(), "Look at this:");
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
            assert_eq!(id.len(), 36, "id should be UUID format: {id}");
            assert_eq!(&id[14..15], "7", "UUID version should be 7: {id}");
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
        assert_eq!(msg.text(), "Let me search");
        assert!(msg.tool_calls.is_some());
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_tool_message() {
        let msg = Message::tool("call_1", "Result: 42");
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.text(), "Result: 42");
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

    #[test]
    fn test_system_message() {
        let msg = Message::system("You are helpful");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.text(), "You are helpful");
        assert_eq!(msg.visibility, Visibility::All);
    }

    #[test]
    fn test_internal_system_message() {
        let msg = Message::internal_system("hidden reminder");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.text(), "hidden reminder");
        assert_eq!(msg.visibility, Visibility::Internal);
    }

    #[test]
    fn test_assistant_with_empty_tool_calls_omits_field() {
        let msg = Message::assistant_with_tool_calls("No tools", vec![]);
        assert!(msg.tool_calls.is_none());
        assert_eq!(msg.text(), "No tools");
    }

    #[test]
    fn test_tool_with_content_blocks() {
        let msg = Message::tool_with_content(
            "call_1",
            vec![ContentBlock::text("part 1"), ContentBlock::text("part 2")],
        );
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.text(), "part 1part 2");
    }

    #[test]
    fn test_message_full_serde_roundtrip_with_tool_calls() {
        let calls = vec![
            ToolCall::new("call_1", "search", json!({"query": "rust"})),
            ToolCall::new("call_2", "fetch", json!({"url": "https://example.com"})),
        ];
        let msg = Message::assistant_with_tool_calls("Multi-tool call", calls);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.role, Role::Assistant);
        assert_eq!(parsed.text(), "Multi-tool call");
        let tc = parsed.tool_calls.unwrap();
        assert_eq!(tc.len(), 2);
        assert_eq!(tc[0].id, "call_1");
        assert_eq!(tc[0].name, "search");
        assert_eq!(tc[1].id, "call_2");
        assert_eq!(tc[1].name, "fetch");
    }

    #[test]
    fn test_tool_message_serde_roundtrip() {
        let msg = Message::tool("call_1", r#"{"result": "hello"}"#);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.role, Role::Tool);
        assert_eq!(parsed.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(parsed.text(), r#"{"result": "hello"}"#);
    }

    #[test]
    fn test_visibility_serde_roundtrip() {
        for vis in [Visibility::All, Visibility::Internal] {
            let json = serde_json::to_string(&vis).unwrap();
            let parsed: Visibility = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, vis);
        }
    }

    #[test]
    fn test_visibility_default_is_all() {
        assert_eq!(Visibility::default(), Visibility::All);
        assert!(Visibility::All.is_default());
        assert!(!Visibility::Internal.is_default());
    }

    #[test]
    fn test_role_serde_roundtrip() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let parsed: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn test_internal_message_omits_visibility_default() {
        let msg = Message::user("visible");
        let json = serde_json::to_string(&msg).unwrap();
        // Default visibility (All) should be omitted
        assert!(!json.contains("visibility"));

        let internal = Message::internal_system("hidden");
        let json = serde_json::to_string(&internal).unwrap();
        assert!(json.contains("\"visibility\":\"internal\""));
    }

    #[test]
    fn test_message_metadata_default_omits_empty() {
        let meta = MessageMetadata::default();
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("run_id"));
        assert!(!json.contains("step_index"));
    }

    // ── Backward compatibility tests (migrated from uncarve) ──

    #[test]
    fn test_message_without_metadata_deserializes() {
        let json = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, Role::User);
        assert!(msg.metadata.is_none());
        assert!(msg.id.is_none());
        assert_eq!(msg.visibility, Visibility::All);
    }

    // ── Role serialization value tests ──

    #[test]
    fn test_role_serialization_values() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }

    // ── Visibility serialization value tests ──

    #[test]
    fn test_visibility_serialization_values() {
        assert_eq!(serde_json::to_string(&Visibility::All).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::to_string(&Visibility::Internal).unwrap(),
            "\"internal\""
        );
    }

    // ── Message clone and debug tests ──

    #[test]
    fn test_message_clone() {
        let msg = Message::user("hello");
        let cloned = msg.clone();
        assert_eq!(cloned.role, Role::User);
        assert_eq!(cloned.text(), "hello");
        assert_eq!(cloned.id, msg.id);
    }

    #[test]
    fn test_message_debug() {
        let msg = Message::user("hello");
        let debug = format!("{:?}", msg);
        assert!(debug.contains("Message"));
        assert!(debug.contains("User"));
    }

    // ── ToolCall tests ──

    #[test]
    fn test_tool_call_clone() {
        let call = ToolCall::new("id_1", "search", json!({"q": "rust"}));
        let cloned = call.clone();
        assert_eq!(cloned.id, "id_1");
        assert_eq!(cloned.name, "search");
        assert_eq!(cloned.arguments, json!({"q": "rust"}));
    }

    #[test]
    fn test_tool_call_debug() {
        let call = ToolCall::new("id_1", "search", json!({}));
        let debug = format!("{:?}", call);
        assert!(debug.contains("ToolCall"));
        assert!(debug.contains("search"));
    }

    // ── MessageMetadata tests ──

    #[test]
    fn test_message_metadata_serde_roundtrip() {
        let meta = MessageMetadata {
            run_id: Some("run-1".into()),
            step_index: Some(5),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: MessageMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, meta);
    }

    #[test]
    fn test_message_metadata_partial_fields() {
        let json = r#"{"run_id":"r1"}"#;
        let meta: MessageMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.run_id.as_deref(), Some("r1"));
        assert!(meta.step_index.is_none());
    }

    // ── Message text extraction tests ──

    #[test]
    fn test_message_text_multiblock() {
        let msg = Message::tool_with_content(
            "c1",
            vec![ContentBlock::text("first"), ContentBlock::text("second")],
        );
        assert_eq!(msg.text(), "firstsecond");
    }

    #[test]
    fn test_message_text_empty_content() {
        let msg = Message {
            id: None,
            role: Role::User,
            content: vec![],
            tool_calls: None,
            tool_call_id: None,
            visibility: Visibility::All,
            metadata: None,
        };
        assert_eq!(msg.text(), "");
    }
}
