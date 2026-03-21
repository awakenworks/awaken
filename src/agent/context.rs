//! Built-in context management: hard truncation to token budget.

use crate::contract::inference::ContextWindowPolicy;
use crate::contract::message::{Message, Role};
use crate::contract::tool::ToolDescriptor;
use crate::contract::transform::{
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
        if absolute_split > system_end {
            messages.drain(system_end..absolute_split);
        }

        // Repair dangling tool calls after truncation
        patch_dangling_tool_calls(&mut messages);

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

// ---------------------------------------------------------------------------
// LLM compaction
// ---------------------------------------------------------------------------

/// Find a safe compaction boundary in the message history.
///
/// Returns the index of the last message that can be safely compacted
/// (all tool call/result pairs are complete before this point).
pub fn find_compaction_boundary(
    messages: &[std::sync::Arc<Message>],
    start: usize,
    end: usize,
) -> Option<usize> {
    use std::collections::HashSet;

    let mut open_calls = HashSet::<String>::new();
    let mut best_boundary = None;

    for (idx, msg) in messages.iter().enumerate().skip(start).take(end - start) {
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                open_calls.insert(call.id.clone());
            }
        }

        if msg.role == Role::Tool {
            if let Some(ref call_id) = msg.tool_call_id {
                open_calls.remove(call_id);
            }
        }

        // Safe boundary: all tool calls resolved and next isn't a tool result
        let next_is_tool = messages
            .get(idx + 1)
            .is_some_and(|next| next.role == Role::Tool);

        if open_calls.is_empty() && !next_is_tool {
            best_boundary = Some(idx);
        }
    }

    best_boundary
}

/// Render messages as a text transcript for LLM summarization.
pub fn render_transcript(messages: &[std::sync::Arc<Message>]) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let text = msg.text();
        if !text.is_empty() {
            lines.push(format!("[{role}]: {text}"));
        }
    }
    lines.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::content::ContentBlock;
    use crate::contract::message::ToolCall;
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
    fn find_compaction_boundary_respects_tool_pairs() {
        use std::sync::Arc;

        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            Arc::new(Message::tool("c1", "found")),
            Arc::new(Message::user("next")), // safe boundary here (idx 3)
            Arc::new(Message::assistant("reply")),
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Should be at idx 3 or 4 (after tool pair is complete)
        assert!(boundary.is_some());
        let b = boundary.unwrap();
        assert!(b >= 3);
    }

    #[test]
    fn render_transcript_formats_correctly() {
        use std::sync::Arc;

        let messages = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi there")),
        ];
        let transcript = render_transcript(&messages);
        assert!(transcript.contains("[User]: hello"));
        assert!(transcript.contains("[Assistant]: hi there"));
    }
}
