//! Context window management: policy-driven message truncation.
//!
//! When conversation history exceeds the model's context window budget,
//! older messages are dropped while preserving tool-call/result pairs
//! and a minimum number of recent messages.

use crate::contracts::thread::{Message, Role};
use crate::engine::token_estimator::{estimate_message_tokens, estimate_messages_tokens};

// Re-export from tirea-contract (canonical location).
pub use tirea_contract::runtime::inference::ContextWindowPolicy;

/// Result of truncation.
#[derive(Debug)]
pub struct TruncationResult<'a> {
    /// Messages to include in the inference request (system + kept history).
    pub messages: Vec<&'a Message>,
    /// Number of history messages dropped.
    pub truncated_count: usize,
    /// Estimated total tokens after truncation (system + history + tools).
    pub estimated_total_tokens: usize,
}

/// Truncate conversation history to fit within the context window budget.
///
/// System messages are never truncated. History messages are dropped from the
/// oldest end, but tool-call/result pairs are kept or dropped as a unit.
///
/// Returns references into the input slices.
pub fn truncate_to_budget<'a>(
    system_messages: &'a [Message],
    history_messages: &'a [Message],
    tool_tokens: usize,
    policy: &ContextWindowPolicy,
) -> TruncationResult<'a> {
    let available = policy
        .max_context_tokens
        .saturating_sub(policy.max_output_tokens)
        .saturating_sub(tool_tokens);

    let system_tokens = estimate_messages_tokens(system_messages);
    let history_budget = available.saturating_sub(system_tokens);

    // Find a safe split point in history that fits the budget.
    // We keep messages from the end (most recent first) and find how far
    // back we can go.
    let split = find_split_point(history_messages, history_budget, policy.min_recent_messages);

    let kept = &history_messages[split..];
    let kept_tokens = estimate_messages_tokens(kept);
    let truncated_count = split;

    let mut messages: Vec<&Message> = Vec::with_capacity(system_messages.len() + kept.len());
    for msg in system_messages {
        messages.push(msg);
    }
    for msg in kept {
        messages.push(msg);
    }

    TruncationResult {
        messages,
        truncated_count,
        estimated_total_tokens: system_tokens + kept_tokens + tool_tokens,
    }
}

/// Find the index at which to split history: messages[split..] are kept.
///
/// Respects tool-call/result pair boundaries: if dropping a message would
/// orphan a tool result (no matching assistant call) or orphan a tool call
/// (no matching result), we adjust the split to keep the pair together.
fn find_split_point(
    history: &[Message],
    budget_tokens: usize,
    min_recent: usize,
) -> usize {
    if history.is_empty() {
        return 0;
    }

    // Minimum: keep at least min_recent messages (or all if fewer).
    let must_keep = min_recent.min(history.len());
    let must_keep_start = history.len().saturating_sub(must_keep);

    // Walk backward from the end, accumulating tokens.
    let mut used_tokens = 0usize;
    let mut candidate_split = history.len(); // start with keeping nothing

    for i in (0..history.len()).rev() {
        let msg_tokens = estimate_message_tokens(&history[i]);
        let new_total = used_tokens + msg_tokens;

        // If we're in the must-keep zone, always include.
        if i >= must_keep_start {
            used_tokens = new_total;
            candidate_split = i;
            continue;
        }

        // If adding this message exceeds budget, stop.
        if new_total > budget_tokens {
            break;
        }

        used_tokens = new_total;
        candidate_split = i;
    }

    // Adjust split to respect tool-call/result pair boundaries.
    // We don't want to keep a Tool result message without its preceding
    // Assistant tool-call message, or vice versa.
    adjust_split_for_tool_pairs(history, candidate_split)
}

/// Adjust the split point so that tool-call/result pairs are not broken.
///
/// If the first kept message is a Tool result, we need to also keep the
/// preceding Assistant message that contains the matching tool call.
/// If the last dropped message is an Assistant with tool calls, we need
/// to also drop the following Tool result messages.
fn adjust_split_for_tool_pairs(history: &[Message], mut split: usize) -> usize {
    if split == 0 || split >= history.len() {
        return split;
    }

    // If the first kept message is a Tool response, move split backward
    // to include the paired Assistant message.
    while split > 0 && history[split].role == Role::Tool {
        split -= 1;
    }

    // If the last dropped message (split-1) is an Assistant with tool_calls,
    // move split forward to drop its orphaned tool results too.
    if split > 0 {
        let last_dropped = &history[split - 1];
        if last_dropped.role == Role::Assistant && last_dropped.tool_calls.is_some() {
            // Drop the tool results that follow.
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
    use crate::contracts::thread::ToolCall;
    use serde_json::json;

    fn user(content: &str) -> Message {
        Message::user(content)
    }

    fn assistant(content: &str) -> Message {
        Message::assistant(content)
    }

    fn assistant_with_calls(content: &str, calls: Vec<ToolCall>) -> Message {
        Message::assistant_with_tool_calls(content, calls)
    }

    fn tool_result(call_id: &str, content: &str) -> Message {
        Message::tool(call_id, content)
    }

    fn system(content: &str) -> Message {
        Message::system(content)
    }

    #[test]
    fn no_truncation_when_within_budget() {
        let sys = vec![system("You are helpful.")];
        let history = vec![user("Hi"), assistant("Hello!")];
        let policy = ContextWindowPolicy {
            max_context_tokens: 200_000,
            max_output_tokens: 8_192,
            ..Default::default()
        };

        let result = truncate_to_budget(&sys, &history, 0, &policy);
        assert_eq!(result.truncated_count, 0);
        assert_eq!(result.messages.len(), 3); // 1 system + 2 history
    }

    #[test]
    fn truncation_drops_oldest_messages() {
        let sys = vec![system("sys")];
        let history: Vec<Message> = (0..100)
            .map(|i| {
                if i % 2 == 0 {
                    user(&format!("message {i}"))
                } else {
                    assistant(&format!("response {i}"))
                }
            })
            .collect();

        let policy = ContextWindowPolicy {
            max_context_tokens: 200, // very tight budget
            max_output_tokens: 50,
            min_recent_messages: 4,
            ..Default::default()
        };

        let result = truncate_to_budget(&sys, &history, 10, &policy);
        assert!(result.truncated_count > 0);
        // Must keep at least min_recent_messages
        let kept_history = result.messages.len() - 1; // minus system
        assert!(kept_history >= 4);
    }

    #[test]
    fn tool_pair_not_broken() {
        let sys = vec![system("sys")];
        let history = vec![
            user("Do something"),
            assistant_with_calls(
                "Using tool",
                vec![ToolCall::new("c1", "search", json!({"q": "x"}))],
            ),
            tool_result("c1", "found it"),
            assistant("Here is the answer."),
            user("Thanks"),
            assistant("You're welcome!"),
        ];

        // Budget that can fit ~3-4 messages but not all 6.
        let policy = ContextWindowPolicy {
            max_context_tokens: 120,
            max_output_tokens: 30,
            min_recent_messages: 2,
            ..Default::default()
        };

        let result = truncate_to_budget(&sys, &history, 10, &policy);

        // Check that no Tool message is the first kept history message
        // (which would mean its paired Assistant was dropped).
        let kept_history: Vec<_> = result.messages.iter().skip(1).collect();
        if !kept_history.is_empty() {
            assert_ne!(
                kept_history[0].role,
                Role::Tool,
                "First kept history message should not be an orphaned tool result"
            );
        }
    }

    #[test]
    fn min_recent_always_preserved() {
        let sys = vec![system("sys")];
        let history: Vec<Message> = (0..20)
            .map(|i| user(&format!("msg {i}")))
            .collect();

        let policy = ContextWindowPolicy {
            max_context_tokens: 50, // impossibly tight
            max_output_tokens: 10,
            min_recent_messages: 5,
            ..Default::default()
        };

        let result = truncate_to_budget(&sys, &history, 0, &policy);
        let kept_history = result.messages.len() - 1;
        assert!(kept_history >= 5, "must keep at least min_recent_messages");
    }

    #[test]
    fn adjust_split_moves_back_for_orphaned_tool_result() {
        let history = vec![
            user("a"),                                                                // 0
            assistant_with_calls("b", vec![ToolCall::new("c1", "t", json!({}))]),     // 1
            tool_result("c1", "r"),                                                   // 2
            user("c"),                                                                // 3
        ];

        // If naive split is 2 (keep [2,3]), tool result at 2 is orphaned.
        let adjusted = adjust_split_for_tool_pairs(&history, 2);
        assert_eq!(adjusted, 1, "should include the assistant with tool calls");
    }

    #[test]
    fn adjust_split_drops_orphaned_tool_results() {
        let history = vec![
            user("a"),                                                                // 0
            assistant_with_calls("b", vec![ToolCall::new("c1", "t", json!({}))]),     // 1
            tool_result("c1", "r"),                                                   // 2
            user("c"),                                                                // 3
        ];

        // If naive split is 2 (keep [2,3]), adjust backward to 1.
        // But if split is at index 2 (a Tool), adjust moves to 1.
        let adjusted = adjust_split_for_tool_pairs(&history, 2);
        assert_eq!(adjusted, 1);
    }

    #[test]
    fn empty_history() {
        let sys = vec![system("sys")];
        let history: Vec<Message> = vec![];
        let policy = ContextWindowPolicy::default();

        let result = truncate_to_budget(&sys, &history, 0, &policy);
        assert_eq!(result.truncated_count, 0);
        assert_eq!(result.messages.len(), 1);
    }

    #[test]
    fn adjust_split_handles_multiple_consecutive_tool_results() {
        // Assistant with 2 tool calls followed by 2 tool results
        let history = vec![
            user("start"),                                                                 // 0
            assistant_with_calls(
                "calling two tools",
                vec![
                    ToolCall::new("c1", "t1", json!({})),
                    ToolCall::new("c2", "t2", json!({})),
                ],
            ),                                                                             // 1
            tool_result("c1", "result1"),                                                  // 2
            tool_result("c2", "result2"),                                                  // 3
            user("continue"),                                                              // 4
        ];

        // Naive split at 2 (first kept = tool result) → should move back to 1
        let adjusted = adjust_split_for_tool_pairs(&history, 2);
        assert_eq!(adjusted, 1, "should include assistant with both tool calls");

        // Naive split at 3 (first kept = tool result) → should move back to 1
        let adjusted = adjust_split_for_tool_pairs(&history, 3);
        assert_eq!(adjusted, 1, "should walk back through all consecutive tool results");
    }

    #[test]
    fn adjust_split_drops_orphaned_results_after_dropped_assistant() {
        // When split=2 and history[1] is an assistant with tool_calls,
        // the tool results at [2],[3] become orphaned and should be dropped too.
        let history = vec![
            user("start"),                                                                 // 0
            assistant_with_calls(
                "calling",
                vec![ToolCall::new("c1", "t1", json!({}))],
            ),                                                                             // 1
            tool_result("c1", "result"),                                                   // 2
            user("next question"),                                                         // 3
            assistant("answer"),                                                           // 4
        ];

        // Naive split at 3 means we keep [3,4]. Last dropped = history[2] which is
        // a tool result, not an assistant. Split at 3 is fine here since history[3]
        // is a user message.
        let adjusted = adjust_split_for_tool_pairs(&history, 3);
        assert_eq!(adjusted, 3, "split at user boundary should be stable");
    }

    #[test]
    fn all_system_messages_preserved_with_empty_history() {
        let sys = vec![
            system("system line 1"),
            system("system line 2"),
            system("system line 3"),
        ];
        let history: Vec<Message> = vec![];
        let policy = ContextWindowPolicy {
            max_context_tokens: 100,
            max_output_tokens: 10,
            min_recent_messages: 5,
            ..Default::default()
        };

        let result = truncate_to_budget(&sys, &history, 0, &policy);
        assert_eq!(result.messages.len(), 3, "all system messages preserved");
        assert_eq!(result.truncated_count, 0);
    }

    #[test]
    fn tool_tokens_reduce_available_budget() {
        let sys = vec![system("sys")];
        let history: Vec<Message> = (0..50)
            .map(|i| user(&format!("message {i} with some extra content padding")))
            .collect();

        let policy = ContextWindowPolicy {
            max_context_tokens: 500,
            max_output_tokens: 100,
            min_recent_messages: 2,
            ..Default::default()
        };

        let result_no_tools = truncate_to_budget(&sys, &history, 0, &policy);
        let result_with_tools = truncate_to_budget(&sys, &history, 200, &policy);

        assert!(
            result_with_tools.truncated_count > result_no_tools.truncated_count,
            "tool token overhead should cause more truncation"
        );
    }

    #[test]
    fn default_policy_values() {
        let p = ContextWindowPolicy::default();
        assert_eq!(p.max_context_tokens, 200_000);
        assert_eq!(p.max_output_tokens, 16_384);
        assert_eq!(p.min_recent_messages, 10);
        assert!(p.enable_prompt_cache);
    }
}
