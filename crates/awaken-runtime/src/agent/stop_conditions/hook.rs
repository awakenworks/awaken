use std::sync::Arc;

use async_trait::async_trait;

use crate::runtime::{PhaseContext, PhaseHook};
use crate::state::StateCommand;
use awaken_contract::StateError;
use awaken_contract::contract::lifecycle::TerminationReason;

use super::super::state::{RunLifecycle, RunLifecycleUpdate};
use super::policy::{StopDecision, StopPolicy, StopPolicyStats};
use super::state::{StopConditionStatsKey, StopConditionStatsState};

/// Internal hook that builds stats from state and evaluates all policies.
pub(super) struct StopConditionHook {
    pub(super) policies: Vec<Arc<dyn StopPolicy>>,
}

impl StopConditionHook {
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn build_stats(&self, ctx: &PhaseContext) -> (StopConditionStatsState, StopPolicyStats) {
        let now = Self::now_ms();
        let mut state = ctx
            .state::<StopConditionStatsKey>()
            .cloned()
            .unwrap_or_default();
        if state.start_time_ms == 0 {
            state.start_time_ms = now;
        }

        // This hook runs once per AfterInference boundary.
        state.step_count = state.step_count.saturating_add(1);

        let mut last_tool_names = Vec::new();
        let mut last_response_text = String::new();
        let mut is_error = false;

        if let Some(ref response) = ctx.llm_response {
            match &response.outcome {
                Ok(stream_result) => {
                    let input = stream_result
                        .usage
                        .as_ref()
                        .and_then(|u| u.prompt_tokens)
                        .unwrap_or(0) as u64;
                    let output = stream_result
                        .usage
                        .as_ref()
                        .and_then(|u| u.completion_tokens)
                        .unwrap_or(0) as u64;
                    state.total_input_tokens = state.total_input_tokens.saturating_add(input);
                    state.total_output_tokens = state.total_output_tokens.saturating_add(output);

                    last_response_text = stream_result.text();
                    last_tool_names = stream_result
                        .tool_calls
                        .iter()
                        .map(|tc| tc.name.clone())
                        .collect();

                    // Successful inference resets consecutive errors
                    state.consecutive_errors = 0;
                }
                Err(_) => {
                    is_error = true;
                    state.consecutive_errors = state.consecutive_errors.saturating_add(1);
                }
            }
        }

        let elapsed_ms = now.saturating_sub(state.start_time_ms);
        let consecutive_errors = if is_error {
            state.consecutive_errors
        } else {
            0
        };

        (
            state.clone(),
            StopPolicyStats {
                step_count: state.step_count,
                total_input_tokens: state.total_input_tokens,
                total_output_tokens: state.total_output_tokens,
                elapsed_ms,
                consecutive_errors,
                last_tool_names,
                last_response_text,
            },
        )
    }
}

#[async_trait]
impl PhaseHook for StopConditionHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let (next_state, stats) = self.build_stats(ctx);
        let mut cmd = StateCommand::new();
        cmd.update::<StopConditionStatsKey>(next_state);

        for policy in &self.policies {
            if let StopDecision::Stop { code, detail } = policy.evaluate(&stats) {
                let reason = TerminationReason::stopped_with_detail(code, detail);
                let (_, done_reason) = reason.to_run_status();
                cmd.update::<RunLifecycle>(RunLifecycleUpdate::Done {
                    done_reason: done_reason.unwrap_or_default(),
                    updated_at: Self::now_ms(),
                });
                return Ok(cmd);
            }
        }

        Ok(cmd)
    }
}
