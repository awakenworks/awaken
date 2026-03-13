use std::collections::VecDeque;

use crate::contracts::thread::ToolCall;
use crate::contracts::{RunContext, StoppedReason};

/// Aggregated runtime stats consumed by stop policies.
pub struct StopPolicyStats<'a> {
    /// Number of completed steps.
    pub step: usize,
    /// Tool calls emitted by the current step.
    pub step_tool_call_count: usize,
    /// Total tool calls across the whole run.
    pub total_tool_call_count: usize,
    /// Cumulative input tokens across all LLM calls.
    pub total_input_tokens: usize,
    /// Cumulative output tokens across all LLM calls.
    pub total_output_tokens: usize,
    /// Number of consecutive rounds where all tools failed.
    pub consecutive_errors: usize,
    /// Time elapsed since the loop started.
    pub elapsed: std::time::Duration,
    /// Tool calls from the most recent LLM response.
    pub last_tool_calls: &'a [ToolCall],
    /// Text from the most recent LLM response.
    pub last_text: &'a str,
    /// History of tool call names per round (most recent last), for loop detection.
    pub tool_call_history: &'a VecDeque<Vec<String>>,
}

/// Canonical stop-policy input.
pub struct StopPolicyInput<'a> {
    /// Current run context.
    pub run_ctx: &'a RunContext,
    /// Runtime stats.
    pub stats: StopPolicyStats<'a>,
}

/// Stop-policy contract used by [`super::StopPolicyPlugin`].
pub trait StopPolicy: Send + Sync {
    /// Stable policy id.
    fn id(&self) -> &str;

    /// Evaluate stop decision. Return `Some(StoppedReason)` to terminate.
    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason>;
}

// ---------------------------------------------------------------------------
// Built-in stop conditions
// ---------------------------------------------------------------------------

/// Stop after a fixed number of tool-call rounds.
pub struct MaxRounds(pub usize);

impl StopPolicy for MaxRounds {
    fn id(&self) -> &str {
        "max_rounds"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        if input.stats.step >= self.0 {
            Some(StoppedReason::new("max_rounds_reached"))
        } else {
            None
        }
    }
}

/// Stop after a wall-clock duration elapses.
pub struct Timeout(pub std::time::Duration);

impl StopPolicy for Timeout {
    fn id(&self) -> &str {
        "timeout"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        if input.stats.elapsed >= self.0 {
            Some(StoppedReason::new("timeout_reached"))
        } else {
            None
        }
    }
}

/// Stop when cumulative token usage exceeds a budget.
pub struct TokenBudget {
    /// Maximum total tokens (input + output). 0 = unlimited.
    pub max_total: usize,
}

impl StopPolicy for TokenBudget {
    fn id(&self) -> &str {
        "token_budget"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        if self.max_total > 0
            && (input.stats.total_input_tokens + input.stats.total_output_tokens) >= self.max_total
        {
            Some(StoppedReason::new("token_budget_exceeded"))
        } else {
            None
        }
    }
}

/// Stop after N consecutive rounds where all tool executions failed.
pub struct ConsecutiveErrors(pub usize);

impl StopPolicy for ConsecutiveErrors {
    fn id(&self) -> &str {
        "consecutive_errors"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        if self.0 > 0 && input.stats.consecutive_errors >= self.0 {
            Some(StoppedReason::new("consecutive_errors_exceeded"))
        } else {
            None
        }
    }
}

/// Stop when a specific tool is called by the LLM.
pub struct StopOnTool(pub String);

impl StopPolicy for StopOnTool {
    fn id(&self) -> &str {
        "stop_on_tool"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        for call in input.stats.last_tool_calls {
            if call.name == self.0 {
                return Some(StoppedReason::with_detail("tool_called", self.0.clone()));
            }
        }
        None
    }
}

/// Stop when LLM output text contains a literal pattern.
pub struct ContentMatch(pub String);

impl StopPolicy for ContentMatch {
    fn id(&self) -> &str {
        "content_match"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        if !self.0.is_empty() && input.stats.last_text.contains(&self.0) {
            Some(StoppedReason::with_detail(
                "content_matched",
                self.0.clone(),
            ))
        } else {
            None
        }
    }
}

/// Stop when the same tool call pattern repeats within a sliding window.
///
/// Compares the sorted tool names of the most recent round against previous
/// rounds within `window` size. If the same set appears twice consecutively,
/// the loop is considered stuck.
pub struct LoopDetection {
    /// Number of recent rounds to compare. Minimum 2.
    pub window: usize,
}

impl StopPolicy for LoopDetection {
    fn id(&self) -> &str {
        "loop_detection"
    }

    fn evaluate(&self, input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
        let window = self.window.max(2);
        let history = input.stats.tool_call_history;
        if history.len() < 2 {
            return None;
        }

        let recent: Vec<_> = history.iter().rev().take(window).collect();
        for pair in recent.windows(2) {
            if pair[0] == pair[1] {
                return Some(StoppedReason::new("loop_detected"));
            }
        }
        None
    }
}
