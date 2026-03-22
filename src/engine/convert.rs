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
}
