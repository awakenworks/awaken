//! Compaction boundary discovery and load-time trimming.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_contract::contract::message::{Message, Role, Visibility};

/// Find a safe compaction boundary in the message history.
///
/// Returns the index of the last message that can be safely compacted
/// (all tool call/result pairs are complete before this point).
pub fn find_compaction_boundary(
    messages: &[Arc<Message>],
    start: usize,
    end: usize,
) -> Option<usize> {
    let mut open_calls = HashSet::<String>::new();
    let mut best_boundary = None;

    for (idx, msg) in messages.iter().enumerate().skip(start).take(end - start) {
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                open_calls.insert(call.id.clone());
            }
        }

        if msg.role == Role::Tool
            && let Some(ref call_id) = msg.tool_call_id
        {
            open_calls.remove(call_id);
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

/// Trim loaded messages to the latest compaction boundary.
///
/// If the message list contains a `<conversation-summary>` internal_system message,
/// all messages before it are dropped. The summary message becomes the first message.
/// This avoids loading already-summarized history into the context window.
///
/// Idempotent: if no summary exists or messages are already trimmed, this is a no-op.
pub fn trim_to_compaction_boundary(messages: &mut Vec<Arc<Message>>) {
    // Find the last summary message (in case of multiple compactions)
    let last_summary_idx = messages.iter().rposition(|m| {
        m.role == Role::System
            && m.visibility == Visibility::Internal
            && m.text().contains("<conversation-summary>")
    });

    if let Some(idx) = last_summary_idx
        && idx > 0
    {
        messages.drain(..idx);
    }
}

/// Record a compaction boundary in the state store.
pub fn record_compaction_boundary(
    boundary: super::plugin::CompactionBoundary,
) -> super::plugin::CompactionAction {
    super::plugin::CompactionAction::RecordBoundary(boundary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::ToolCall;
    use serde_json::json;

    #[test]
    fn find_compaction_boundary_respects_tool_pairs() {
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
    fn trim_to_compaction_boundary_drops_pre_summary() {
        let mut messages = vec![
            Arc::new(Message::user("old msg 1")),
            Arc::new(Message::assistant("old reply")),
            Arc::new(Message::internal_system(
                "<conversation-summary>\nSummary of old messages\n</conversation-summary>",
            )),
            Arc::new(Message::user("new msg")),
            Arc::new(Message::assistant("new reply")),
        ];

        trim_to_compaction_boundary(&mut messages);
        assert_eq!(messages.len(), 3);
        assert!(messages[0].text().contains("conversation-summary"));
        assert_eq!(messages[1].text(), "new msg");
    }

    #[test]
    fn trim_to_compaction_boundary_noop_without_summary() {
        let mut messages = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
        ];
        let len_before = messages.len();
        trim_to_compaction_boundary(&mut messages);
        assert_eq!(messages.len(), len_before);
    }

    #[test]
    fn find_compaction_boundary_does_not_cut_open_tool_round() {
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant("reply")),
            Arc::new(Message::user("next")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            // c1 has no result yet — open tool round
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Boundary should be before the open tool round (idx 2 at latest)
        if let Some(b) = boundary {
            assert!(b <= 2, "boundary should not include open tool round");
        }
    }

    #[test]
    fn trim_to_compaction_boundary_idempotent() {
        let mut messages = vec![
            Arc::new(Message::user("old")),
            Arc::new(Message::internal_system(
                "<conversation-summary>\nSummary\n</conversation-summary>",
            )),
            Arc::new(Message::user("new")),
        ];

        trim_to_compaction_boundary(&mut messages);
        let len_after_first = messages.len();

        trim_to_compaction_boundary(&mut messages);
        assert_eq!(
            messages.len(),
            len_after_first,
            "second trim should be noop"
        );
    }

    #[test]
    fn find_boundary_skips_open_tool_rounds() {
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant("ok")),
            Arc::new(Message::user("do something")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            // c1 result is missing — open tool round
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Must not place boundary at or after the open tool call (idx 3)
        match boundary {
            Some(b) => assert!(b < 3, "boundary {b} must be before open tool call at idx 3"),
            None => {} // also acceptable if no safe boundary exists
        }
    }

    #[test]
    fn find_boundary_respects_suffix_messages() {
        // Search only within a sub-range, leaving suffix messages untouched
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("old1")),
            Arc::new(Message::assistant("reply1")),
            Arc::new(Message::user("old2")),
            Arc::new(Message::assistant("reply2")),
            // suffix: last 2 messages are "raw suffix"
            Arc::new(Message::user("recent")),
            Arc::new(Message::assistant("recent_reply")),
        ];

        let suffix_count = 2;
        let search_end = messages.len().saturating_sub(suffix_count);
        let boundary = find_compaction_boundary(&messages, 0, search_end);
        // Boundary must be within the searched range, not touching suffix
        if let Some(b) = boundary {
            assert!(
                b < search_end,
                "boundary {b} must be before suffix start {search_end}"
            );
        }
    }

    #[test]
    fn find_boundary_returns_none_when_too_few_messages() {
        // Single message — no safe compaction point
        let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("only message"))];
        // Search range is empty (start == end)
        let boundary = find_compaction_boundary(&messages, 0, 0);
        assert!(boundary.is_none(), "empty range should yield no boundary");

        // Range with only an open tool call — no safe boundary
        let messages2: Vec<Arc<Message>> = vec![Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "fn", json!({}))],
        ))];
        let boundary2 = find_compaction_boundary(&messages2, 0, messages2.len());
        assert!(
            boundary2.is_none(),
            "single open tool call should yield no boundary"
        );
    }

    #[test]
    fn find_compaction_boundary_multiple_complete_tool_rounds() {
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c1", "search", json!({}))],
            )),
            Arc::new(Message::tool("c1", "found it")),
            Arc::new(Message::user("next")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new("c2", "read", json!({}))],
            )),
            Arc::new(Message::tool("c2", "content")),
            Arc::new(Message::user("last")),
            Arc::new(Message::assistant("done")),
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        assert!(boundary.is_some());
        // Should be at or after idx 6 (after second tool round)
        let b = boundary.unwrap();
        assert!(
            b >= 6,
            "boundary should be after all tool rounds: got {}",
            b
        );
    }

    #[test]
    fn find_compaction_boundary_empty_range() {
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("hello")),
            Arc::new(Message::assistant("hi")),
        ];
        let boundary = find_compaction_boundary(&messages, 0, 0);
        assert!(boundary.is_none(), "empty range should yield no boundary");
    }

    #[test]
    fn find_compaction_boundary_range_start_equals_end() {
        let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("only"))];
        let boundary = find_compaction_boundary(&messages, 1, 1);
        assert!(boundary.is_none());
    }

    #[test]
    fn trim_to_compaction_boundary_uses_last_summary() {
        let mut messages = vec![
            Arc::new(Message::user("old msg 1")),
            Arc::new(Message::internal_system(
                "<conversation-summary>\nFirst summary\n</conversation-summary>",
            )),
            Arc::new(Message::user("mid msg")),
            Arc::new(Message::internal_system(
                "<conversation-summary>\nSecond summary\n</conversation-summary>",
            )),
            Arc::new(Message::user("new msg")),
        ];

        trim_to_compaction_boundary(&mut messages);
        // Should trim to the LAST summary (index 3)
        assert_eq!(messages.len(), 2);
        assert!(messages[0].text().contains("Second summary"));
        assert_eq!(messages[1].text(), "new msg");
    }

    #[test]
    fn find_compaction_boundary_with_multiple_tool_calls_in_one_round() {
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("do things")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![
                    ToolCall::new("c1", "search", json!({})),
                    ToolCall::new("c2", "read", json!({})),
                ],
            )),
            Arc::new(Message::tool("c1", "found")),
            Arc::new(Message::tool("c2", "content")),
            Arc::new(Message::user("thanks")),
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        assert!(boundary.is_some());
        // Both tool results are present, so boundary can be after them
        let b = boundary.unwrap();
        assert!(
            b >= 3,
            "boundary should be after all tool results: got {}",
            b
        );
    }

    #[test]
    fn find_compaction_boundary_partial_tool_results() {
        // Two tool calls but only one result
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("start")),
            Arc::new(Message::assistant_with_tool_calls(
                "",
                vec![
                    ToolCall::new("c1", "search", json!({})),
                    ToolCall::new("c2", "read", json!({})),
                ],
            )),
            Arc::new(Message::tool("c1", "found")),
            // c2 result missing
        ];

        let boundary = find_compaction_boundary(&messages, 0, messages.len());
        // Should not place boundary after the incomplete tool round
        match boundary {
            Some(b) => assert!(b < 1, "boundary should not include incomplete tool round"),
            None => {} // also valid
        }
    }
}
