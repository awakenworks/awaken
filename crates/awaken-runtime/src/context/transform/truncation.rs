//! Message truncation: find split points that respect token budgets and tool-call pairing.

use awaken_contract::contract::message::{Message, Role};
use awaken_contract::contract::transform::estimate_message_tokens;

/// Find the split point in history that fits the token budget.
///
/// Always keeps at least `min_recent` messages from the end.
/// Adjusts boundaries to avoid splitting tool call/result pairs.
pub fn find_split_point(history: &[Message], budget_tokens: usize, min_recent: usize) -> usize {
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
pub fn adjust_split_for_tool_pairs(history: &[Message], mut split: usize) -> usize {
    if split == 0 || split >= history.len() {
        return split;
    }

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
    use awaken_contract::contract::message::ToolCall;
    use serde_json::json;

    #[test]
    fn find_split_empty_history() {
        let split = find_split_point(&[], 1000, 5);
        assert_eq!(split, 0);
    }

    #[test]
    fn find_split_all_within_budget() {
        let history = vec![Message::user("Hi"), Message::assistant("Hello!")];
        let split = find_split_point(&history, 100_000, 2);
        assert_eq!(split, 0, "everything fits, nothing to drop");
    }

    #[test]
    fn find_split_drops_oldest_when_tight() {
        let history: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("msg {i}"))
                } else {
                    Message::assistant(format!("reply {i}"))
                }
            })
            .collect();
        let split = find_split_point(&history, 30, 2);
        assert!(split > 0, "some messages should be dropped");
        let kept = history.len() - split;
        assert!(kept >= 2, "must keep at least min_recent=2");
    }

    #[test]
    fn find_split_respects_min_recent_even_beyond_budget() {
        let history: Vec<Message> = (0..10)
            .map(|i| Message::user(format!("message {i} with padding")))
            .collect();
        let split = find_split_point(&history, 1, 5);
        let kept = history.len() - split;
        assert!(kept >= 5, "must keep at least min_recent=5, kept={kept}");
    }

    #[test]
    fn find_split_min_recent_exceeds_history_len() {
        let history = vec![Message::user("a"), Message::assistant("b")];
        let split = find_split_point(&history, 1, 100);
        assert_eq!(split, 0, "min_recent > len means keep all");
    }

    #[test]
    fn adjust_split_moves_back_for_orphaned_tool_result() {
        let history = vec![
            Message::user("a"),
            Message::assistant_with_tool_calls("b", vec![ToolCall::new("c1", "t", json!({}))]),
            Message::tool("c1", "result"),
            Message::user("c"),
        ];
        let adjusted = adjust_split_for_tool_pairs(&history, 2);
        assert_eq!(adjusted, 1, "should include the assistant with tool calls");
    }

    #[test]
    fn adjust_split_drops_orphaned_results_after_dropped_assistant() {
        let history = vec![
            Message::user("a"),
            Message::assistant_with_tool_calls("b", vec![ToolCall::new("c1", "t", json!({}))]),
            Message::tool("c1", "result"),
            Message::user("c"),
            Message::assistant("answer"),
        ];
        let adjusted = adjust_split_for_tool_pairs(&history, 3);
        assert_eq!(adjusted, 3, "split at user boundary should be stable");
    }

    #[test]
    fn adjust_split_handles_multiple_consecutive_tool_results() {
        let history = vec![
            Message::user("start"),
            Message::assistant_with_tool_calls(
                "calling two",
                vec![
                    ToolCall::new("c1", "t1", json!({})),
                    ToolCall::new("c2", "t2", json!({})),
                ],
            ),
            Message::tool("c1", "r1"),
            Message::tool("c2", "r2"),
            Message::user("continue"),
        ];
        assert_eq!(adjust_split_for_tool_pairs(&history, 2), 1);
        assert_eq!(adjust_split_for_tool_pairs(&history, 3), 1);
    }

    #[test]
    fn adjust_split_at_zero_is_noop() {
        let history = vec![Message::user("a"), Message::assistant("b")];
        assert_eq!(adjust_split_for_tool_pairs(&history, 0), 0);
    }

    #[test]
    fn adjust_split_at_len_minus_one_non_tool() {
        // When split is at the last element and it's not a Tool, no adjustment needed
        let history = vec![Message::user("a"), Message::assistant("b")];
        assert_eq!(adjust_split_for_tool_pairs(&history, 1), 1);
    }

    #[test]
    fn adjust_split_at_end_returns_len() {
        let history = vec![Message::user("a"), Message::assistant("b")];
        // split == history.len() means keep nothing; guard returns early
        assert_eq!(adjust_split_for_tool_pairs(&history, 2), 2);
    }

    #[test]
    fn adjust_split_backward_then_no_forward() {
        let history = vec![
            Message::user("start"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![ToolCall::new("c1", "t", json!({}))],
            ),
            Message::tool("c1", "result"),
            Message::user("next"),
        ];
        let adjusted = adjust_split_for_tool_pairs(&history, 2);
        assert_eq!(adjusted, 1);
    }

    // -----------------------------------------------------------------------
    // Additional truncation tests
    // -----------------------------------------------------------------------

    #[test]
    fn find_split_with_various_history_lengths() {
        // Verify split works correctly for history lengths 1 through 30
        for len in 1..=30 {
            let history: Vec<Message> = (0..len)
                .map(|i| {
                    if i % 2 == 0 {
                        Message::user(format!("u{i}"))
                    } else {
                        Message::assistant(format!("a{i}"))
                    }
                })
                .collect();
            let split = find_split_point(&history, 100_000, 2);
            assert!(split <= history.len(), "split out of range for len={len}");
            let kept = history.len() - split;
            assert!(
                kept >= 2.min(history.len()),
                "min_recent not honored for len={len}: kept={kept}"
            );
        }
    }

    #[test]
    fn token_estimation_accuracy_relative_to_content_size() {
        // Longer messages should estimate more tokens
        let short = Message::user("hi");
        let long = Message::user(&"x".repeat(400));
        let short_tokens = estimate_message_tokens(&short);
        let long_tokens = estimate_message_tokens(&long);
        assert!(
            long_tokens > short_tokens,
            "longer message should estimate more tokens: short={short_tokens}, long={long_tokens}"
        );
        // The 400-char message should estimate roughly 100 content tokens + 4 overhead = 104
        assert!(
            long_tokens >= 100,
            "400-char message should be >= 100 tokens, got {long_tokens}"
        );
    }

    #[test]
    fn find_split_edge_single_message() {
        let history = vec![Message::user("only one")];
        // Budget is huge, min_recent=1 => keep everything
        assert_eq!(find_split_point(&history, 100_000, 1), 0);
        // Budget is tiny, min_recent=1 => still keep 1
        assert_eq!(find_split_point(&history, 1, 1), 0);
        // Budget is tiny, min_recent=0 => can drop everything
        let split = find_split_point(&history, 1, 0);
        // With min_recent=0 and tiny budget, the message may exceed budget
        assert!(split <= 1);
    }

    #[test]
    fn tool_pair_preserved_across_truncation_boundary() {
        // Tool call + multiple results should never be split
        let history = vec![
            Message::user("old stuff with lots of padding text to consume budget"),
            Message::assistant("old reply with lots of padding text to consume budget"),
            Message::user("trigger"),
            Message::assistant_with_tool_calls(
                "calling tools",
                vec![
                    ToolCall::new("c1", "search", json!({})),
                    ToolCall::new("c2", "read", json!({})),
                    ToolCall::new("c3", "write", json!({})),
                ],
            ),
            Message::tool("c1", "result1"),
            Message::tool("c2", "result2"),
            Message::tool("c3", "result3"),
            Message::user("final"),
        ];
        // Try various split points and verify tool pairs stay intact
        for candidate in 0..history.len() {
            let adjusted = adjust_split_for_tool_pairs(&history, candidate);
            if adjusted > 0 && adjusted < history.len() {
                assert_ne!(
                    history[adjusted].role,
                    Role::Tool,
                    "adjusted split at {adjusted} should not start with Tool"
                );
            }
        }
    }

    #[test]
    fn context_window_policy_threshold_triggers_truncation() {
        // Verify that when total tokens exceed (max_context - max_output), truncation occurs
        let history: Vec<Message> = (0..50)
            .map(|i| Message::user(format!("message number {i} with some padding text")))
            .collect();
        let total_tokens: usize = history.iter().map(|m| estimate_message_tokens(m)).sum();
        // Set budget to half the total
        let budget = total_tokens / 2;
        let split = find_split_point(&history, budget, 2);
        assert!(split > 0, "should truncate when over budget");
        // Verify kept messages fit in budget (approximately)
        let kept_tokens: usize = history[split..]
            .iter()
            .map(|m| estimate_message_tokens(m))
            .sum();
        assert!(
            kept_tokens <= budget || (history.len() - split) <= 2,
            "kept tokens {kept_tokens} should be <= budget {budget} (unless forced by min_recent)"
        );
    }

    #[test]
    fn truncation_with_only_system_prompt_no_history() {
        // If the split point function receives an empty history slice, it should return 0
        let split = find_split_point(&[], 1000, 5);
        assert_eq!(split, 0);
        let adjusted = adjust_split_for_tool_pairs(&[], 0);
        assert_eq!(adjusted, 0);
    }

    #[test]
    fn truncation_preserves_recent_n_messages_exactly() {
        let history: Vec<Message> = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("u{i}"))
                } else {
                    Message::assistant(format!("a{i}"))
                }
            })
            .collect();
        // Set budget to 1 token (impossibly small) but min_recent=6
        let split = find_split_point(&history, 1, 6);
        let kept = history.len() - split;
        assert!(kept >= 6, "should keep at least min_recent=6, got {kept}");
    }

    #[test]
    fn mixed_message_types_truncation() {
        // Mix of user, assistant, tool call, tool result messages
        let history = vec![
            Message::user("old user msg"),
            Message::assistant("old assistant msg"),
            Message::user("mid user msg"),
            Message::assistant_with_tool_calls(
                "mid tool call",
                vec![ToolCall::new("c1", "search", json!({}))],
            ),
            Message::tool("c1", "mid tool result"),
            Message::user("recent user"),
            Message::assistant("recent assistant"),
            Message::user("latest user"),
            Message::assistant("latest reply"),
        ];
        let split = find_split_point(&history, 40, 2);
        assert!(split > 0, "tight budget should force truncation");
        let kept = &history[split..];
        assert!(kept.len() >= 2, "must keep min_recent=2");
        // First kept message should not be orphaned Tool
        assert_ne!(
            kept[0].role,
            Role::Tool,
            "first kept message must not be orphaned tool result"
        );
    }

    #[test]
    fn very_large_message_handling() {
        // A single very large message should be handled without panic
        let huge_text = "x".repeat(100_000);
        let history = vec![
            Message::user(&huge_text),
            Message::assistant("small reply"),
            Message::user("another"),
            Message::assistant("end"),
        ];
        let huge_tokens = estimate_message_tokens(&history[0]);
        assert!(huge_tokens > 20_000, "huge message should have many tokens");
        // With a small budget, the huge message should be dropped
        let split = find_split_point(&history, 100, 2);
        assert!(split >= 1, "should drop the huge message");
    }

    #[test]
    fn adjust_split_forward_drops_orphaned_results() {
        // When the last dropped message is an assistant with tool_calls,
        // all following tool results should also be dropped
        let history = vec![
            Message::user("a"),
            Message::assistant_with_tool_calls(
                "calling",
                vec![
                    ToolCall::new("c1", "t1", json!({})),
                    ToolCall::new("c2", "t2", json!({})),
                ],
            ),
            Message::tool("c1", "r1"),
            Message::tool("c2", "r2"),
            Message::user("keep this"),
            Message::assistant("keep reply"),
        ];
        // Split at 2 means we'd keep from index 2 onward (starting with Tool).
        // adjust_split should move backward to include the assistant, or forward past tools.
        let adjusted = adjust_split_for_tool_pairs(&history, 2);
        // Should move back to 1 (include assistant) since first kept is Tool
        assert_eq!(
            adjusted, 1,
            "should move back to include assistant with tool_calls"
        );
    }

    #[test]
    fn find_split_tool_pair_not_broken() {
        let history = vec![
            Message::user("old1"),
            Message::assistant("old_reply"),
            Message::user("Do something"),
            Message::assistant_with_tool_calls(
                "Using tool",
                vec![ToolCall::new("c1", "search", json!({"q": "x"}))],
            ),
            Message::tool("c1", "found it"),
            Message::assistant("Here is the answer."),
            Message::user("Thanks"),
            Message::assistant("Welcome!"),
        ];
        let split = find_split_point(&history, 60, 2);
        if split < history.len() {
            assert_ne!(
                history[split].role,
                Role::Tool,
                "first kept message must not be an orphaned tool result"
            );
        }
    }
}
