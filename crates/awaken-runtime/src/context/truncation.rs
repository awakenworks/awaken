//! Truncation recovery logic for the agent loop.
//!
//! When the LLM stops due to `MaxTokens` without emitting complete tool
//! calls, this module provides helpers to inject a continuation prompt and
//! re-enter inference.

use awaken_contract::contract::inference::StreamResult;
use awaken_contract::contract::message::{Message, Visibility};

/// Continuation prompt sent to the model after truncation.
const CONTINUATION_PROMPT: &str = "Your response was cut off because it exceeded the output token limit. \
     Please break your work into smaller pieces. Continue from where you left off.";

/// Mutable state for tracking recovery retries during a single run.
#[derive(Debug, Default)]
pub struct TruncationState {
    pub truncation_retries: usize,
}

impl TruncationState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Check if truncation recovery should retry inference.
///
/// Returns `true` (and increments the retry counter) when all three
/// conditions are met:
/// 1. The result needs truncation recovery (MaxTokens + incomplete tool calls)
/// 2. Haven't exceeded the configured max retries
/// 3. Configured retries > 0
pub fn should_retry(
    result: &StreamResult,
    state: &mut TruncationState,
    max_retries: usize,
) -> bool {
    if result.needs_truncation_recovery()
        && max_retries > 0
        && state.truncation_retries < max_retries
    {
        state.truncation_retries += 1;
        tracing::info!(
            retry = state.truncation_retries,
            max = max_retries,
            "truncation recovery: retrying after MaxTokens with incomplete tool calls"
        );
        true
    } else {
        false
    }
}

/// Build the continuation prompt message (Internal visibility).
pub fn continuation_message() -> Message {
    let mut msg = Message::user(CONTINUATION_PROMPT);
    msg.visibility = Visibility::Internal;
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::inference::{StopReason, TokenUsage};
    use awaken_contract::contract::message::ToolCall;
    use serde_json::json;

    // =====================================================================
    // Helpers
    // =====================================================================

    fn max_tokens_with_incomplete() -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                completion_tokens: Some(4096),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: true,
        }
    }

    fn end_turn_result() -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }
    }

    fn max_tokens_with_complete_tools() -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "search", json!({"q": "test"}))],
            usage: None,
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: false,
        }
    }

    fn tool_use_result() -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: vec![ToolCall::new("c1", "read_file", json!({"path": "/tmp"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        }
    }

    fn no_stop_reason_result() -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
            has_incomplete_tool_calls: false,
        }
    }

    fn max_tokens_no_incomplete() -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                completion_tokens: Some(4096),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::MaxTokens),
            has_incomplete_tool_calls: false,
        }
    }

    // =====================================================================
    // Core should_retry tests
    // =====================================================================

    #[test]
    fn triggers_retry_on_max_tokens_with_incomplete_tools() {
        let mut state = TruncationState::new();
        assert!(should_retry(&max_tokens_with_incomplete(), &mut state, 3));
        assert_eq!(state.truncation_retries, 1);
    }

    #[test]
    fn no_retry_on_end_turn() {
        let mut state = TruncationState::new();
        assert!(!should_retry(&end_turn_result(), &mut state, 3));
        assert_eq!(state.truncation_retries, 0);
    }

    #[test]
    fn no_retry_when_tools_are_complete() {
        let mut state = TruncationState::new();
        assert!(!should_retry(
            &max_tokens_with_complete_tools(),
            &mut state,
            3
        ));
        assert_eq!(state.truncation_retries, 0);
    }

    #[test]
    fn no_retry_on_tool_use_stop() {
        let mut state = TruncationState::new();
        assert!(!should_retry(&tool_use_result(), &mut state, 3));
        assert_eq!(state.truncation_retries, 0);
    }

    #[test]
    fn no_retry_when_stop_reason_is_none() {
        let mut state = TruncationState::new();
        assert!(!should_retry(&no_stop_reason_result(), &mut state, 3));
        assert_eq!(state.truncation_retries, 0);
    }

    #[test]
    fn no_retry_when_max_tokens_but_no_incomplete_tools() {
        let mut state = TruncationState::new();
        assert!(!should_retry(&max_tokens_no_incomplete(), &mut state, 3));
        assert_eq!(state.truncation_retries, 0);
    }

    #[test]
    fn no_retry_when_max_retries_is_zero() {
        let mut state = TruncationState::new();
        assert!(!should_retry(&max_tokens_with_incomplete(), &mut state, 0));
        assert_eq!(state.truncation_retries, 0);
    }

    // =====================================================================
    // Counter behavior
    // =====================================================================

    #[test]
    fn respects_max_retries() {
        let mut state = TruncationState::new();
        let max = 3;
        for i in 0..max {
            assert!(
                should_retry(&max_tokens_with_incomplete(), &mut state, max),
                "retry {i} should succeed"
            );
        }
        assert!(
            !should_retry(&max_tokens_with_incomplete(), &mut state, max),
            "retry after max should fail"
        );
        assert_eq!(state.truncation_retries, max);
    }

    #[test]
    fn counter_not_incremented_on_non_retry() {
        let mut state = TruncationState::new();
        assert!(!should_retry(&end_turn_result(), &mut state, 3));
        assert!(!should_retry(&tool_use_result(), &mut state, 3));
        assert!(!should_retry(&no_stop_reason_result(), &mut state, 3));
        assert!(!should_retry(
            &max_tokens_with_complete_tools(),
            &mut state,
            3
        ));
        assert_eq!(
            state.truncation_retries, 0,
            "counter should remain 0 after non-retry calls"
        );
    }

    #[test]
    fn counter_increments_only_on_actual_retry() {
        let mut state = TruncationState::new();
        // Non-retry calls
        should_retry(&end_turn_result(), &mut state, 3);
        should_retry(&tool_use_result(), &mut state, 3);
        assert_eq!(state.truncation_retries, 0);

        // Actual retry
        should_retry(&max_tokens_with_incomplete(), &mut state, 3);
        assert_eq!(state.truncation_retries, 1);

        // Non-retry again
        should_retry(&end_turn_result(), &mut state, 3);
        assert_eq!(state.truncation_retries, 1);

        // Another retry
        should_retry(&max_tokens_with_incomplete(), &mut state, 3);
        assert_eq!(state.truncation_retries, 2);
    }

    // =====================================================================
    // Mixed sequences
    // =====================================================================

    #[test]
    fn truncation_then_normal_end() {
        let mut state = TruncationState::new();
        assert!(should_retry(&max_tokens_with_incomplete(), &mut state, 3));
        assert_eq!(state.truncation_retries, 1);
        assert!(!should_retry(&end_turn_result(), &mut state, 3));
        assert_eq!(state.truncation_retries, 1);
    }

    #[test]
    fn truncation_then_tool_use() {
        let mut state = TruncationState::new();
        assert!(should_retry(&max_tokens_with_incomplete(), &mut state, 3));
        assert!(!should_retry(&tool_use_result(), &mut state, 3));
        assert_eq!(state.truncation_retries, 1);
    }

    #[test]
    fn exhaust_retries_then_truncation_is_refused() {
        let max = 3;
        let mut state = TruncationState::new();
        for _ in 0..max {
            assert!(should_retry(&max_tokens_with_incomplete(), &mut state, max));
        }
        assert!(!should_retry(
            &max_tokens_with_incomplete(),
            &mut state,
            max
        ));
        assert!(!should_retry(
            &max_tokens_with_incomplete(),
            &mut state,
            max
        ));
        assert_eq!(state.truncation_retries, max);
    }

    // =====================================================================
    // continuation_message tests
    // =====================================================================

    #[test]
    fn continuation_message_is_internal() {
        let msg = continuation_message();
        assert_eq!(msg.visibility, Visibility::Internal);
        assert_eq!(msg.role, awaken_contract::contract::message::Role::User);
    }

    #[test]
    fn continuation_message_mentions_token_limit() {
        let msg = continuation_message();
        let text = msg.text();
        assert!(
            text.contains("output token limit"),
            "should explain truncation cause"
        );
    }

    #[test]
    fn continuation_message_asks_to_continue() {
        let msg = continuation_message();
        let text = msg.text();
        assert!(
            text.contains("Continue"),
            "should instruct model to continue"
        );
    }

    #[test]
    fn continuation_message_is_deterministic() {
        let msg1 = continuation_message();
        let msg2 = continuation_message();
        assert_eq!(msg1.text(), msg2.text());
        assert_eq!(msg1.visibility, msg2.visibility);
        assert_eq!(msg1.role, msg2.role);
    }
}
