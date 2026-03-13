use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::composition::StopConditionSpec;
use crate::contracts::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::phase::{ActionSet, AfterInferenceAction};
use crate::contracts::runtime::state::AnyStateAction;
use crate::contracts::runtime::tool_call::ToolResult;
use crate::contracts::runtime::StreamResult;
use crate::contracts::thread::{Message, Role, ToolCall};
use crate::contracts::{RunContext, TerminationReason};

use super::conditions::{StopPolicy, StopPolicyInput, StopPolicyStats};
use super::state::{StopPolicyRuntimeAction, StopPolicyRuntimeState};
use super::{
    ConsecutiveErrors, ContentMatch, LoopDetection, MaxRounds, StopOnTool, Timeout, TokenBudget,
    STOP_POLICY_PLUGIN_ID,
};

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
            ctx.run_policy().clone(),
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

#[derive(Debug, Clone, Default)]
pub(super) struct MessageDerivedStopStats {
    pub(super) step: usize,
    pub(super) step_tool_call_count: usize,
    pub(super) total_tool_call_count: usize,
    pub(super) consecutive_errors: usize,
    pub(super) last_tool_calls: Vec<ToolCall>,
    pub(super) last_text: String,
    pub(super) tool_call_history: VecDeque<Vec<String>>,
}

pub(super) fn derive_stats_from_messages(messages: &[Arc<Message>]) -> MessageDerivedStopStats {
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

pub(super) fn derive_stats_from_messages_with_response(
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

pub(super) fn condition_from_spec(spec: StopConditionSpec) -> Arc<dyn StopPolicy> {
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
