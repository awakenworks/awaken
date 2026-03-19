use super::outcome::{LoopStats, LoopUsage};
use crate::contracts::runtime::StreamResult;
use crate::runtime::loop_runner::state_commit::RunTokenTotals;
use std::collections::VecDeque;
use std::time::Instant;

/// Internal state tracked across run steps for loop stats/cancellation flows.
pub(super) struct LoopRunState {
    pub(super) completed_steps: usize,
    pub(super) total_thinking_tokens: usize,
    pub(super) total_input_tokens: usize,
    pub(super) total_output_tokens: usize,
    pub(super) llm_calls: usize,
    pub(super) llm_retries: usize,
    pub(super) tool_calls: usize,
    pub(super) step_tool_call_count: usize,
    pub(super) tool_errors: usize,
    pub(super) consecutive_errors: usize,
    start_time: Instant,
    /// Tool call names per step (most recent last), capped at 20 entries.
    pub(super) tool_call_history: VecDeque<Vec<String>>,
    /// Number of truncation recovery retries consumed so far.
    pub(super) truncation_retries: usize,
    /// Number of mid-stream error recovery retries consumed so far.
    pub(super) stream_event_retries: usize,
}

impl LoopRunState {
    pub(super) fn new() -> Self {
        Self {
            completed_steps: 0,
            total_thinking_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            llm_calls: 0,
            llm_retries: 0,
            tool_calls: 0,
            step_tool_call_count: 0,
            tool_errors: 0,
            consecutive_errors: 0,
            start_time: Instant::now(),
            tool_call_history: VecDeque::new(),
            truncation_retries: 0,
            stream_event_retries: 0,
        }
    }
    pub(super) fn token_totals(&self) -> RunTokenTotals {
        RunTokenTotals {
            input_tokens: self.total_input_tokens as u64,
            output_tokens: self.total_output_tokens as u64,
        }
    }

    pub(super) fn record_llm_attempts(&mut self, attempts: usize) {
        if attempts == 0 {
            return;
        }
        self.llm_calls = self.llm_calls.saturating_add(attempts);
        self.llm_retries = self.llm_retries.saturating_add(attempts.saturating_sub(1));
    }

    pub(super) fn update_from_response(&mut self, result: &StreamResult) {
        if let Some(ref usage) = result.usage {
            self.total_thinking_tokens += usage.thinking_tokens.unwrap_or(0) as usize;
            self.total_input_tokens += usage.prompt_tokens.unwrap_or(0) as usize;
            self.total_output_tokens += usage.completion_tokens.unwrap_or(0) as usize;
        }
    }

    pub(super) fn record_tool_step(
        &mut self,
        tool_calls: &[crate::contracts::thread::ToolCall],
        error_count: usize,
    ) {
        self.step_tool_call_count = tool_calls.len();
        let mut names: Vec<String> = tool_calls.iter().map(|tc| tc.name.clone()).collect();
        names.sort();
        if self.tool_call_history.len() >= 20 {
            self.tool_call_history.pop_front();
        }
        self.tool_call_history.push_back(names);
        self.tool_calls = self.tool_calls.saturating_add(tool_calls.len());
        self.tool_errors = self.tool_errors.saturating_add(error_count);

        if error_count > 0 && error_count == tool_calls.len() {
            self.consecutive_errors += 1;
        } else {
            self.consecutive_errors = 0;
        }
    }

    pub(super) fn record_step_without_tools(&mut self) {
        self.step_tool_call_count = 0;
    }

    pub(super) fn usage(&self) -> LoopUsage {
        LoopUsage {
            prompt_tokens: self.total_input_tokens,
            completion_tokens: self.total_output_tokens,
            total_tokens: self.total_input_tokens + self.total_output_tokens,
            thinking_tokens: self.total_thinking_tokens,
        }
    }

    pub(super) fn stats(&self) -> LoopStats {
        LoopStats {
            duration_ms: self.start_time.elapsed().as_millis() as u64,
            steps: self.completed_steps,
            llm_calls: self.llm_calls,
            llm_retries: self.llm_retries,
            tool_calls: self.tool_calls,
            tool_errors: self.tool_errors,
        }
    }
}
