//! Conversion between awaken types and genai types.

use genai::chat::{
    self, ChatMessage, ChatRequest, ContentPart, MessageContent, Tool as GenaiTool,
    ToolCall as GenaiToolCall, ToolResponse,
};

use crate::contract::content::ContentBlock;
use crate::contract::inference::{StopReason, TokenUsage};
use crate::contract::message::{Message, Role, ToolCall};
use crate::contract::tool::ToolDescriptor;

// ---------------------------------------------------------------------------
// Message → ChatMessage
// ---------------------------------------------------------------------------

/// Convert an awaken `Message` to a genai `ChatMessage`.
pub fn to_chat_message(msg: &Message) -> ChatMessage {
    match msg.role {
        Role::System => {
            let text = msg.text();
            ChatMessage::system(text)
        }
        Role::User => {
            let parts = to_content_parts(&msg.content);
            if parts.len() == 1 {
                if let ContentPart::Text(text) = &parts[0] {
                    return ChatMessage::user(text.clone());
                }
            }
            ChatMessage::user(MessageContent::from_parts(parts))
        }
        Role::Assistant => {
            if let Some(ref calls) = msg.tool_calls {
                let genai_calls: Vec<GenaiToolCall> =
                    calls.iter().map(to_genai_tool_call).collect();
                let text = msg.text();
                if text.is_empty() {
                    ChatMessage::from(genai_calls)
                } else {
                    let mut content = MessageContent::from_text(text);
                    for call in genai_calls {
                        content.push(ContentPart::ToolCall(call));
                    }
                    ChatMessage::assistant(content)
                }
            } else {
                ChatMessage::assistant(msg.text())
            }
        }
        Role::Tool => {
            let call_id = msg.tool_call_id.as_deref().unwrap_or("");
            let response = ToolResponse {
                call_id: call_id.to_string(),
                content: msg.text(),
            };
            ChatMessage::from(response)
        }
    }
}

/// Convert awaken `ContentBlock`s to genai `ContentPart`s.
fn to_content_parts(blocks: &[ContentBlock]) -> Vec<ContentPart> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(ContentPart::Text(text.clone())),
            ContentBlock::Image { source } => match source {
                crate::contract::content::ImageSource::Url { url } => {
                    Some(ContentPart::from_binary_url("image/png", url, None))
                }
                crate::contract::content::ImageSource::Base64 { media_type, data } => Some(
                    ContentPart::from_binary_base64(media_type, data.as_str(), None),
                ),
            },
            ContentBlock::Document { source, .. } => match source {
                crate::contract::content::DocumentSource::Url { url } => {
                    Some(ContentPart::from_binary_url("application/pdf", url, None))
                }
                crate::contract::content::DocumentSource::Base64 { media_type, data } => Some(
                    ContentPart::from_binary_base64(media_type, data.as_str(), None),
                ),
            },
            // ToolUse, ToolResult, Thinking are handled separately
            _ => None,
        })
        .collect()
}

fn to_genai_tool_call(call: &ToolCall) -> GenaiToolCall {
    GenaiToolCall {
        call_id: call.id.clone(),
        fn_name: call.name.clone(),
        fn_arguments: call.arguments.clone(),
        thought_signatures: None,
    }
}

// ---------------------------------------------------------------------------
// ToolDescriptor → genai::Tool
// ---------------------------------------------------------------------------

/// Convert an awaken `ToolDescriptor` to a genai `Tool`.
pub fn to_genai_tool(desc: &ToolDescriptor) -> GenaiTool {
    GenaiTool::new(&desc.id)
        .with_description(&desc.description)
        .with_schema(desc.parameters.clone())
}

// ---------------------------------------------------------------------------
// Build ChatRequest
// ---------------------------------------------------------------------------

/// Build a genai `ChatRequest` from messages, system prompt, and tools.
pub fn build_chat_request(
    system: &[ContentBlock],
    messages: &[Message],
    tools: &[ToolDescriptor],
    enable_prompt_cache: bool,
) -> ChatRequest {
    let mut chat_messages: Vec<ChatMessage> = Vec::with_capacity(messages.len() + 1);

    // System prompt as first message
    if !system.is_empty() {
        let text = crate::contract::content::extract_text(system);
        if !text.is_empty() {
            let mut msg = ChatMessage::system(text);
            if enable_prompt_cache {
                msg = msg.with_options(chat::CacheControl::Ephemeral);
            }
            chat_messages.push(msg);
        }
    }

    // Conversation messages — all go to LLM (including Internal visibility)
    for msg in messages {
        chat_messages.push(to_chat_message(msg));
    }

    let genai_tools: Vec<GenaiTool> = tools.iter().map(to_genai_tool).collect();

    let mut request = ChatRequest::new(chat_messages);
    if !genai_tools.is_empty() {
        request = request.with_tools(genai_tools);
    }

    request
}

// ---------------------------------------------------------------------------
// genai types → awaken types
// ---------------------------------------------------------------------------

/// Map genai `StopReason` to awaken `StopReason`.
pub fn map_stop_reason(reason: &chat::StopReason) -> Option<StopReason> {
    match reason {
        chat::StopReason::Completed(_) => Some(StopReason::EndTurn),
        chat::StopReason::MaxTokens(_) => Some(StopReason::MaxTokens),
        chat::StopReason::ToolCall(_) => Some(StopReason::ToolUse),
        chat::StopReason::StopSequence(_) => Some(StopReason::StopSequence),
        chat::StopReason::ContentFilter(_) | chat::StopReason::Other(_) => None,
    }
}

/// Map genai `Usage` to awaken `TokenUsage`.
pub fn map_usage(u: &chat::Usage) -> TokenUsage {
    let (cache_read, cache_creation) = u
        .prompt_tokens_details
        .as_ref()
        .map_or((None, None), |d| (d.cached_tokens, d.cache_creation_tokens));

    let thinking_tokens = u
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens);

    TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        thinking_tokens,
    }
}

/// Convert a genai `ToolCall` to an awaken `ToolCall`.
pub fn from_genai_tool_call(call: &GenaiToolCall) -> ToolCall {
    ToolCall::new(&call.call_id, &call.fn_name, call.fn_arguments.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_converts_to_chat_message() {
        let msg = Message::user("hello");
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::User));
    }

    #[test]
    fn tool_descriptor_converts_to_genai_tool() {
        let desc = ToolDescriptor::new("calc", "calculator", "Evaluates math");
        let tool = to_genai_tool(&desc);
        assert_eq!(tool.name, "calc".into());
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(
            map_stop_reason(&chat::StopReason::Completed("stop".into())),
            Some(StopReason::EndTurn)
        );
        assert_eq!(
            map_stop_reason(&chat::StopReason::MaxTokens("length".into())),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            map_stop_reason(&chat::StopReason::ToolCall("tool_use".into())),
            Some(StopReason::ToolUse)
        );
    }

    #[test]
    fn assistant_with_tool_calls_converts() {
        let msg = Message::assistant_with_tool_calls(
            "Let me calc",
            vec![ToolCall::new("c1", "calculator", json!({"expr": "2+2"}))],
        );
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::Assistant));
    }

    // -- Task 1 tests --------------------------------------------------------

    #[test]
    fn to_chat_message_user() {
        let msg = Message::user("How are you?");
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::User));
        let text = cm.content.first_text().expect("should have text");
        assert_eq!(text, "How are you?");
    }

    #[test]
    fn to_chat_message_assistant_with_tools() {
        let calls = vec![
            ToolCall::new("c1", "search", json!({"q": "rust"})),
            ToolCall::new("c2", "calc", json!({"expr": "1+1"})),
        ];
        let msg = Message::assistant_with_tool_calls("I'll help", calls);
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::Assistant));
        // Content should include text and tool call parts
        let tool_calls = cm.content.tool_calls();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].fn_name, "search");
        assert_eq!(tool_calls[1].fn_name, "calc");
        // Text should still be present
        let text = cm.content.first_text().expect("should have text");
        assert_eq!(text, "I'll help");
    }

    #[test]
    fn to_chat_message_assistant_with_tools_no_text() {
        let calls = vec![ToolCall::new("c1", "search", json!({"q": "rust"}))];
        let msg = Message::assistant_with_tool_calls("", calls);
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::Assistant));
        let tool_calls = cm.content.tool_calls();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].call_id, "c1");
    }

    #[test]
    fn to_chat_message_tool_result() {
        let msg = Message::tool("call-42", "Result is 42");
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::Tool));
        // ToolResponse messages store content as a ToolResponse part, not plain text.
        // Verify the content is non-empty and the role is correct.
        assert!(!cm.content.parts().is_empty());
    }

    #[test]
    fn to_chat_message_system() {
        let msg = Message::system("You are a helpful assistant.");
        let cm = to_chat_message(&msg);
        assert!(matches!(cm.role, chat::ChatRole::System));
        let text = cm.content.first_text().expect("should have text");
        assert_eq!(text, "You are a helpful assistant.");
    }

    #[test]
    fn build_chat_request_no_tools() {
        let system = vec![ContentBlock::text("Be helpful")];
        let messages = vec![Message::user("hi")];
        let req = build_chat_request(&system, &messages, &[], false);
        // System message + user message = 2 messages
        assert_eq!(req.messages.len(), 2);
        assert!(matches!(req.messages[0].role, chat::ChatRole::System));
        assert!(matches!(req.messages[1].role, chat::ChatRole::User));
        // No tools
        assert!(req.tools.is_none());
    }

    #[test]
    fn build_chat_request_with_tools() {
        let tools = vec![
            ToolDescriptor::new("search", "search", "Web search"),
            ToolDescriptor::new("calc", "calc", "Calculator"),
        ];
        let req = build_chat_request(&[], &[Message::user("hi")], &tools, false);
        let genai_tools = req.tools.expect("should have tools");
        assert_eq!(genai_tools.len(), 2);
        assert_eq!(genai_tools[0].name, "search".into());
        assert_eq!(genai_tools[1].name, "calc".into());
    }

    #[test]
    fn to_genai_tool_preserves_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "limit": {"type": "integer"}
            },
            "required": ["query"]
        });
        let desc =
            ToolDescriptor::new("search", "search", "Web search").with_parameters(schema.clone());
        let tool = to_genai_tool(&desc);
        assert_eq!(tool.schema.as_ref().expect("schema present"), &schema);
        assert_eq!(tool.description.as_deref(), Some("Web search"));
    }

    #[test]
    fn build_chat_request_empty_system_skipped() {
        let req = build_chat_request(&[], &[Message::user("hi")], &[], false);
        // Only user message, no system
        assert_eq!(req.messages.len(), 1);
        assert!(matches!(req.messages[0].role, chat::ChatRole::User));
    }

    #[test]
    fn build_chat_request_prompt_cache_sets_options() {
        let system = vec![ContentBlock::text("Be helpful")];
        let req = build_chat_request(&system, &[], &[], true);
        assert_eq!(req.messages.len(), 1);
        // The system message should have options set (cache control)
        assert!(req.messages[0].options.is_some());
    }

    #[test]
    fn from_genai_tool_call_roundtrip() {
        let genai_call = GenaiToolCall {
            call_id: "c1".into(),
            fn_name: "search".into(),
            fn_arguments: json!({"q": "rust"}),
            thought_signatures: None,
        };
        let awaken_call = from_genai_tool_call(&genai_call);
        assert_eq!(awaken_call.id, "c1");
        assert_eq!(awaken_call.name, "search");
        assert_eq!(awaken_call.arguments, json!({"q": "rust"}));
    }

    #[test]
    fn map_usage_handles_all_fields() {
        let genai_usage = chat::Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            prompt_tokens_details: Some(chat::PromptTokensDetails {
                cached_tokens: Some(20),
                cache_creation_tokens: Some(10),
                cache_creation_details: None,
                audio_tokens: None,
            }),
            completion_tokens_details: Some(chat::CompletionTokensDetails {
                reasoning_tokens: Some(5),
                accepted_prediction_tokens: None,
                rejected_prediction_tokens: None,
                audio_tokens: None,
            }),
        };
        let usage = map_usage(&genai_usage);
        assert_eq!(usage.prompt_tokens, Some(100));
        assert_eq!(usage.completion_tokens, Some(50));
        assert_eq!(usage.total_tokens, Some(150));
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.cache_creation_tokens, Some(10));
        assert_eq!(usage.thinking_tokens, Some(5));
    }

    #[test]
    fn stop_reason_content_filter_maps_to_none() {
        assert_eq!(
            map_stop_reason(&chat::StopReason::ContentFilter("filter".into())),
            None
        );
    }

    #[test]
    fn stop_reason_other_maps_to_none() {
        assert_eq!(
            map_stop_reason(&chat::StopReason::Other("unknown".into())),
            None
        );
    }

    #[test]
    fn stop_reason_stop_sequence_maps_correctly() {
        assert_eq!(
            map_stop_reason(&chat::StopReason::StopSequence("seq".into())),
            Some(StopReason::StopSequence)
        );
    }

    #[test]
    fn prompt_cache_hints_applied_to_system_messages() {
        let system = vec![ContentBlock::text("You are a helpful assistant.")];
        let messages = vec![Message::user("hello")];
        let req = build_chat_request(&system, &messages, &[], true);

        // The system message (first) should have CacheControl::Ephemeral in options
        let system_msg = &req.messages[0];
        assert!(matches!(system_msg.role, chat::ChatRole::System));
        let opts = system_msg
            .options
            .as_ref()
            .expect("system message should have options when prompt cache is enabled");
        assert_eq!(opts.cache_control, Some(chat::CacheControl::Ephemeral));
    }

    #[test]
    fn prompt_cache_hints_not_applied_when_disabled() {
        let system = vec![ContentBlock::text("You are a helpful assistant.")];
        let messages = vec![Message::user("hello")];
        let req = build_chat_request(&system, &messages, &[], false);

        // The system message should NOT have options set
        let system_msg = &req.messages[0];
        assert!(matches!(system_msg.role, chat::ChatRole::System));
        assert!(
            system_msg.options.is_none(),
            "system message should not have options when prompt cache is disabled"
        );
    }
}
