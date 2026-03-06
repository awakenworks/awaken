//! Fast heuristic token estimation for context window management.
//!
//! Uses character-based heuristics rather than a full tokenizer to avoid
//! heavy dependencies. Accuracy is ±20% which is sufficient for deciding
//! when to truncate or compact conversation history.

use crate::contracts::runtime::tool_call::ToolDescriptor;
use crate::contracts::thread::Message;

/// Approximate tokens per ASCII character for typical LLM tokenizers.
const CHARS_PER_TOKEN_ASCII: f32 = 4.0;
/// Approximate tokens per CJK character.
const CHARS_PER_TOKEN_CJK: f32 = 1.5;
/// Overhead tokens per message (role tag, separators, etc.).
const MESSAGE_OVERHEAD: usize = 4;
/// Overhead tokens per tool call structure (JSON envelope, name, id).
const TOOL_CALL_OVERHEAD: usize = 20;
/// Overhead tokens per tool descriptor (JSON schema envelope).
const TOOL_DESCRIPTOR_OVERHEAD: usize = 20;

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

/// Estimate token count for a text string.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut cjk_chars = 0usize;
    let mut other_chars = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk_chars += 1;
        } else {
            other_chars += 1;
        }
    }
    let cjk_tokens = (cjk_chars as f32 / CHARS_PER_TOKEN_CJK).ceil() as usize;
    let ascii_tokens = (other_chars as f32 / CHARS_PER_TOKEN_ASCII).ceil() as usize;
    (cjk_tokens + ascii_tokens).max(1)
}

/// Estimate token count for a single message.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let content_tokens = estimate_tokens(&msg.content);
    let tool_call_tokens: usize = msg
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|c| {
                    estimate_tokens(&c.name)
                        + estimate_tokens(&c.arguments.to_string())
                        + TOOL_CALL_OVERHEAD
                })
                .sum()
        })
        .unwrap_or(0);
    content_tokens + tool_call_tokens + MESSAGE_OVERHEAD
}

/// Estimate total token count for a slice of messages.
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Estimate total token count for tool descriptors.
pub fn estimate_tool_tokens(tools: &[ToolDescriptor]) -> usize {
    tools
        .iter()
        .map(|t| {
            estimate_tokens(&t.name)
                + estimate_tokens(&t.description)
                + estimate_tokens(&t.parameters.to_string())
                + TOOL_DESCRIPTOR_OVERHEAD
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_ascii() {
        // "Hello world" = 11 chars → ~3 tokens
        let tokens = estimate_tokens("Hello world");
        assert!(tokens >= 2 && tokens <= 5, "got {tokens}");
    }

    #[test]
    fn estimate_tokens_cjk() {
        // "你好世界" = 4 CJK chars → ~3 tokens
        let tokens = estimate_tokens("你好世界");
        assert!(tokens >= 2 && tokens <= 5, "got {tokens}");
    }

    #[test]
    fn estimate_tokens_mixed() {
        let tokens = estimate_tokens("Hello 你好 world 世界");
        assert!(tokens >= 4 && tokens <= 10, "got {tokens}");
    }

    #[test]
    fn estimate_tokens_code_block() {
        let code = "fn main() {\n    let x = compute(42);\n    return x;\n}";
        let tokens = estimate_tokens(code);
        assert!(tokens >= 8 && tokens <= 20, "got {tokens}");
    }

    #[test]
    fn estimate_message_tokens_simple() {
        let msg = Message::user("What is 2+2?");
        let tokens = estimate_message_tokens(&msg);
        assert!(tokens >= 5, "got {tokens}");
    }

    #[test]
    fn estimate_message_tokens_with_tool_calls() {
        use crate::contracts::thread::ToolCall;
        let msg = Message::assistant_with_tool_calls(
            "I'll calculate that.",
            vec![ToolCall::new("call_1", "calculator", json!({"expr": "2+2"}))],
        );
        let tokens = estimate_message_tokens(&msg);
        // Content + tool call name + args + overheads
        assert!(tokens >= 15, "got {tokens}");
    }

    #[test]
    fn estimate_tool_tokens_basic() {
        let tools = vec![ToolDescriptor::new("calc", "Calculator", "Evaluate math expressions")
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "expression": { "type": "string" }
                },
                "required": ["expression"]
            }))];
        let tokens = estimate_tool_tokens(&tools);
        assert!(tokens >= 20, "got {tokens}");
    }

    #[test]
    fn estimate_messages_tokens_multiple() {
        let messages = vec![
            Message::user("Hello"),
            Message::assistant("Hi there!"),
            Message::user("How are you?"),
        ];
        let total = estimate_messages_tokens(&messages);
        let sum: usize = messages.iter().map(estimate_message_tokens).sum();
        assert_eq!(total, sum);
    }
}
