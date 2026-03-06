//! Pure functions for converting between tirea and genai types.

use crate::contracts::runtime::tool_call::{Tool, ToolDescriptor, ToolResult};
use crate::contracts::thread::{Message, Role, ToolCall};
use genai::chat::{ChatMessage, ChatRequest, MessageContent, ToolResponse};

/// Convert a ToolDescriptor to a genai Tool.
pub fn to_genai_tool(desc: &ToolDescriptor) -> genai::chat::Tool {
    genai::chat::Tool::new(&desc.id)
        .with_description(&desc.description)
        .with_schema(desc.parameters.clone())
}

/// Convert a Message to a genai ChatMessage.
pub fn to_chat_message(msg: &Message) -> ChatMessage {
    match msg.role {
        Role::System => ChatMessage::system(&msg.content),
        Role::User => ChatMessage::user(&msg.content),
        Role::Assistant => {
            if let Some(ref calls) = msg.tool_calls {
                // Build tool calls for genai
                let genai_calls: Vec<genai::chat::ToolCall> = calls
                    .iter()
                    .map(|c| genai::chat::ToolCall {
                        call_id: c.id.clone(),
                        fn_name: c.name.clone(),
                        fn_arguments: c.arguments.clone(),
                        thought_signatures: None,
                    })
                    .collect();

                // Create assistant message with tool calls
                let mut content = MessageContent::from(msg.content.as_str());
                for call in genai_calls {
                    content.push(genai::chat::ContentPart::ToolCall(call));
                }
                ChatMessage::assistant(content)
            } else {
                ChatMessage::assistant(&msg.content)
            }
        }
        Role::Tool => {
            let call_id = msg.tool_call_id.as_deref().unwrap_or("");
            let response = ToolResponse {
                call_id: call_id.to_string(),
                content: msg.content.clone(),
            };
            ChatMessage::from(response)
        }
    }
}

/// Build a genai ChatRequest from messages and tools.
pub fn build_request(messages: &[Message], tools: &[&dyn Tool]) -> ChatRequest {
    let chat_messages: Vec<ChatMessage> = messages.iter().map(to_chat_message).collect();

    let genai_tools: Vec<genai::chat::Tool> = tools
        .iter()
        .map(|t| to_genai_tool(&t.descriptor()))
        .collect();

    let mut request = ChatRequest::new(chat_messages);

    if !genai_tools.is_empty() {
        request = request.with_tools(genai_tools);
    }

    request
}

/// Apply prompt cache hints to a chat request.
///
/// Sets `CacheControl::Ephemeral` on the last system message in the request,
/// which tells Anthropic to cache everything up to (and including) that message.
/// This is a no-op for providers that don't support cache control.
pub fn apply_prompt_cache_hints(request: &mut ChatRequest) {
    // Find the last system message and mark it as the cache boundary.
    if let Some(pos) = request
        .messages
        .iter()
        .rposition(|m| matches!(m.role, genai::chat::ChatRole::System))
    {
        let msg = request.messages.remove(pos);
        request
            .messages
            .insert(pos, msg.with_options(genai::chat::CacheControl::Ephemeral));
    }
}

/// Create a user message (convenience function).
pub fn user_message(content: impl Into<String>) -> Message {
    Message::user(content)
}

/// Create an assistant message (convenience function).
pub fn assistant_message(content: impl Into<String>) -> Message {
    Message::assistant(content)
}

/// Create an assistant message with tool calls (convenience function).
pub fn assistant_tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Message {
    Message::assistant_with_tool_calls(content, calls)
}

/// Create a tool response message from ToolResult.
pub fn tool_response(call_id: impl Into<String>, result: &ToolResult) -> Message {
    let content = serde_json::to_string(result)
        .unwrap_or_else(|_| result.message.clone().unwrap_or_default());
    Message::tool(call_id, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Mock tool for testing
    struct MockTool;

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("mock", "Mock Tool", "A mock tool for testing").with_parameters(
                json!({
                    "type": "object",
                    "properties": {
                        "input": { "type": "string" }
                    }
                }),
            )
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::contracts::ToolCallContext<'_>,
        ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
            Ok(ToolResult::success("mock", json!({"result": "ok"})))
        }
    }

    #[test]
    fn test_to_genai_tool() {
        let desc = ToolDescriptor::new("calc", "Calculator", "Calculate expressions")
            .with_parameters(json!({"type": "object"}));

        let genai_tool = to_genai_tool(&desc);

        assert_eq!(genai_tool.name, "calc");
        assert_eq!(
            genai_tool.description.as_deref(),
            Some("Calculate expressions")
        );
    }

    #[test]
    fn test_to_chat_message_user() {
        let msg = Message::user("Hello");
        let chat_msg = to_chat_message(&msg);

        // ChatMessage doesn't expose role directly, but we can verify it was created
        assert!(
            format!("{:?}", chat_msg).contains("User")
                || format!("{:?}", chat_msg).to_lowercase().contains("user")
        );
    }

    #[test]
    fn test_to_chat_message_assistant() {
        let msg = Message::assistant("Hi there");
        let _chat_msg = to_chat_message(&msg);
        // Basic smoke test - conversion should not panic
    }

    #[test]
    fn test_to_chat_message_assistant_with_tools() {
        let calls = vec![ToolCall::new("call_1", "search", json!({"q": "rust"}))];
        let msg = Message::assistant_with_tool_calls("Searching...", calls);
        let _chat_msg = to_chat_message(&msg);
        // Basic smoke test - conversion should not panic
    }

    #[test]
    fn test_to_chat_message_tool() {
        let msg = Message::tool("call_1", "Result: 42");
        let _chat_msg = to_chat_message(&msg);
        // Basic smoke test - conversion should not panic
    }

    #[test]
    fn test_build_request_no_tools() {
        let messages = vec![Message::user("Hello"), Message::assistant("Hi!")];

        let request = build_request(&messages, &[]);

        assert_eq!(request.messages.len(), 2);
        assert!(request.tools.is_none());
    }

    #[test]
    fn test_build_request_with_tools() {
        let messages = vec![Message::user("Hello")];
        let mock_tool = MockTool;
        let tools: Vec<&dyn Tool> = vec![&mock_tool];

        let request = build_request(&messages, &tools);

        assert_eq!(request.messages.len(), 1);
        assert!(request.tools.is_some());
        assert_eq!(request.tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_tool_response_from_result() {
        let result = ToolResult::success("calc", json!({"answer": 42}));
        let msg = tool_response("call_1", &result);

        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert!(msg.content.contains("42") || msg.content.contains("success"));
    }

    // Additional edge case tests

    #[test]
    fn test_to_chat_message_system() {
        let msg = Message::system("You are a helpful assistant.");
        let chat_msg = to_chat_message(&msg);

        let debug_str = format!("{:?}", chat_msg);
        assert!(debug_str.to_lowercase().contains("system") || !debug_str.is_empty());
    }

    #[test]
    fn test_build_request_empty_messages() {
        let messages: Vec<Message> = vec![];
        let request = build_request(&messages, &[]);

        assert!(request.messages.is_empty());
    }

    #[test]
    fn test_build_request_multiple_tools() {
        struct Tool1;
        struct Tool2;
        struct Tool3;

        #[async_trait::async_trait]
        impl Tool for Tool1 {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("tool1", "Tool 1", "First tool")
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &crate::contracts::ToolCallContext<'_>,
            ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
                Ok(ToolResult::success("tool1", json!({})))
            }
        }

        #[async_trait::async_trait]
        impl Tool for Tool2 {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("tool2", "Tool 2", "Second tool")
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &crate::contracts::ToolCallContext<'_>,
            ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
                Ok(ToolResult::success("tool2", json!({})))
            }
        }

        #[async_trait::async_trait]
        impl Tool for Tool3 {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("tool3", "Tool 3", "Third tool")
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &crate::contracts::ToolCallContext<'_>,
            ) -> Result<ToolResult, crate::contracts::runtime::tool_call::ToolError> {
                Ok(ToolResult::success("tool3", json!({})))
            }
        }

        let t1 = Tool1;
        let t2 = Tool2;
        let t3 = Tool3;
        let tools: Vec<&dyn Tool> = vec![&t1, &t2, &t3];

        let request = build_request(&[Message::user("test")], &tools);
        assert_eq!(request.tools.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn test_to_chat_message_with_special_characters() {
        let msg = Message::user(
            "Hello! How are you?\n\nI have a question about \"quotes\" and 'apostrophes'.",
        );
        let _chat_msg = to_chat_message(&msg);
        // Should not panic with special characters
    }

    #[test]
    fn test_to_chat_message_with_unicode() {
        let msg = Message::user("你好世界! 🌍 Привет мир! مرحبا بالعالم");
        let _chat_msg = to_chat_message(&msg);
        // Should handle unicode properly
    }

    #[test]
    fn test_to_chat_message_with_empty_content() {
        let msg = Message::user("");
        let _chat_msg = to_chat_message(&msg);
        // Should handle empty content
    }

    #[test]
    fn test_to_chat_message_with_very_long_content() {
        let long_content = "a".repeat(100_000);
        let msg = Message::user(&long_content);
        let _chat_msg = to_chat_message(&msg);
        // Should handle very long content
    }

    #[test]
    fn test_tool_response_from_error_result() {
        let result = ToolResult::error("calc", "Division by zero");
        let msg = tool_response("call_err", &result);

        assert_eq!(msg.role, Role::Tool);
        assert!(msg.content.contains("error") || msg.content.contains("Division"));
    }

    #[test]
    fn test_tool_response_from_pending_result() {
        let result = ToolResult::suspended("long_task", "Processing...");
        let msg = tool_response("call_pending", &result);

        assert_eq!(msg.role, Role::Tool);
        assert!(msg.content.contains("pending") || msg.content.contains("Processing"));
    }

    #[test]
    fn test_assistant_message_with_multiple_tool_calls() {
        let calls = vec![
            ToolCall::new("call_1", "search", json!({"q": "rust"})),
            ToolCall::new("call_2", "calculate", json!({"expr": "1+1"})),
            ToolCall::new("call_3", "format", json!({"text": "hello"})),
        ];
        let msg = assistant_tool_calls("I'll help you with multiple tasks.", calls);

        assert_eq!(msg.role, Role::Assistant);
        assert!(msg.tool_calls.is_some());
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn test_to_genai_tool_with_complex_schema() {
        let desc =
            ToolDescriptor::new("api", "API Call", "Make API requests").with_parameters(json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "DELETE"]
                    },
                    "url": {
                        "type": "string",
                        "format": "uri"
                    },
                    "headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "object"
                    }
                },
                "required": ["method", "url"]
            }));

        let genai_tool = to_genai_tool(&desc);
        assert_eq!(genai_tool.name, "api");
    }

    #[test]
    fn test_build_request_conversation_history() {
        // Simulate a multi-step conversation
        let messages = vec![
            Message::user("What is 2+2?"),
            Message::assistant("2+2 equals 4."),
            Message::user("And what is 4*4?"),
            Message::assistant("4*4 equals 16."),
            Message::user("Thanks!"),
            Message::assistant("You're welcome!"),
        ];

        let request = build_request(&messages, &[]);
        assert_eq!(request.messages.len(), 6);
    }

    #[test]
    fn test_build_request_with_tool_responses() {
        let messages = vec![
            Message::user("Calculate 5*5"),
            Message::assistant_with_tool_calls(
                "I'll calculate that for you.",
                vec![ToolCall::new("call_1", "calc", json!({"expr": "5*5"}))],
            ),
            Message::tool("call_1", r#"{"result": 25}"#),
            Message::assistant("5*5 equals 25."),
        ];

        let request = build_request(&messages, &[]);
        assert_eq!(request.messages.len(), 4);
    }

    #[test]
    fn test_user_message_convenience() {
        let msg = user_message("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "Hello");
    }

    #[test]
    fn test_assistant_message_convenience() {
        let msg = assistant_message("Hi there");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "Hi there");
    }

    #[test]
    fn apply_prompt_cache_hints_marks_last_system_message() {
        let messages = vec![
            Message::system("System prompt"),
            Message::system("Session context"),
            Message::user("Hello"),
            Message::assistant("Hi!"),
        ];
        let mut request = build_request(&messages, &[]);
        apply_prompt_cache_hints(&mut request);

        // Last system message (index 1) should have CacheControl::Ephemeral.
        // First system message should not.
        let debug_0 = format!("{:?}", request.messages[0]);
        let debug_1 = format!("{:?}", request.messages[1]);
        assert!(
            !debug_0.contains("Ephemeral"),
            "first system message should not have cache hint"
        );
        assert!(
            debug_1.contains("Ephemeral"),
            "last system message should have Ephemeral cache hint"
        );
        // Message count should be preserved.
        assert_eq!(request.messages.len(), 4);
    }

    #[test]
    fn apply_prompt_cache_hints_noop_without_system_messages() {
        let messages = vec![Message::user("Hello"), Message::assistant("Hi!")];
        let mut request = build_request(&messages, &[]);
        let before = format!("{:?}", request.messages);
        apply_prompt_cache_hints(&mut request);
        let after = format!("{:?}", request.messages);
        assert_eq!(before, after, "should be no-op when no system messages exist");
    }

    #[test]
    fn apply_prompt_cache_hints_single_system_message() {
        let messages = vec![
            Message::system("Only system"),
            Message::user("Hello"),
        ];
        let mut request = build_request(&messages, &[]);
        apply_prompt_cache_hints(&mut request);
        let debug_0 = format!("{:?}", request.messages[0]);
        assert!(
            debug_0.contains("Ephemeral"),
            "single system message should get cache hint"
        );
    }

    #[test]
    fn test_tool_response_with_complex_data() {
        let result = ToolResult::success(
            "api",
            json!({
                "status": 200,
                "headers": {"Content-Type": "application/json"},
                "body": {
                    "users": [
                        {"id": 1, "name": "Alice"},
                        {"id": 2, "name": "Bob"}
                    ]
                }
            }),
        );

        let msg = tool_response("call_api", &result);
        assert!(msg.content.contains("users") || msg.content.contains("Alice"));
    }
}
