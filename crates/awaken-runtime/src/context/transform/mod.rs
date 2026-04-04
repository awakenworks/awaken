//! Built-in request transform: truncate messages to fit the token budget.

mod compaction;
mod truncation;

pub use compaction::{
    ARTIFACT_COMPACT_THRESHOLD_TOKENS, ARTIFACT_PREVIEW_MAX_CHARS, ARTIFACT_PREVIEW_MAX_LINES,
    compact_artifact, compact_tool_results,
};
pub use truncation::{adjust_split_for_tool_pairs, find_split_point};

use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::contract::transform::{
    InferenceRequestTransform, TransformOutput, estimate_message_tokens, estimate_tokens,
    estimate_tool_tokens, patch_dangling_tool_calls,
};

/// Built-in request transform: truncate messages to fit the token budget.
///
/// Preserves all system messages and the most recent conversation messages.
/// Adjusts split points to avoid orphaning tool call/result pairs.
pub struct ContextTransform {
    policy: ContextWindowPolicy,
}

impl ContextTransform {
    pub fn new(policy: ContextWindowPolicy) -> Self {
        Self { policy }
    }
}

impl InferenceRequestTransform for ContextTransform {
    fn transform(
        &self,
        mut messages: Vec<Message>,
        tool_descriptors: &[ToolDescriptor],
    ) -> TransformOutput {
        // Compact oversized tool results before truncation
        compact_tool_results(&mut messages);

        let tool_tokens = estimate_tool_tokens(tool_descriptors);
        let available = self
            .policy
            .max_context_tokens
            .saturating_sub(self.policy.max_output_tokens)
            .saturating_sub(tool_tokens);

        let total = estimate_tokens(&messages);
        if total <= available {
            return TransformOutput { messages };
        }

        // Split into system prefix and history
        let system_end = messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(messages.len());

        let system_tokens: usize = messages[..system_end]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        let history_budget = available.saturating_sub(system_tokens);

        // Find split point: walk backward from end, accumulating tokens
        let history = &messages[system_end..];
        let split = find_split_point(history, history_budget, self.policy.min_recent_messages);
        let absolute_split = system_end + split;

        // Remove truncated messages
        let dropped = absolute_split.saturating_sub(system_end);
        if absolute_split > system_end {
            messages.drain(system_end..absolute_split);
        }
        let kept = messages.len();

        // Repair dangling tool calls after truncation
        patch_dangling_tool_calls(&mut messages);

        tracing::debug!(dropped, kept, "truncation_applied");

        TransformOutput { messages }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::inference::ContextWindowPolicy;
    use awaken_contract::contract::message::ToolCall;
    use serde_json::json;

    fn make_policy(max_tokens: usize, min_recent: usize) -> ContextWindowPolicy {
        ContextWindowPolicy {
            max_context_tokens: max_tokens,
            max_output_tokens: 0,
            min_recent_messages: min_recent,
            enable_prompt_cache: false,
            autocompact_threshold: None,
            compaction_mode: Default::default(),
            compaction_raw_suffix_messages: 2,
        }
    }

    #[test]
    fn truncation_preserves_all_when_under_budget() {
        let transform = ContextTransform::new(make_policy(100_000, 2));
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        assert_eq!(output.messages.len(), 3);
    }

    #[test]
    fn truncation_keeps_system_and_recent() {
        // Very tight budget: system + ~2 recent messages
        let transform = ContextTransform::new(make_policy(50, 2));
        let mut messages = vec![Message::system("sys")];
        // Add many user/assistant turns
        for i in 0..20 {
            messages.push(Message::user(format!("msg {i}")));
            messages.push(Message::assistant(format!("reply {i}")));
        }

        let output = transform.transform(messages, &[]);
        // Should have system + at least 2 recent messages
        assert!(output.messages.len() >= 3);
        assert_eq!(output.messages[0].role, Role::System);
    }

    #[test]
    fn truncation_repairs_dangling_tool_calls() {
        let transform = ContextTransform::new(make_policy(30, 1));
        let messages = vec![
            Message::system("sys"),
            Message::user("old msg 1"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "search", json!({}))],
            ),
            Message::tool("c1", "result"),
            Message::user("old msg 2"),
            Message::assistant("old reply"),
            // many more to force truncation...
            Message::user("recent"),
            Message::assistant("recent reply"),
        ];

        let output = transform.transform(messages, &[]);
        // Should not have orphaned tool calls
        for (i, msg) in output.messages.iter().enumerate() {
            if msg.role == Role::Assistant
                && let Some(ref calls) = msg.tool_calls
            {
                for call in calls {
                    assert!(
                        output.messages[i + 1..]
                            .iter()
                            .any(|m| m.tool_call_id.as_deref() == Some(&call.id)),
                        "tool call {} should have a matching result",
                        call.id
                    );
                }
            }
        }
    }

    #[test]
    fn truncation_tool_pair_not_broken() {
        // Tight budget — truncation should not split an assistant+tool pair
        let transform = ContextTransform::new(make_policy(60, 1));
        let messages = vec![
            Message::system("sys"),
            Message::user("old"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "search", json!({}))],
            ),
            Message::tool("c1", "found"),
            Message::user("recent"),
            Message::assistant("reply"),
        ];

        let output = transform.transform(messages, &[]);
        // If the assistant with tool_calls is kept, its tool result must also be kept
        for (i, msg) in output.messages.iter().enumerate() {
            if msg.role == Role::Assistant
                && let Some(ref calls) = msg.tool_calls
            {
                for call in calls {
                    let has_result = output.messages[i + 1..]
                        .iter()
                        .any(|m| m.tool_call_id.as_deref() == Some(&call.id));
                    assert!(
                        has_result,
                        "tool call {} should have matching result",
                        call.id
                    );
                }
            }
        }
    }

    #[test]
    fn truncation_with_tool_descriptors_reduces_budget() {
        use awaken_contract::contract::tool::ToolDescriptor;

        let transform = ContextTransform::new(make_policy(100, 2));
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("world"),
        ];

        // Without tools: all fit
        let output_no_tools = transform.transform(messages.clone(), &[]);
        let count_no_tools = output_no_tools.messages.len();

        // With large tool schemas: might truncate
        let big_tool = ToolDescriptor {
            id: "t".into(),
            name: "t".into(),
            description: "x".repeat(200),
            parameters: json!({"type": "object", "properties": {
                "a": {"type": "string"}, "b": {"type": "string"},
                "c": {"type": "string"}, "d": {"type": "string"},
            }}),
            category: None,
            metadata: Default::default(),
        };

        let output_with_tools = transform.transform(messages, &[big_tool]);
        // With tools consuming budget, we may have fewer messages
        assert!(output_with_tools.messages.len() <= count_no_tools);
    }

    #[test]
    fn no_truncation_when_within_budget() {
        let transform = ContextTransform::new(make_policy(100_000, 2));
        let messages = vec![
            Message::system("system prompt"),
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("how are you?"),
            Message::assistant("doing great"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        assert_eq!(output.messages.len(), messages.len());
        for (a, b) in output.messages.iter().zip(messages.iter()) {
            assert_eq!(a.text(), b.text());
        }
    }

    #[test]
    fn truncation_drops_oldest_history() {
        let transform = ContextTransform::new(make_policy(60, 2));
        let filler = |tag: &str| format!("{tag}:{}", "x".repeat(40));
        let messages = vec![
            Message::system("sys"),
            Message::user(filler("old1")),
            Message::assistant(filler("old_reply1")),
            Message::user(filler("old2")),
            Message::assistant(filler("old_reply2")),
            Message::user(filler("recent1")),
            Message::assistant(filler("recent_reply1")),
        ];

        let output = transform.transform(messages, &[]);
        // System must be preserved
        assert_eq!(output.messages[0].role, Role::System);
        assert_eq!(output.messages[0].text(), "sys");
        // Oldest history should be dropped
        let texts: Vec<String> = output.messages.iter().map(|m| m.text()).collect();
        assert!(
            !texts.iter().any(|t| t.starts_with("old1:")),
            "oldest message should be dropped"
        );
        // Recent messages should be preserved
        assert!(
            texts.iter().any(|t| t.starts_with("recent_reply1:")),
            "most recent message should be preserved"
        );
    }

    #[test]
    fn min_recent_always_preserved() {
        // Very tight budget but min_recent = 4; should keep at least 4 history messages
        let transform = ContextTransform::new(make_policy(20, 4));
        let messages = vec![
            Message::system("s"),
            Message::user("a"),
            Message::assistant("b"),
            Message::user("c"),
            Message::assistant("d"),
            Message::user("e"),
            Message::assistant("f"),
        ];

        let output = transform.transform(messages, &[]);
        // System is always kept; history portion should have at least min_recent messages
        let history_count = output
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .count();
        assert!(
            history_count >= 4,
            "min_recent_messages=4 but only {history_count} history messages kept"
        );
    }

    #[test]
    fn system_messages_never_truncated() {
        // Multiple system messages at the start — all must survive truncation
        let transform = ContextTransform::new(make_policy(60, 1));
        let messages = vec![
            Message::system("system prompt 1"),
            Message::system("system prompt 2"),
            Message::system("system prompt 3"),
            Message::user("old1"),
            Message::assistant("old_reply1"),
            Message::user("old2"),
            Message::assistant("old_reply2"),
            Message::user("recent"),
            Message::assistant("recent_reply"),
        ];

        let output = transform.transform(messages, &[]);
        let system_msgs: Vec<&Message> = output
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .collect();
        assert_eq!(
            system_msgs.len(),
            3,
            "all system messages must be preserved"
        );
        assert_eq!(system_msgs[0].text(), "system prompt 1");
        assert_eq!(system_msgs[1].text(), "system prompt 2");
        assert_eq!(system_msgs[2].text(), "system prompt 3");
    }

    #[test]
    fn truncation_empty_messages() {
        let transform = ContextTransform::new(make_policy(100, 2));
        let messages = vec![];
        let output = transform.transform(messages, &[]);
        assert!(output.messages.is_empty());
    }

    #[test]
    fn truncation_system_only() {
        let transform = ContextTransform::new(make_policy(100, 2));
        let messages = vec![Message::system("system only")];
        let output = transform.transform(messages, &[]);
        assert_eq!(output.messages.len(), 1);
        assert_eq!(output.messages[0].role, Role::System);
    }

    #[test]
    fn truncation_preserves_message_order() {
        let transform = ContextTransform::new(make_policy(100_000, 2));
        let messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
        ];
        let output = transform.transform(messages.clone(), &[]);
        for (i, msg) in output.messages.iter().enumerate() {
            assert_eq!(msg.role, messages[i].role);
            assert_eq!(msg.text(), messages[i].text());
        }
    }

    #[test]
    fn truncation_with_only_tool_messages() {
        let transform = ContextTransform::new(make_policy(100, 1));
        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![ToolCall::new("c1", "t", json!({}))]),
            Message::tool("c1", "result"),
        ];
        let output = transform.transform(messages, &[]);
        // Should have at least system and something
        assert!(!output.messages.is_empty());
        assert_eq!(output.messages[0].role, Role::System);
    }
}
