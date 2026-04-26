//! Compaction boundary discovery, plan/apply helpers, and load-time trimming.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_contract::contract::inference::ContextWindowPolicy;
use awaken_contract::contract::message::{Message, Role, Visibility};
use awaken_contract::contract::transform::estimate_message_tokens;

use super::summarizer::{MIN_COMPACTION_GAIN_TOKENS, extract_previous_summary, render_transcript};

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

/// Inputs needed to run a compaction off the main thread. Snapshotted at
/// trigger time so the background task does not race with the live
/// `messages` list (which keeps growing during summarization).
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Pre-rendered transcript to feed the summarizer (Internal messages
    /// already filtered).
    pub transcript: String,
    /// Previous cumulative summary, if any, for incremental updates.
    pub previous_summary: Option<String>,
    /// Stable id of the last message included in the summary. The swap
    /// path locates the cut point against the current message list by
    /// this id, so it survives any new messages appended in the window.
    pub boundary_message_id: String,
    /// Token estimate of the messages that the summary will replace.
    /// Used for the `pre_tokens` field of the recorded boundary.
    pub pre_tokens: usize,
}

/// Result of a successful in-place swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedCompaction {
    /// Index in the original list where the cut happened.
    pub boundary_index: usize,
    /// Tokens that were dropped from the head of the message list.
    pub pre_tokens: usize,
    /// Tokens used by the inserted summary message.
    pub post_tokens: usize,
}

/// Decide whether compaction should run right now and, if so, capture the
/// inputs needed by the background summarization task. Returns `None` if
/// compaction is not feasible (no safe boundary, savings below threshold,
/// boundary message has no stable id, transcript is empty, etc.).
pub fn plan_compaction(
    messages: &[Arc<Message>],
    policy: &ContextWindowPolicy,
) -> Option<CompactionPlan> {
    if messages.len() < 2 {
        return None;
    }
    let keep_suffix = policy.compaction_raw_suffix_messages.min(messages.len());
    let search_end = messages.len().saturating_sub(keep_suffix);
    if search_end < 2 {
        return None;
    }
    let boundary = find_compaction_boundary(messages, 0, search_end)?;
    let boundary_message_id = messages[boundary].id.clone()?;
    let pre_tokens: usize = messages[..=boundary]
        .iter()
        .map(|m| estimate_message_tokens(m))
        .sum();
    if pre_tokens < MIN_COMPACTION_GAIN_TOKENS {
        return None;
    }
    let transcript = render_transcript(&messages[..=boundary]);
    if transcript.is_empty() {
        return None;
    }
    let previous_summary = extract_previous_summary(messages);
    Some(CompactionPlan {
        transcript,
        previous_summary,
        boundary_message_id,
        pre_tokens,
    })
}

/// Apply a freshly produced summary to the live message list. Locates the
/// boundary message by id (not by index) so it is safe against any
/// messages appended between trigger and completion. Returns `None` when
/// the boundary message is no longer present (already trimmed by an
/// earlier compaction or rewritten by another path); callers should treat
/// that as a benign skip.
pub fn apply_summary(
    messages: &mut Vec<Arc<Message>>,
    boundary_message_id: &str,
    summary_text: &str,
) -> Option<AppliedCompaction> {
    let idx = messages
        .iter()
        .position(|m| m.id.as_deref() == Some(boundary_message_id))?;
    let pre_tokens: usize = messages[..=idx]
        .iter()
        .map(|m| estimate_message_tokens(m))
        .sum();
    messages.drain(..=idx);
    let summary_message = Arc::new(Message::internal_system(format!(
        "<conversation-summary>\n{summary_text}\n</conversation-summary>"
    )));
    let post_tokens = estimate_message_tokens(&summary_message);
    messages.insert(0, summary_message);
    Some(AppliedCompaction {
        boundary_index: idx,
        pre_tokens,
        post_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::ToolCall;
    use serde_json::json;

    fn long_user(text: &str, copies: usize) -> Arc<Message> {
        Arc::new(Message::user(text.repeat(copies)))
    }

    #[test]
    fn plan_compaction_returns_none_when_savings_below_threshold() {
        let messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("hi")),
            Arc::new(Message::assistant("hello")),
            Arc::new(Message::user("how are you?")),
            Arc::new(Message::assistant("fine")),
        ];
        let policy = ContextWindowPolicy {
            compaction_raw_suffix_messages: 1,
            ..Default::default()
        };
        assert!(plan_compaction(&messages, &policy).is_none());
    }

    #[test]
    fn plan_compaction_captures_boundary_message_id() {
        // Pad the head with enough tokens to clear MIN_COMPACTION_GAIN_TOKENS.
        let mut messages: Vec<Arc<Message>> = (0..6)
            .map(|i| {
                if i % 2 == 0 {
                    long_user("filler ", 600)
                } else {
                    Arc::new(Message::assistant("ack"))
                }
            })
            .collect();
        messages.push(Arc::new(Message::user("recent")));
        let policy = ContextWindowPolicy {
            compaction_raw_suffix_messages: 1,
            ..Default::default()
        };
        let plan = plan_compaction(&messages, &policy).expect("plan");
        // Boundary id must reference an actual message in the snapshot.
        assert!(
            messages
                .iter()
                .any(|m| m.id.as_deref() == Some(plan.boundary_message_id.as_str()))
        );
        assert!(plan.pre_tokens >= MIN_COMPACTION_GAIN_TOKENS);
        assert!(!plan.transcript.is_empty());
    }

    #[test]
    fn apply_summary_swaps_when_boundary_present() {
        let mut messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("old1")),
            Arc::new(Message::assistant("old2")),
            Arc::new(Message::user("BOUNDARY")),
            Arc::new(Message::assistant("after-boundary")),
            Arc::new(Message::user("appended-during-window")),
        ];
        let boundary_id = messages[2].id.clone().unwrap();

        let applied = apply_summary(&mut messages, &boundary_id, "synthetic summary").unwrap();
        assert_eq!(applied.boundary_index, 2);
        assert!(applied.pre_tokens > 0);
        assert!(applied.post_tokens > 0);

        // First message must now be the summary; messages after the boundary
        // (including ones appended during the compaction window) are kept.
        assert!(
            messages[0]
                .text()
                .contains("<conversation-summary>\nsynthetic summary"),
            "summary missing or malformed: {}",
            messages[0].text()
        );
        assert_eq!(messages[1].text(), "after-boundary");
        assert_eq!(messages[2].text(), "appended-during-window");
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn apply_summary_returns_none_when_boundary_already_gone() {
        let mut messages: Vec<Arc<Message>> = vec![
            Arc::new(Message::user("a")),
            Arc::new(Message::assistant("b")),
        ];
        let original = messages.clone();
        assert!(apply_summary(&mut messages, "non-existent-id", "any").is_none());
        // Skip must be benign: the live list is unchanged.
        assert_eq!(messages.len(), original.len());
        for (a, b) in messages.iter().zip(original.iter()) {
            assert_eq!(a.text(), b.text());
        }
    }

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
        if let Some(b) = boundary {
            assert!(b < 3, "boundary {b} must be before open tool call at idx 3");
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
        if let Some(b) = boundary {
            assert!(b < 1, "boundary should not include incomplete tool round");
        }
    }
}
