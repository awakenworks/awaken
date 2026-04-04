//! Inference request transformation pipeline.
//!
//! Plugins register `InferenceRequestTransform` implementations to modify
//! the assembled message list before it is sent to the LLM executor.
//! Transforms are applied in registration order after context message injection.

use std::borrow::Borrow;

use super::content::ContentBlock;
use super::message::Message;
use super::tool::ToolDescriptor;

/// Output of a request transform.
pub struct TransformOutput {
    /// Transformed message list.
    pub messages: Vec<Message>,
}

/// A synchronous, pure transformation applied to the assembled message list
/// before sending to the LLM executor.
///
/// Registered via `PluginRegistrar::register_request_transform()`.
/// Applied in registration order by the loop runner.
pub trait InferenceRequestTransform: Send + Sync {
    fn transform(
        &self,
        messages: Vec<Message>,
        tool_descriptors: &[ToolDescriptor],
    ) -> TransformOutput;
}

/// Apply a chain of request transforms in order.
pub fn apply_transforms(
    mut messages: Vec<Message>,
    tool_descriptors: &[ToolDescriptor],
    transforms: &[std::sync::Arc<dyn InferenceRequestTransform>],
) -> Vec<Message> {
    for transform in transforms {
        let output = transform.transform(messages, tool_descriptors);
        messages = output.messages;
    }
    messages
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Estimate token count for a single message (roughly 4 chars per token).
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let content_chars: usize = msg
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::Thinking { thinking } => thinking.len(),
            _ => 50, // rough estimate for non-text blocks
        })
        .sum();
    // role overhead + content
    4 + content_chars / 4
}

/// Estimate total token count for a slice of messages.
///
/// Accepts both `&[Message]` and `&[Arc<Message>]` via the `Borrow<Message>` bound.
pub fn estimate_tokens<M: Borrow<Message>>(messages: &[M]) -> usize {
    messages
        .iter()
        .map(|m| estimate_message_tokens(m.borrow()))
        .sum()
}

/// Estimate token count for tool descriptors.
pub fn estimate_tool_tokens(tools: &[ToolDescriptor]) -> usize {
    tools
        .iter()
        .map(|t| {
            let schema_chars = serde_json::to_string(&t.parameters)
                .map(|s| s.len())
                .unwrap_or(100);
            t.name.len() + t.description.len() + schema_chars / 4 + 10
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Dangling tool call repair
// ---------------------------------------------------------------------------

/// Ensure every tool call in an assistant message has a matching tool result.
///
/// If an assistant message has tool_calls but some don't have corresponding
/// Tool role messages, synthetic "[interrupted]" results are inserted.
pub fn patch_dangling_tool_calls(messages: &mut Vec<Message>) {
    use super::message::Role;
    use std::collections::HashSet;

    let result_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    let mut insertions: Vec<(usize, Vec<Message>)> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        if msg.role != Role::Assistant {
            continue;
        }
        let Some(ref calls) = msg.tool_calls else {
            continue;
        };

        let missing: Vec<&str> = calls
            .iter()
            .map(|tc| tc.id.as_str())
            .filter(|id| !result_ids.contains(*id))
            .collect();

        if missing.is_empty() {
            continue;
        }

        // Find insertion point: after this assistant message and any existing tool results
        let mut insert_at = i + 1;
        while insert_at < messages.len() && messages[insert_at].role == Role::Tool {
            insert_at += 1;
        }

        let synthetic: Vec<Message> = missing
            .into_iter()
            .map(|id| Message::tool(id, "[Tool execution was interrupted]"))
            .collect();
        insertions.push((insert_at, synthetic));
    }

    // Apply insertions in reverse order to preserve indices
    for (idx, msgs) in insertions.into_iter().rev() {
        let idx = idx.min(messages.len());
        for (offset, msg) in msgs.into_iter().enumerate() {
            messages.insert(idx + offset, msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::message::{Message, Role, ToolCall};
    use serde_json::json;

    #[test]
    fn estimate_tokens_for_text_message() {
        let msg = Message::user("Hello, world!"); // 13 chars → ~3 tokens + 4 overhead
        let est = estimate_message_tokens(&msg);
        assert!(est > 0);
        assert!(est < 20);
    }

    #[test]
    fn estimate_tokens_empty_messages() {
        assert_eq!(estimate_tokens::<Message>(&[]), 0);
    }

    #[test]
    fn patch_dangling_adds_synthetic_results() {
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "let me search",
                vec![
                    ToolCall::new("c1", "search", json!({})),
                    ToolCall::new("c2", "read", json!({})),
                ],
            ),
            Message::tool("c1", "found it"),
            // c2 has no result — dangling
        ];

        patch_dangling_tool_calls(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c2"));
        assert!(messages[3].text().contains("interrupted"));
    }

    #[test]
    fn patch_dangling_no_change_when_complete() {
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![ToolCall::new("c1", "search", json!({}))]),
            Message::tool("c1", "done"),
        ];

        let len_before = messages.len();
        patch_dangling_tool_calls(&mut messages);
        assert_eq!(messages.len(), len_before);
    }

    #[test]
    fn estimate_tokens_ascii() {
        let msg = Message::user("abcdefghijklmnop"); // 16 chars → 4 tokens + 4 overhead = 8
        let est = estimate_message_tokens(&msg);
        assert_eq!(est, 8);
    }

    #[test]
    fn estimate_tokens_cjk() {
        // CJK characters are ~3 bytes each, but token estimation uses char count
        let msg = Message::user("你好世界测试"); // 6 CJK chars, 18 bytes → 6/4 = 1 token + 4 = 5
        let est = estimate_message_tokens(&msg);
        assert!(est > 0);
    }

    #[test]
    fn estimate_tokens_with_tool_calls() {
        let msg = Message::assistant_with_tool_calls(
            "calling tools",
            vec![ToolCall::new(
                "c1",
                "search",
                json!({"query": "rust async"}),
            )],
        );
        let est = estimate_message_tokens(&msg);
        assert!(est > 4); // more than just overhead
    }

    #[test]
    fn estimate_tool_tokens_includes_schema() {
        use crate::contract::tool::ToolDescriptor;
        let tool = ToolDescriptor {
            id: "calc".into(),
            name: "calculator".into(),
            description: "Evaluate math expressions".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "expression": {"type": "string"}
                }
            }),
            category: None,
            metadata: Default::default(),
        };
        let est = estimate_tool_tokens(&[tool]);
        assert!(est > 10); // name + description + schema
    }

    #[test]
    fn estimate_multiple_messages() {
        let messages = vec![
            Message::system("you are helpful"),
            Message::user("hello"),
            Message::assistant("hi there"),
        ];
        let total = estimate_tokens(&messages);
        let individual: usize = messages.iter().map(estimate_message_tokens).sum();
        assert_eq!(total, individual);
    }

    #[test]
    fn patch_dangling_multiple_missing() {
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "",
                vec![
                    ToolCall::new("c1", "a", json!({})),
                    ToolCall::new("c2", "b", json!({})),
                    ToolCall::new("c3", "c", json!({})),
                ],
            ),
            // No results at all
        ];
        patch_dangling_tool_calls(&mut messages);
        // Should have 3 synthetic results
        assert_eq!(messages.len(), 5);
        for message in messages.iter().take(5).skip(2) {
            assert_eq!(message.role, Role::Tool);
            assert!(message.text().contains("interrupted"));
        }
    }

    #[test]
    fn patch_dangling_no_tool_calls_is_noop() {
        let mut messages = vec![Message::user("hello"), Message::assistant("hi")];
        let len_before = messages.len();
        patch_dangling_tool_calls(&mut messages);
        assert_eq!(messages.len(), len_before);
    }

    #[test]
    fn apply_transforms_chains_in_order() {
        struct PrependTransform(String);
        impl InferenceRequestTransform for PrependTransform {
            fn transform(
                &self,
                mut messages: Vec<Message>,
                _tools: &[ToolDescriptor],
            ) -> TransformOutput {
                messages.insert(0, Message::system(&self.0));
                TransformOutput { messages }
            }
        }

        let messages = vec![Message::user("hello")];
        let transforms: Vec<std::sync::Arc<dyn InferenceRequestTransform>> = vec![
            std::sync::Arc::new(PrependTransform("first".into())),
            std::sync::Arc::new(PrependTransform("second".into())),
        ];

        let result = apply_transforms(messages, &[], &transforms);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].text(), "second"); // second transform prepends last
        assert_eq!(result[1].text(), "first");
        assert_eq!(result[2].text(), "hello");
    }
}
