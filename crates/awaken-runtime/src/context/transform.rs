//! Built-in request transform: truncate messages to fit the token budget.

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::contract::transform::{
    InferenceRequestTransform, TransformOutput, estimate_message_tokens, estimate_tokens,
    estimate_tool_tokens, patch_dangling_tool_calls,
};

/// Token threshold above which a tool result is compacted to a preview.
pub const ARTIFACT_COMPACT_THRESHOLD_TOKENS: usize = 2048;

/// Maximum characters retained in a compacted artifact preview.
pub const ARTIFACT_PREVIEW_MAX_CHARS: usize = 1600;

/// Maximum lines retained in a compacted artifact preview.
pub const ARTIFACT_PREVIEW_MAX_LINES: usize = 24;

/// Compact a single artifact string if it exceeds the token threshold.
///
/// Returns the original content unchanged when estimated tokens are within budget.
/// Otherwise truncates to [`ARTIFACT_PREVIEW_MAX_CHARS`] / [`ARTIFACT_PREVIEW_MAX_LINES`]
/// (whichever is shorter) and appends a compaction indicator.
pub fn compact_artifact(content: &str) -> String {
    let estimated_tokens = content.len() / 4;
    if estimated_tokens < ARTIFACT_COMPACT_THRESHOLD_TOKENS {
        return content.to_string();
    }

    // Truncate by line limit first, then by char limit
    let mut char_count = 0usize;
    let mut line_count = 0usize;
    let mut end_byte = 0usize;

    for (idx, ch) in content.char_indices() {
        if char_count >= ARTIFACT_PREVIEW_MAX_CHARS || line_count >= ARTIFACT_PREVIEW_MAX_LINES {
            break;
        }
        if ch == '\n' {
            line_count += 1;
        }
        char_count += 1;
        end_byte = idx + ch.len_utf8();
    }

    let preview = &content[..end_byte];
    format!(
        "{preview}\n\n[Content compacted: original ~{estimated_tokens} tokens, showing first {char_count} chars]"
    )
}

/// Compact tool result messages that exceed the artifact token threshold.
///
/// Iterates over all `Role::Tool` messages and replaces oversized text content
/// blocks with a truncated preview plus compaction indicator.
pub fn compact_tool_results(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        let mut modified = false;
        let new_content: Vec<ContentBlock> = msg
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => {
                    let compacted = compact_artifact(text);
                    if compacted.len() != text.len() {
                        modified = true;
                    }
                    ContentBlock::Text { text: compacted }
                }
                other => other.clone(),
            })
            .collect();
        if modified {
            msg.content = new_content;
        }
    }
}

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

/// Find the split point in history that fits the token budget.
///
/// Always keeps at least `min_recent` messages from the end.
/// Adjusts boundaries to avoid splitting tool call/result pairs.
fn find_split_point(history: &[Message], budget_tokens: usize, min_recent: usize) -> usize {
    if history.is_empty() {
        return 0;
    }

    let must_keep = min_recent.min(history.len());
    let must_keep_start = history.len().saturating_sub(must_keep);

    let mut used_tokens = 0usize;
    let mut candidate_split = history.len();

    for i in (0..history.len()).rev() {
        let msg_tokens = estimate_message_tokens(&history[i]);
        let new_total = used_tokens + msg_tokens;

        if i >= must_keep_start {
            used_tokens = new_total;
            candidate_split = i;
            continue;
        }

        if new_total > budget_tokens {
            break;
        }

        used_tokens = new_total;
        candidate_split = i;
    }

    adjust_split_for_tool_pairs(history, candidate_split)
}

/// Adjust split to avoid orphaning tool call/result pairs.
fn adjust_split_for_tool_pairs(history: &[Message], mut split: usize) -> usize {
    // If first kept message is Tool, move split backward to include paired Assistant
    while split > 0 && history[split].role == Role::Tool {
        split -= 1;
    }

    // If last dropped message is Assistant with tool_calls,
    // move split forward to drop orphaned tool results
    if split > 0 {
        let last_dropped = &history[split - 1];
        if last_dropped.role == Role::Assistant && last_dropped.tool_calls.is_some() {
            while split < history.len() && history[split].role == Role::Tool {
                split += 1;
            }
        }
    }

    split
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
            if msg.role == Role::Assistant {
                if let Some(ref calls) = msg.tool_calls {
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
            if msg.role == Role::Assistant {
                if let Some(ref calls) = msg.tool_calls {
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

    // -----------------------------------------------------------------------
    // Artifact compaction tests
    // -----------------------------------------------------------------------

    #[test]
    fn small_tool_result_not_compacted() {
        let small_content = "x".repeat(100);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls("", vec![ToolCall::new("c1", "search", json!({}))]),
            Message::tool("c1", &small_content),
        ];
        compact_tool_results(&mut messages);
        assert_eq!(messages[2].text(), small_content);
    }

    #[test]
    fn large_tool_result_compacted_to_preview() {
        // 2048 tokens * 4 chars/token = 8192 chars needed to exceed threshold
        let large_content = "a".repeat(10_000);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "list_files", json!({}))],
            ),
            Message::tool("c1", &large_content),
        ];
        compact_tool_results(&mut messages);

        let result = messages[2].text();
        assert!(
            result.len() < large_content.len(),
            "content should be shorter after compaction"
        );
        assert!(
            result.contains("[Content compacted:"),
            "should contain compaction indicator"
        );
        assert!(result.contains("tokens"), "indicator should mention tokens");
        assert!(result.contains("chars"), "indicator should mention chars");
    }

    #[test]
    fn compact_preserves_non_tool_messages() {
        let large_text = "b".repeat(10_000);
        let mut messages = vec![
            Message::system(&large_text),
            Message::user(&large_text),
            Message::assistant(&large_text),
        ];
        let texts_before: Vec<String> = messages.iter().map(|m| m.text()).collect();
        compact_tool_results(&mut messages);
        let texts_after: Vec<String> = messages.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts_before, texts_after,
            "non-tool messages should be unchanged"
        );
    }

    #[test]
    fn compact_artifact_below_threshold_unchanged() {
        let content = "short content";
        let result = compact_artifact(content);
        assert_eq!(result, content);
    }

    #[test]
    fn compact_artifact_above_threshold_truncates() {
        let content = "x".repeat(10_000);
        let result = compact_artifact(&content);
        assert!(result.len() < content.len());
        assert!(result.contains("[Content compacted:"));
    }

    #[test]
    fn compact_artifact_respects_line_limit() {
        // Create content with many lines that exceeds threshold
        let content: String = (0..100)
            .map(|i| format!("line {}: {}", i, "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let result = compact_artifact(&content);
        // Count lines in the preview part (before the compaction indicator)
        let lines_before_indicator = result
            .split("[Content compacted:")
            .next()
            .unwrap_or("")
            .lines()
            .count();
        assert!(
            lines_before_indicator <= ARTIFACT_PREVIEW_MAX_LINES + 1,
            "should respect line limit, got {} lines",
            lines_before_indicator
        );
    }

    #[test]
    fn compact_tool_results_multiple_tool_messages() {
        let small = "x".repeat(100);
        let large = "y".repeat(10_000);
        let mut messages = vec![
            Message::user("go"),
            Message::assistant_with_tool_calls(
                "",
                vec![
                    ToolCall::new("c1", "small", json!({})),
                    ToolCall::new("c2", "large", json!({})),
                ],
            ),
            Message::tool("c1", &small),
            Message::tool("c2", &large),
        ];
        compact_tool_results(&mut messages);

        // Small tool result unchanged
        assert_eq!(messages[2].text(), small);
        // Large tool result compacted
        assert!(messages[3].text().len() < large.len());
        assert!(messages[3].text().contains("[Content compacted:"));
    }

    #[test]
    fn compact_artifact_boundary_just_under_threshold() {
        // Exactly at threshold: 2048 * 4 = 8192 chars
        let content = "a".repeat(8191);
        let result = compact_artifact(&content);
        // 8191 / 4 = 2047, which is < 2048 threshold
        assert_eq!(result, content, "just under threshold should not compact");
    }

    #[test]
    fn compact_artifact_boundary_at_threshold() {
        // At threshold: 2048 * 4 = 8192 chars
        let content = "a".repeat(8192);
        let result = compact_artifact(&content);
        // 8192 / 4 = 2048, which is NOT < 2048
        assert!(result.len() < content.len(), "at threshold should compact");
    }
}
