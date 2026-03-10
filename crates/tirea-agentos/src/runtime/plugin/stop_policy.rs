use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::composition::StopConditionSpec;
use crate::contracts::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::phase::{ActionSet, AfterInferenceAction};
use crate::contracts::runtime::state::AnyStateAction;
use crate::contracts::runtime::tool_call::ToolResult;
use crate::contracts::runtime::StreamResult;
use crate::contracts::thread::{Message, Role, ToolCall};
use crate::contracts::{RunContext, StoppedReason, TerminationReason};
use tirea_state::{GCounter, State};

pub const STOP_POLICY_PLUGIN_ID: &str = "stop_policy";

#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(
    path = "__kernel.stop_policy_runtime",
    action = "StopPolicyRuntimeAction",
    scope = "run"
)]
struct StopPolicyRuntimeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default)]
    #[tirea(lattice)]
    pub total_input_tokens: GCounter,
    #[serde(default)]
    #[tirea(lattice)]
    pub total_output_tokens: GCounter,
}

/// Action type for `StopPolicyRuntimeState` reducer.
#[derive(Serialize, Deserialize)]
pub(crate) enum StopPolicyRuntimeAction {
    /// Record token usage from a single inference call.
    RecordTokens {
        started_at_ms: Option<u64>,
        prompt_tokens: usize,
        completion_tokens: usize,
    },
}

impl StopPolicyRuntimeState {
    fn reduce(&mut self, action: StopPolicyRuntimeAction) {
        match action {
            StopPolicyRuntimeAction::RecordTokens {
                started_at_ms,
                prompt_tokens,
                completion_tokens,
            } => {
                if let Some(ms) = started_at_ms {
                    if self.started_at_ms.is_none() {
                        self.started_at_ms = Some(ms);
                    }
                }
                self.total_input_tokens.increment("_", prompt_tokens as u64);
                self.total_output_tokens
                    .increment("_", completion_tokens as u64);
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct MessageDerivedStopStats {
    step: usize,
    step_tool_call_count: usize,
    total_tool_call_count: usize,
    consecutive_errors: usize,
    last_tool_calls: Vec<ToolCall>,
    last_text: String,
    tool_call_history: VecDeque<Vec<String>>,
}

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

/// Stop-policy contract used by [`StopPolicyPlugin`].
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

/// Plugin adapter that evaluates configured stop policies at `AfterInference`.
///
/// This keeps stop-domain semantics out of the core loop.
pub struct StopPolicyPlugin {
    conditions: Vec<Arc<dyn StopPolicy>>,
}

impl std::fmt::Debug for StopPolicyPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopPolicyPlugin")
            .field("conditions_len", &self.conditions.len())
            .finish()
    }
}

impl StopPolicyPlugin {
    pub fn new(
        mut stop_conditions: Vec<Arc<dyn StopPolicy>>,
        stop_condition_specs: Vec<StopConditionSpec>,
    ) -> Self {
        stop_conditions.extend(stop_condition_specs.into_iter().map(condition_from_spec));
        Self {
            conditions: stop_conditions,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }
}

#[async_trait]
impl AgentBehavior for StopPolicyPlugin {
    fn id(&self) -> &str {
        STOP_POLICY_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(StopPolicyRuntimeState);

    async fn after_inference(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<AfterInferenceAction> {
        if self.conditions.is_empty() {
            return ActionSet::empty();
        }

        let Some(response) = ctx.response() else {
            return ActionSet::empty();
        };
        let now_ms = now_millis();
        let prompt_tokens = response
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_tokens)
            .unwrap_or(0) as usize;
        let completion_tokens = response
            .usage
            .as_ref()
            .and_then(|usage| usage.completion_tokens)
            .unwrap_or(0) as usize;

        let runtime = ctx
            .snapshot_of::<StopPolicyRuntimeState>()
            .unwrap_or_default();
        let started_at_ms = runtime.started_at_ms.unwrap_or(now_ms);
        let total_input_tokens =
            (runtime.total_input_tokens.value() as usize).saturating_add(prompt_tokens);
        let total_output_tokens =
            (runtime.total_output_tokens.value() as usize).saturating_add(completion_tokens);

        let mut actions: ActionSet<AfterInferenceAction> = ActionSet::empty();

        // Emit state patch for token recording
        actions = actions.and(AfterInferenceAction::State(AnyStateAction::new::<
            StopPolicyRuntimeState,
        >(
            StopPolicyRuntimeAction::RecordTokens {
                started_at_ms: if runtime.started_at_ms.is_none() {
                    Some(now_ms)
                } else {
                    None
                },
                prompt_tokens,
                completion_tokens,
            },
        )));

        // Only count messages from the current run to avoid cross-run accumulation.
        let run_messages = &ctx.messages()[ctx.initial_message_count()..];
        let message_stats = derive_stats_from_messages_with_response(run_messages, response);
        let elapsed = std::time::Duration::from_millis(now_ms.saturating_sub(started_at_ms));

        let run_ctx = RunContext::new(
            ctx.thread_id().to_string(),
            ctx.snapshot(),
            ctx.messages().to_vec(),
            ctx.run_config().clone(),
        );
        let input = StopPolicyInput {
            run_ctx: &run_ctx,
            stats: StopPolicyStats {
                step: message_stats.step,
                step_tool_call_count: message_stats.step_tool_call_count,
                total_tool_call_count: message_stats.total_tool_call_count,
                total_input_tokens,
                total_output_tokens,
                consecutive_errors: message_stats.consecutive_errors,
                elapsed,
                last_tool_calls: &message_stats.last_tool_calls,
                last_text: &message_stats.last_text,
                tool_call_history: &message_stats.tool_call_history,
            },
        };
        for condition in &self.conditions {
            if let Some(stopped) = condition.evaluate(&input) {
                actions = actions.and(AfterInferenceAction::Terminate(TerminationReason::Stopped(
                    stopped,
                )));
                break;
            }
        }
        actions
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn derive_stats_from_messages(messages: &[Arc<Message>]) -> MessageDerivedStopStats {
    let mut assistant_indices = Vec::new();
    for (idx, message) in messages.iter().enumerate() {
        if message.role == Role::Assistant {
            assistant_indices.push(idx);
        }
    }

    let mut stats = MessageDerivedStopStats {
        step: assistant_indices.len(),
        ..MessageDerivedStopStats::default()
    };
    let mut consecutive_errors = 0usize;

    for (round_idx, &assistant_idx) in assistant_indices.iter().enumerate() {
        let assistant = &messages[assistant_idx];
        let tool_calls = assistant.tool_calls.clone().unwrap_or_default();

        if !tool_calls.is_empty() {
            stats.total_tool_call_count =
                stats.total_tool_call_count.saturating_add(tool_calls.len());
            let mut names: Vec<String> = tool_calls.iter().map(|tc| tc.name.clone()).collect();
            names.sort();
            if stats.tool_call_history.len() >= 20 {
                stats.tool_call_history.pop_front();
            }
            stats.tool_call_history.push_back(names);
        }

        if round_idx + 1 == assistant_indices.len() {
            stats.step_tool_call_count = tool_calls.len();
            stats.last_tool_calls = tool_calls.clone();
            stats.last_text = assistant.content.clone();
        }

        if tool_calls.is_empty() {
            consecutive_errors = 0;
            continue;
        }

        let next_assistant_idx = assistant_indices
            .get(round_idx + 1)
            .copied()
            .unwrap_or(messages.len());
        let tool_results =
            collect_round_tool_results(messages, assistant_idx + 1, next_assistant_idx);
        let round_all_errors = tool_calls
            .iter()
            .all(|call| tool_results.get(&call.id).copied().unwrap_or(false));
        if round_all_errors {
            consecutive_errors = consecutive_errors.saturating_add(1);
        } else {
            consecutive_errors = 0;
        }
    }

    stats.consecutive_errors = consecutive_errors;
    stats
}

fn derive_stats_from_messages_with_response(
    messages: &[Arc<Message>],
    response: &StreamResult,
) -> MessageDerivedStopStats {
    let mut all_messages = Vec::with_capacity(messages.len() + 1);
    all_messages.extend(messages.iter().cloned());
    all_messages.push(Arc::new(Message::assistant_with_tool_calls(
        response.text.clone(),
        response.tool_calls.clone(),
    )));
    derive_stats_from_messages(&all_messages)
}

fn collect_round_tool_results(
    messages: &[Arc<Message>],
    from: usize,
    to: usize,
) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for message in messages.iter().take(to).skip(from) {
        if message.role != Role::Tool {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_ref() else {
            continue;
        };
        let is_error = serde_json::from_str::<ToolResult>(&message.content)
            .map(|result| result.is_error())
            .unwrap_or(false);
        out.insert(call_id.clone(), is_error);
    }
    out
}

fn condition_from_spec(spec: StopConditionSpec) -> Arc<dyn StopPolicy> {
    match spec {
        StopConditionSpec::MaxRounds { rounds } => Arc::new(MaxRounds(rounds)),
        StopConditionSpec::Timeout { seconds } => {
            Arc::new(Timeout(std::time::Duration::from_secs(seconds)))
        }
        StopConditionSpec::TokenBudget { max_total } => Arc::new(TokenBudget { max_total }),
        StopConditionSpec::ConsecutiveErrors { max } => Arc::new(ConsecutiveErrors(max)),
        StopConditionSpec::StopOnTool { tool_name } => Arc::new(StopOnTool(tool_name)),
        StopConditionSpec::ContentMatch { pattern } => Arc::new(ContentMatch(pattern)),
        StopConditionSpec::LoopDetection { window } => Arc::new(LoopDetection { window }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::thread::Message;
    use crate::contracts::StreamResult;
    use serde_json::json;
    use tirea_state::LatticeRegistry;

    #[test]
    fn derives_round_stats_from_messages() {
        let call_1 = ToolCall::new("c1", "failing", json!({}));
        let call_2 = ToolCall::new("c2", "echo", json!({}));
        let messages = vec![
            Arc::new(Message::assistant_with_tool_calls(
                "r1",
                vec![call_1.clone()],
            )),
            Arc::new(Message::tool(
                "c1",
                serde_json::to_string(&ToolResult::error("failing", "boom")).unwrap(),
            )),
            Arc::new(Message::assistant_with_tool_calls(
                "r2",
                vec![call_2.clone()],
            )),
            Arc::new(Message::tool(
                "c2",
                serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
            )),
        ];
        let stats = derive_stats_from_messages(&messages);
        assert_eq!(stats.step, 2);
        assert_eq!(stats.total_tool_call_count, 2);
        assert_eq!(stats.step_tool_call_count, 1);
        assert_eq!(stats.last_tool_calls.len(), 1);
        assert_eq!(stats.last_tool_calls[0].id, call_2.id);
        assert_eq!(stats.last_tool_calls[0].name, call_2.name);
        assert_eq!(stats.last_text, "r2");
        assert_eq!(stats.consecutive_errors, 0);
    }

    #[test]
    fn derives_stats_with_current_response() {
        let prior_messages = vec![Arc::new(Message::user("u1"))];
        let response = StreamResult {
            text: "r1".to_string(),
            tool_calls: vec![ToolCall::new("c1", "echo", json!({}))],
            usage: None,
            stop_reason: None,
        };
        let stats = derive_stats_from_messages_with_response(&prior_messages, &response);
        assert_eq!(stats.step, 1);
        assert_eq!(stats.step_tool_call_count, 1);
        assert_eq!(stats.total_tool_call_count, 1);
        assert_eq!(stats.last_text, "r1");
        assert_eq!(stats.last_tool_calls.len(), 1);
        assert_eq!(stats.last_tool_calls[0].id, "c1");
    }

    /// Empty run (no prior messages, text-only response with no tools).
    #[test]
    fn stats_text_only_response_counts_step_but_no_tools() {
        let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("hello"))];
        let response = StreamResult {
            text: "hi there".to_string(),
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
        };
        let stats = derive_stats_from_messages_with_response(&messages, &response);
        assert_eq!(stats.step, 1, "text-only response still counts as a step");
        assert_eq!(stats.step_tool_call_count, 0);
        assert_eq!(stats.total_tool_call_count, 0);
        assert_eq!(stats.last_text, "hi there");
        assert!(stats.tool_call_history.is_empty());
    }

    /// No messages at all — response is the only thing.
    #[test]
    fn stats_empty_messages_with_response() {
        let messages: Vec<Arc<Message>> = vec![];
        let response = StreamResult {
            text: "hi".to_string(),
            tool_calls: vec![],
            usage: None,
            stop_reason: None,
        };
        let stats = derive_stats_from_messages_with_response(&messages, &response);
        assert_eq!(stats.step, 1);
        assert_eq!(stats.total_tool_call_count, 0);
        assert_eq!(stats.consecutive_errors, 0);
    }

    /// Consecutive errors reset to 0 when a round has no tool calls.
    #[test]
    fn consecutive_errors_reset_on_no_tool_round() {
        let fail_call = ToolCall::new("f1", "broken", json!({}));
        let messages = vec![
            // Round 1: tool fails
            Arc::new(Message::assistant_with_tool_calls(
                "r1",
                vec![fail_call.clone()],
            )),
            Arc::new(Message::tool(
                &fail_call.id,
                serde_json::to_string(&ToolResult::error("broken", "boom")).unwrap(),
            )),
            // Round 2: text-only (no tools) — resets consecutive errors
            Arc::new(Message::assistant("text only")),
        ];
        let stats = derive_stats_from_messages(&messages);
        assert_eq!(
            stats.consecutive_errors, 0,
            "text-only round resets consecutive errors"
        );
        assert_eq!(stats.step, 2);
    }

    /// tool_call_history caps at 20 entries (sliding window).
    #[test]
    fn tool_call_history_caps_at_twenty() {
        let mut messages: Vec<Arc<Message>> = Vec::new();
        for i in 0..25 {
            let call = ToolCall::new(&format!("c{i}"), &format!("tool_{i}"), json!({}));
            messages.push(Arc::new(Message::assistant_with_tool_calls(
                format!("r{i}"),
                vec![call.clone()],
            )));
            messages.push(Arc::new(Message::tool(
                &call.id,
                serde_json::to_string(&ToolResult::success(
                    &format!("tool_{i}"),
                    json!({"ok": true}),
                ))
                .unwrap(),
            )));
        }
        let stats = derive_stats_from_messages(&messages);
        assert_eq!(stats.step, 25);
        assert_eq!(
            stats.tool_call_history.len(),
            20,
            "history must be capped at 20"
        );
        // Oldest entries are dropped — first entry should be tool_5 (index 5).
        assert_eq!(stats.tool_call_history[0], vec!["tool_5".to_string()]);
    }

    /// Multiple tool calls in one round are sorted and tracked together.
    #[test]
    fn multi_tool_round_sorted_in_history() {
        let calls = vec![
            ToolCall::new("c1", "zebra", json!({})),
            ToolCall::new("c2", "alpha", json!({})),
        ];
        let messages = vec![
            Arc::new(Message::assistant_with_tool_calls("r1", calls.clone())),
            Arc::new(Message::tool(
                "c1",
                serde_json::to_string(&ToolResult::success("zebra", json!({}))).unwrap(),
            )),
            Arc::new(Message::tool(
                "c2",
                serde_json::to_string(&ToolResult::success("alpha", json!({}))).unwrap(),
            )),
        ];
        let stats = derive_stats_from_messages(&messages);
        assert_eq!(stats.step, 1);
        assert_eq!(stats.step_tool_call_count, 2);
        assert_eq!(stats.total_tool_call_count, 2);
        assert_eq!(
            stats.tool_call_history[0],
            vec!["alpha".to_string(), "zebra".to_string()],
            "tool names must be sorted within a round"
        );
    }

    /// Partial tool failure: only when ALL tools fail does consecutive_errors increment.
    #[test]
    fn consecutive_errors_only_on_all_tools_failing() {
        let ok_call = ToolCall::new("c1", "ok_tool", json!({}));
        let fail_call = ToolCall::new("c2", "bad_tool", json!({}));
        let messages = vec![
            Arc::new(Message::assistant_with_tool_calls(
                "r1",
                vec![ok_call.clone(), fail_call.clone()],
            )),
            Arc::new(Message::tool(
                &ok_call.id,
                serde_json::to_string(&ToolResult::success("ok_tool", json!({}))).unwrap(),
            )),
            Arc::new(Message::tool(
                &fail_call.id,
                serde_json::to_string(&ToolResult::error("bad_tool", "fail")).unwrap(),
            )),
        ];
        let stats = derive_stats_from_messages(&messages);
        assert_eq!(
            stats.consecutive_errors, 0,
            "mixed success/fail does not count as all-error"
        );
    }

    #[test]
    fn stop_condition_spec_serialization_roundtrip() {
        let specs = vec![
            StopConditionSpec::MaxRounds { rounds: 5 },
            StopConditionSpec::Timeout { seconds: 30 },
            StopConditionSpec::TokenBudget { max_total: 1000 },
            StopConditionSpec::ConsecutiveErrors { max: 3 },
            StopConditionSpec::StopOnTool {
                tool_name: "finish".to_string(),
            },
            StopConditionSpec::ContentMatch {
                pattern: "DONE".to_string(),
            },
            StopConditionSpec::LoopDetection { window: 4 },
        ];
        for spec in specs {
            let encoded = serde_json::to_string(&spec).unwrap();
            let restored: StopConditionSpec = serde_json::from_str(&encoded).unwrap();
            assert_eq!(restored, spec);
        }
    }

    #[test]
    fn stop_policy_registers_lattice_paths() {
        let mut registry = LatticeRegistry::new();
        let plugin = StopPolicyPlugin::new(vec![], vec![]);
        plugin.register_lattice_paths(&mut registry);
        assert!(
            registry
                .get(&tirea_state::parse_path(
                    "__kernel.stop_policy_runtime.total_input_tokens"
                ))
                .is_some(),
            "total_input_tokens should be registered"
        );
        assert!(
            registry
                .get(&tirea_state::parse_path(
                    "__kernel.stop_policy_runtime.total_output_tokens"
                ))
                .is_some(),
            "total_output_tokens should be registered"
        );
    }

    #[test]
    fn record_tokens_increments_gcounters() {
        let mut state = StopPolicyRuntimeState::default();
        assert_eq!(state.total_input_tokens.value(), 0);
        assert_eq!(state.total_output_tokens.value(), 0);

        state.reduce(StopPolicyRuntimeAction::RecordTokens {
            started_at_ms: Some(1000),
            prompt_tokens: 100,
            completion_tokens: 50,
        });
        assert_eq!(state.total_input_tokens.value(), 100);
        assert_eq!(state.total_output_tokens.value(), 50);
        assert_eq!(state.started_at_ms, Some(1000));

        state.reduce(StopPolicyRuntimeAction::RecordTokens {
            started_at_ms: Some(2000),
            prompt_tokens: 200,
            completion_tokens: 150,
        });
        assert_eq!(state.total_input_tokens.value(), 300);
        assert_eq!(state.total_output_tokens.value(), 200);
        assert_eq!(
            state.started_at_ms,
            Some(1000),
            "started_at_ms should not change once set"
        );
    }

    /// Regression: `derive_stats_from_messages` used to count assistant messages
    /// from the entire thread history, not just the current run. When a thread is
    /// reused across multiple runs, the step counter accumulated across runs,
    /// causing MaxRounds to fire prematurely on subsequent runs.
    #[test]
    fn stats_should_only_count_current_run_messages() {
        // Simulate a thread with 5 prior assistant turns from previous runs.
        let mut prior_messages: Vec<Arc<Message>> = Vec::new();
        for i in 0..5 {
            let call = ToolCall::new(&format!("old-{i}"), "echo", json!({}));
            prior_messages.push(Arc::new(Message::user(format!("u{i}"))));
            prior_messages.push(Arc::new(Message::assistant_with_tool_calls(
                format!("prior-{i}"),
                vec![call.clone()],
            )));
            prior_messages.push(Arc::new(Message::tool(
                &call.id,
                serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
            )));
        }

        // Current run adds a new user message.
        let run_start = prior_messages.len();
        prior_messages.push(Arc::new(Message::user("new-user-message")));

        // Current LLM response — this is the 1st assistant turn of the NEW run.
        let response = StreamResult {
            text: "new-response".to_string(),
            tool_calls: vec![ToolCall::new("new-1", "echo", json!({}))],
            usage: None,
            stop_reason: None,
        };

        // Only count messages from run_start onward.
        let stats =
            derive_stats_from_messages_with_response(&prior_messages[run_start..], &response);
        assert_eq!(
            stats.step, 1,
            "step must be 1 (only the current run's assistant turn), not 6"
        );
        assert_eq!(stats.total_tool_call_count, 1);
        assert_eq!(stats.consecutive_errors, 0);
    }

    /// Prior run's consecutive tool errors must not carry over into a new run.
    #[test]
    fn consecutive_errors_do_not_leak_across_runs() {
        // Prior run: 3 rounds all failing.
        let mut messages: Vec<Arc<Message>> = Vec::new();
        for i in 0..3 {
            let call = ToolCall::new(&format!("fail-{i}"), "broken", json!({}));
            messages.push(Arc::new(Message::user(format!("u{i}"))));
            messages.push(Arc::new(Message::assistant_with_tool_calls(
                format!("a{i}"),
                vec![call.clone()],
            )));
            messages.push(Arc::new(Message::tool(
                &call.id,
                serde_json::to_string(&ToolResult::error("broken", "boom")).unwrap(),
            )));
        }

        // New run boundary.
        let run_start = messages.len();
        messages.push(Arc::new(Message::user("new-turn")));

        // New run: first response succeeds.
        let response = StreamResult {
            text: "ok".to_string(),
            tool_calls: vec![ToolCall::new("ok-1", "echo", json!({}))],
            usage: None,
            stop_reason: None,
        };

        let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
        assert_eq!(
            stats.consecutive_errors, 0,
            "errors from prior run must not leak"
        );
        assert_eq!(stats.step, 1);
    }

    /// Prior run's tool call history must not trigger loop detection in a new run.
    #[test]
    fn tool_call_history_does_not_leak_across_runs() {
        // Prior run: 3 rounds calling the same tool "echo".
        let mut messages: Vec<Arc<Message>> = Vec::new();
        for i in 0..3 {
            let call = ToolCall::new(&format!("old-{i}"), "echo", json!({}));
            messages.push(Arc::new(Message::user(format!("u{i}"))));
            messages.push(Arc::new(Message::assistant_with_tool_calls(
                format!("a{i}"),
                vec![call.clone()],
            )));
            messages.push(Arc::new(Message::tool(
                &call.id,
                serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
            )));
        }

        let run_start = messages.len();
        messages.push(Arc::new(Message::user("new-turn")));

        // New run: first response also calls "echo" — should NOT trigger loop.
        let response = StreamResult {
            text: "".to_string(),
            tool_calls: vec![ToolCall::new("new-1", "echo", json!({}))],
            usage: None,
            stop_reason: None,
        };

        let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
        assert_eq!(
            stats.tool_call_history.len(),
            1,
            "only 1 round in the new run"
        );
    }

    /// Prior run's total_tool_call_count must not accumulate into the new run.
    #[test]
    fn total_tool_call_count_does_not_leak_across_runs() {
        let mut messages: Vec<Arc<Message>> = Vec::new();
        for i in 0..4 {
            let call = ToolCall::new(&format!("old-{i}"), "echo", json!({}));
            messages.push(Arc::new(Message::user(format!("u{i}"))));
            messages.push(Arc::new(Message::assistant_with_tool_calls(
                format!("a{i}"),
                vec![call.clone()],
            )));
            messages.push(Arc::new(Message::tool(
                &call.id,
                serde_json::to_string(&ToolResult::success("echo", json!({"ok": true}))).unwrap(),
            )));
        }

        let run_start = messages.len();
        messages.push(Arc::new(Message::user("new-turn")));

        let response = StreamResult {
            text: "".to_string(),
            tool_calls: vec![
                ToolCall::new("n1", "a", json!({})),
                ToolCall::new("n2", "b", json!({})),
            ],
            usage: None,
            stop_reason: None,
        };

        let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
        assert_eq!(
            stats.total_tool_call_count, 2,
            "only 2 tool calls in the new run, not 6"
        );
    }

    /// Fresh thread (run_start=0): all messages belong to the current run.
    #[test]
    fn fresh_thread_counts_all_messages() {
        let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("hello"))];
        let run_start = 0;

        let response = StreamResult {
            text: "hi".to_string(),
            tool_calls: vec![ToolCall::new("c1", "echo", json!({}))],
            usage: None,
            stop_reason: None,
        };

        let stats = derive_stats_from_messages_with_response(&messages[run_start..], &response);
        assert_eq!(stats.step, 1);
        assert_eq!(stats.total_tool_call_count, 1);
    }

    // -----------------------------------------------------------------------
    // Built-in policy evaluate tests
    // -----------------------------------------------------------------------

    /// Construct a `StopPolicyInput` with defaults and evaluate the given policy.
    /// Override fields via assignment after construction to avoid duplicate-field errors.
    macro_rules! eval_policy {
        ($policy:expr, { $($field:ident : $val:expr),* $(,)? }) => {{
            let run_ctx = RunContext::new(
                "t", json!({}), vec![], crate::contracts::RunConfig::default(),
            );
            let empty_history = VecDeque::new();
            let no_tools: &[ToolCall] = &[];
            #[allow(unused_mut, unused_assignments)]
            let mut stats = StopPolicyStats {
                step: 0,
                step_tool_call_count: 0,
                total_tool_call_count: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                consecutive_errors: 0,
                elapsed: std::time::Duration::ZERO,
                last_tool_calls: no_tools,
                last_text: "",
                tool_call_history: &empty_history,
            };
            $(stats.$field = $val;)*
            let input = StopPolicyInput { run_ctx: &run_ctx, stats };
            $policy.evaluate(&input)
        }};
    }

    #[test]
    fn max_rounds_does_not_fire_below_limit() {
        assert!(eval_policy!(MaxRounds(5), { step: 4 }).is_none());
    }

    #[test]
    fn max_rounds_fires_at_limit() {
        let r = eval_policy!(MaxRounds(5), { step: 5 }).unwrap();
        assert_eq!(r.code, "max_rounds_reached");
    }

    #[test]
    fn max_rounds_fires_above_limit() {
        assert!(eval_policy!(MaxRounds(5), { step: 10 }).is_some());
    }

    #[test]
    fn max_rounds_step_zero_does_not_fire() {
        assert!(eval_policy!(MaxRounds(5), {}).is_none());
    }

    #[test]
    fn consecutive_errors_fires_at_threshold() {
        let r = eval_policy!(ConsecutiveErrors(3), { consecutive_errors: 3 }).unwrap();
        assert_eq!(r.code, "consecutive_errors_exceeded");
    }

    #[test]
    fn consecutive_errors_does_not_fire_below_threshold() {
        assert!(eval_policy!(ConsecutiveErrors(3), { consecutive_errors: 2 }).is_none());
    }

    #[test]
    fn consecutive_errors_zero_max_never_fires() {
        assert!(
            eval_policy!(ConsecutiveErrors(0), { consecutive_errors: 100 }).is_none(),
            "max=0 means disabled"
        );
    }

    #[test]
    fn loop_detection_fires_on_repeated_tool_pattern() {
        let mut h = VecDeque::new();
        h.push_back(vec!["read".into(), "write".into()]);
        h.push_back(vec!["read".into(), "write".into()]);
        let r = eval_policy!(LoopDetection { window: 3 }, { tool_call_history: &h }).unwrap();
        assert_eq!(r.code, "loop_detected");
    }

    #[test]
    fn loop_detection_does_not_fire_on_different_patterns() {
        let mut h = VecDeque::new();
        h.push_back(vec!["read".into()]);
        h.push_back(vec!["write".into()]);
        assert!(eval_policy!(LoopDetection { window: 3 }, { tool_call_history: &h }).is_none());
    }

    #[test]
    fn loop_detection_needs_at_least_two_rounds() {
        let mut h = VecDeque::new();
        h.push_back(vec!["read".into()]);
        assert!(
            eval_policy!(LoopDetection { window: 2 }, { tool_call_history: &h }).is_none(),
            "single round cannot form a loop"
        );
    }

    #[test]
    fn loop_detection_window_clamps_to_minimum_two() {
        let mut h = VecDeque::new();
        h.push_back(vec!["x".into()]);
        h.push_back(vec!["x".into()]);
        assert!(
            eval_policy!(LoopDetection { window: 1 }, { tool_call_history: &h }).is_some(),
            "window clamped to 2 still detects the pair"
        );
    }

    #[test]
    fn content_match_empty_pattern_never_fires() {
        assert!(
            eval_policy!(ContentMatch(String::new()), { last_text: "anything here" }).is_none(),
            "empty pattern must not match"
        );
    }

    #[test]
    fn content_match_fires_on_substring() {
        let r =
            eval_policy!(ContentMatch("DONE".into()), { last_text: "work is DONE now" }).unwrap();
        assert_eq!(r.code, "content_matched");
        assert_eq!(r.detail.as_deref(), Some("DONE"));
    }

    #[test]
    fn content_match_no_match() {
        assert!(
            eval_policy!(ContentMatch("DONE".into()), { last_text: "still working" }).is_none()
        );
    }

    #[test]
    fn token_budget_zero_never_fires() {
        assert!(
            eval_policy!(TokenBudget { max_total: 0 }, {
                total_input_tokens: 999_999, total_output_tokens: 999_999
            })
            .is_none(),
            "max_total=0 means unlimited"
        );
    }

    #[test]
    fn token_budget_fires_at_limit() {
        let r = eval_policy!(TokenBudget { max_total: 1000 }, {
            total_input_tokens: 600, total_output_tokens: 400
        })
        .unwrap();
        assert_eq!(r.code, "token_budget_exceeded");
    }

    #[test]
    fn token_budget_does_not_fire_below_limit() {
        assert!(eval_policy!(TokenBudget { max_total: 1000 }, {
            total_input_tokens: 400, total_output_tokens: 500
        })
        .is_none());
    }

    #[test]
    fn stop_on_tool_no_match() {
        let calls = [ToolCall::new("c1", "echo", json!({}))];
        assert!(eval_policy!(StopOnTool("finish".into()), { last_tool_calls: &calls }).is_none());
    }

    #[test]
    fn stopped_reason_payload() {
        let calls = [ToolCall::new("c1", "finish", json!({}))];
        let r = eval_policy!(StopOnTool("finish".into()), {
            step: 1,
            step_tool_call_count: 1,
            total_tool_call_count: 1,
            last_tool_calls: &calls
        })
        .unwrap();
        assert_eq!(r.code, "tool_called");
        assert_eq!(r.detail.as_deref(), Some("finish"));
    }

    // -----------------------------------------------------------------------
    // condition_from_spec factory
    // -----------------------------------------------------------------------

    #[test]
    fn condition_from_spec_max_rounds() {
        let c = condition_from_spec(StopConditionSpec::MaxRounds { rounds: 7 });
        assert_eq!(c.id(), "max_rounds");
        assert!(eval_policy!(&*c, { step: 7 }).is_some());
    }

    #[test]
    fn condition_from_spec_timeout() {
        let c = condition_from_spec(StopConditionSpec::Timeout { seconds: 10 });
        assert_eq!(c.id(), "timeout");
        assert!(eval_policy!(&*c, { elapsed: std::time::Duration::from_secs(10) }).is_some());
    }

    #[test]
    fn condition_from_spec_token_budget() {
        let c = condition_from_spec(StopConditionSpec::TokenBudget { max_total: 500 });
        assert_eq!(c.id(), "token_budget");
        assert!(eval_policy!(&*c, { total_input_tokens: 300, total_output_tokens: 200 }).is_some());
    }

    #[test]
    fn condition_from_spec_consecutive_errors() {
        let c = condition_from_spec(StopConditionSpec::ConsecutiveErrors { max: 2 });
        assert_eq!(c.id(), "consecutive_errors");
        assert!(eval_policy!(&*c, { consecutive_errors: 2 }).is_some());
    }

    #[test]
    fn condition_from_spec_stop_on_tool() {
        let c = condition_from_spec(StopConditionSpec::StopOnTool {
            tool_name: "done".into(),
        });
        assert_eq!(c.id(), "stop_on_tool");
        let calls = [ToolCall::new("c1", "done", json!({}))];
        assert!(eval_policy!(&*c, { last_tool_calls: &calls }).is_some());
    }

    #[test]
    fn condition_from_spec_content_match() {
        let c = condition_from_spec(StopConditionSpec::ContentMatch {
            pattern: "END".into(),
        });
        assert_eq!(c.id(), "content_match");
        assert!(eval_policy!(&*c, { last_text: "THE END" }).is_some());
    }

    #[test]
    fn condition_from_spec_loop_detection() {
        let c = condition_from_spec(StopConditionSpec::LoopDetection { window: 3 });
        assert_eq!(c.id(), "loop_detection");
        let mut h = VecDeque::new();
        h.push_back(vec!["x".to_string()]);
        h.push_back(vec!["x".to_string()]);
        assert!(eval_policy!(&*c, { tool_call_history: &h }).is_some());
    }
}
