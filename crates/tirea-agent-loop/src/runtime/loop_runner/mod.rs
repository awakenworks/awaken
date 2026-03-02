//! Agent loop implementation with Phase-based plugin execution.
//!
//! The agent loop orchestrates the conversation between user, LLM, and tools:
//!
//! ```text
//! User Input → LLM → Tool Calls? → Execute Tools → LLM → ... → Final Response
//! ```
//!
//! # Phase Execution
//!
//! Each phase dispatches to its typed plugin hook:
//!
//! ```text
//! RunStart (once)
//!     │
//!     ▼
//! ┌─────────────────────────┐
//! │      StepStart          │ ← plugins can apply state patches
//! ├─────────────────────────┤
//! │    BeforeInference      │ ← plugins can inject prompt context, filter tools
//! ├─────────────────────────┤
//! │      [LLM CALL]         │
//! ├─────────────────────────┤
//! │    AfterInference       │
//! ├─────────────────────────┤
//! │  ┌───────────────────┐  │
//! │  │ BeforeToolExecute │  │ ← plugins can block/pending
//! │  ├───────────────────┤  │
//! │  │   [TOOL EXEC]     │  │
//! │  ├───────────────────┤  │
//! │  │ AfterToolExecute  │  │ ← plugins can add reminders
//! │  └───────────────────┘  │
//! ├─────────────────────────┤
//! │       StepEnd           │
//! └─────────────────────────┘
//!     │
//!     ▼
//! RunEnd (once)
//! ```

mod config;
mod core;
mod event_envelope_meta;
mod outcome;
mod parallel_state_merge;
mod plugin_runtime;
mod run_state;
mod state_commit;
mod stream_core;
mod stream_runner;
mod tool_exec;

use crate::contracts::io::ResumeDecisionAction;
use crate::contracts::runtime::phase::Phase;
use crate::contracts::runtime::state::{reduce_state_actions, AnyStateAction, ScopeContext};
use crate::contracts::runtime::tool_call::{Tool, ToolResult};
use crate::contracts::runtime::ActivityManager;
use crate::contracts::runtime::{
    DecisionReplayPolicy, RunLifecycleAction, RunLifecycleState, StreamResult, SuspendedCall,
    SuspendedCallAction, SuspendedCallState, ToolCallResume, ToolCallResumeMode, ToolCallStatus,
    ToolExecutionRequest, ToolExecutionResult,
};
use crate::contracts::thread::CheckpointReason;
use crate::contracts::thread::{gen_message_id, Message, MessageMetadata, ToolCall};
use crate::contracts::RunContext;
use crate::contracts::{AgentEvent, RunAction, TerminationReason, ToolCallDecision};
use crate::engine::convert::{assistant_message, assistant_tool_calls, tool_response};
use crate::runtime::activity::ActivityHub;

use crate::runtime::streaming::StreamCollector;
use async_stream::stream;
use futures::{Stream, StreamExt};
use genai::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

pub use crate::contracts::runtime::ToolExecutor;
pub use crate::runtime::run_context::{
    await_or_cancel, is_cancelled, CancelAware, RunCancellationToken, StateCommitError,
    StateCommitter, TOOL_SCOPE_CALLER_AGENT_ID_KEY, TOOL_SCOPE_CALLER_MESSAGES_KEY,
    TOOL_SCOPE_CALLER_STATE_KEY, TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use config::StaticStepToolProvider;
pub use config::{Agent, BaseAgent, GenaiLlmExecutor, LlmRetryPolicy};
pub use config::{LlmEventStream, LlmExecutor};
pub use config::{StepToolInput, StepToolProvider, StepToolSnapshot};
#[cfg(test)]
use core::build_messages;
use core::{
    build_request_for_filtered_tools, inference_inputs_from_step, suspended_calls_from_ctx,
    tool_call_states_from_ctx, transition_tool_call_state, upsert_tool_call_state,
    ToolCallStateSeed, ToolCallStateTransition,
};
pub use outcome::{tool_map, tool_map_from_arc, AgentLoopError};
pub use outcome::{LoopOutcome, LoopStats, LoopUsage};
#[cfg(test)]
use plugin_runtime::emit_agent_phase;
#[cfg(test)]
use plugin_runtime::emit_cleanup_phases;
use run_state::LoopRunState;
pub use state_commit::ChannelStateCommitter;
use state_commit::PendingDeltaCommitContext;
use std::time::{SystemTime, UNIX_EPOCH};
use tirea_state::TrackedPatch;
#[cfg(test)]
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use tool_exec::execute_single_tool_with_phases;
#[cfg(test)]
use tool_exec::execute_tools_parallel_with_phases;
pub use tool_exec::ExecuteToolsOutcome;
use tool_exec::{
    apply_tool_results_impl, apply_tool_results_to_session,
    execute_single_tool_with_phases_deferred, scope_with_tool_caller_context, step_metadata,
    ToolPhaseContext,
};
pub use tool_exec::{
    execute_tools, execute_tools_with_behaviors, execute_tools_with_config,
    ParallelToolExecutionMode, ParallelToolExecutor, SequentialToolExecutor,
};

/// Fully resolved agent wiring ready for execution.
///
/// Contains everything needed to run an agent loop: the agent,
/// the resolved tool map, and the runtime config. This is a pure data struct
/// that can be inspected, mutated, and tested independently.
pub struct ResolvedRun {
    /// The agent (model, behavior, execution strategies, ...).
    ///
    /// Exposed as a concrete [`BaseAgent`] so callers can mutate fields
    /// (model, plugins, tool_executor, ...) between resolution and execution.
    /// Converted to `Arc<dyn Agent>` at the execution boundary.
    pub agent: BaseAgent,
    /// Resolved tool map after filtering and wiring.
    pub tools: HashMap<String, Arc<dyn Tool>>,
    /// Runtime configuration (user_id, run_id, ...).
    pub run_config: crate::contracts::RunConfig,
}

impl ResolvedRun {
    /// Add or replace a tool in the resolved tool map.
    #[must_use]
    pub fn with_tool(mut self, id: String, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(id, tool);
        self
    }

    /// Overlay tools from a map (insert-if-absent semantics).
    pub fn overlay_tools(&mut self, tools: HashMap<String, Arc<dyn Tool>>) {
        for (id, tool) in tools {
            self.tools.entry(id).or_insert(tool);
        }
    }
}

fn uuid_v7() -> String {
    Uuid::now_v7().simple().to_string()
}

pub(crate) fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

pub(super) fn sync_run_lifecycle_for_termination(
    run_ctx: &mut RunContext,
    termination: &TerminationReason,
) -> Result<(), AgentLoopError> {
    let run_id = run_ctx
        .run_config
        .value("run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let Some(run_id) = run_id else {
        return Ok(());
    };

    let (status, done_reason) = termination.to_run_status();

    let base_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let actions = vec![AnyStateAction::new::<RunLifecycleState>(
        RunLifecycleAction::Set {
            id: run_id.to_string(),
            status,
            done_reason,
            updated_at: current_unix_millis(),
        },
    )];
    let patches =
        reduce_state_actions(actions, &base_state, "agent_loop", &ScopeContext::run())
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    run_ctx.add_thread_patches(patches);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancellationStage {
    Inference,
    ToolExecution,
}

pub(super) const CANCELLATION_INFERENCE_USER_MESSAGE: &str =
    "The previous run was interrupted during inference. Please continue from the current context.";
pub(super) const CANCELLATION_TOOL_USER_MESSAGE: &str =
    "The previous run was interrupted while using tools. Please continue from the current context.";

pub(super) fn append_cancellation_user_message(run_ctx: &mut RunContext, stage: CancellationStage) {
    let content = match stage {
        CancellationStage::Inference => CANCELLATION_INFERENCE_USER_MESSAGE,
        CancellationStage::ToolExecution => CANCELLATION_TOOL_USER_MESSAGE,
    };
    run_ctx.add_message(Arc::new(Message::user(content)));
}

pub(super) fn effective_llm_models(agent: &dyn Agent) -> Vec<String> {
    let mut models = Vec::with_capacity(1 + agent.fallback_models().len());
    models.push(agent.model().to_string());
    for model in agent.fallback_models() {
        if model.trim().is_empty() {
            continue;
        }
        if !models.iter().any(|m| m == model) {
            models.push(model.clone());
        }
    }
    models
}

pub(super) fn llm_retry_attempts(agent: &dyn Agent) -> usize {
    agent.llm_retry_policy().max_attempts_per_model.max(1)
}

pub(super) fn is_retryable_llm_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let non_retryable = [
        "401",
        "403",
        "404",
        "400",
        "422",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_request",
        "bad request",
    ];
    if non_retryable.iter().any(|p| lower.contains(p)) {
        return false;
    }
    let retryable = [
        "429",
        "too many requests",
        "rate limit",
        "timeout",
        "timed out",
        "temporar",
        "connection",
        "network",
        "unavailable",
        "server error",
        "502",
        "503",
        "504",
        "reset by peer",
        "eof",
    ];
    retryable.iter().any(|p| lower.contains(p))
}

pub(super) fn retry_backoff_ms(agent: &dyn Agent, retry_index: usize) -> u64 {
    let policy = agent.llm_retry_policy();
    let initial = policy.initial_backoff_ms;
    let cap = policy.max_backoff_ms.max(policy.initial_backoff_ms);
    if retry_index == 0 {
        return initial.min(cap);
    }
    let shift = (retry_index - 1).min(20) as u32;
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    initial.saturating_mul(factor).min(cap)
}

pub(super) async fn wait_retry_backoff(
    agent: &dyn Agent,
    retry_index: usize,
    run_cancellation_token: Option<&RunCancellationToken>,
) -> bool {
    let wait_ms = retry_backoff_ms(agent, retry_index);
    match await_or_cancel(
        run_cancellation_token,
        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)),
    )
    .await
    {
        CancelAware::Cancelled => true,
        CancelAware::Value(_) => false,
    }
}

pub(super) enum LlmAttemptOutcome<T> {
    Success {
        value: T,
        model: String,
        attempts: usize,
    },
    Cancelled,
    Exhausted {
        last_error: String,
        attempts: usize,
    },
}

fn is_run_cancelled(token: Option<&RunCancellationToken>) -> bool {
    is_cancelled(token)
}

pub(super) fn step_tool_provider_for_run(
    agent: &dyn Agent,
    tools: HashMap<String, Arc<dyn Tool>>,
) -> Arc<dyn StepToolProvider> {
    agent.step_tool_provider().unwrap_or_else(|| {
        Arc::new(StaticStepToolProvider::new(tools)) as Arc<dyn StepToolProvider>
    })
}

pub(super) fn llm_executor_for_run(agent: &dyn Agent) -> Arc<dyn LlmExecutor> {
    agent
        .llm_executor()
        .unwrap_or_else(|| Arc::new(GenaiLlmExecutor::new(Client::default())))
}

pub(super) async fn resolve_step_tool_snapshot(
    step_tool_provider: &Arc<dyn StepToolProvider>,
    run_ctx: &RunContext,
) -> Result<StepToolSnapshot, AgentLoopError> {
    step_tool_provider
        .provide(StepToolInput { state: run_ctx })
        .await
}

fn mark_step_completed(run_state: &mut LoopRunState) {
    run_state.completed_steps += 1;
}

fn build_loop_outcome(
    run_ctx: RunContext,
    termination: TerminationReason,
    response: Option<String>,
    run_state: &LoopRunState,
    failure: Option<outcome::LoopFailure>,
) -> LoopOutcome {
    LoopOutcome {
        run_ctx,
        termination,
        response: response.filter(|text| !text.is_empty()),
        usage: run_state.usage(),
        stats: run_state.stats(),
        failure,
    }
}

pub(super) async fn run_llm_with_retry_and_fallback<T, Invoke, Fut>(
    agent: &dyn Agent,
    run_cancellation_token: Option<&RunCancellationToken>,
    retry_current_model: bool,
    unknown_error: &str,
    mut invoke: Invoke,
) -> LlmAttemptOutcome<T>
where
    Invoke: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = genai::Result<T>>,
{
    let mut last_llm_error = unknown_error.to_string();
    let model_candidates = effective_llm_models(agent);
    let max_attempts = llm_retry_attempts(agent);
    let mut total_attempts = 0usize;

    for model in model_candidates {
        for attempt in 1..=max_attempts {
            total_attempts = total_attempts.saturating_add(1);
            let response_res =
                match await_or_cancel(run_cancellation_token, invoke(model.clone())).await {
                    CancelAware::Cancelled => return LlmAttemptOutcome::Cancelled,
                    CancelAware::Value(resp) => resp,
                };

            match response_res {
                Ok(value) => {
                    return LlmAttemptOutcome::Success {
                        value,
                        model,
                        attempts: total_attempts,
                    };
                }
                Err(e) => {
                    let message = e.to_string();
                    last_llm_error =
                        format!("model='{model}' attempt={attempt}/{max_attempts}: {message}");
                    let can_retry_same_model = retry_current_model
                        && attempt < max_attempts
                        && is_retryable_llm_error(&message);
                    if can_retry_same_model {
                        let cancelled =
                            wait_retry_backoff(agent, attempt, run_cancellation_token).await;
                        if cancelled {
                            return LlmAttemptOutcome::Cancelled;
                        }
                        continue;
                    }
                    break;
                }
            }
        }
    }

    LlmAttemptOutcome::Exhausted {
        last_error: last_llm_error,
        attempts: total_attempts,
    }
}

pub(super) async fn run_step_prepare_phases(
    run_ctx: &RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) -> Result<(Vec<Message>, Vec<String>, RunAction, Vec<TrackedPatch>), AgentLoopError> {
    let system_prompt = agent.system_prompt().to_string();
    let ((messages, filtered_tools, run_action), pending) = plugin_runtime::run_phase_block(
        run_ctx,
        tool_descriptors,
        agent,
        &[Phase::StepStart, Phase::BeforeInference],
        |_| {},
        |step| inference_inputs_from_step(step, &system_prompt),
    )
    .await?;
    Ok((messages, filtered_tools, run_action, pending))
}

pub(super) struct PreparedStep {
    pub(super) messages: Vec<Message>,
    pub(super) filtered_tools: Vec<String>,
    pub(super) run_action: RunAction,
    pub(super) pending_patches: Vec<TrackedPatch>,
}

pub(super) async fn prepare_step_execution(
    run_ctx: &RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) -> Result<PreparedStep, AgentLoopError> {
    let (messages, filtered_tools, run_action, pending) =
        run_step_prepare_phases(run_ctx, tool_descriptors, agent).await?;
    Ok(PreparedStep {
        messages,
        filtered_tools,
        run_action,
        pending_patches: pending,
    })
}

pub(super) async fn apply_llm_error_cleanup(
    run_ctx: &mut RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
    error_type: &'static str,
    message: String,
) -> Result<(), AgentLoopError> {
    plugin_runtime::emit_cleanup_phases(run_ctx, tool_descriptors, agent, error_type, message).await
}

pub(super) async fn complete_step_after_inference(
    run_ctx: &mut RunContext,
    result: &StreamResult,
    step_meta: MessageMetadata,
    assistant_message_id: Option<String>,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) -> Result<Option<TerminationReason>, AgentLoopError> {
    let (run_action, pending) = plugin_runtime::run_phase_block(
        run_ctx,
        tool_descriptors,
        agent,
        &[Phase::AfterInference],
        |step| {
            use crate::contracts::runtime::inference::LLMResponse;
            step.extensions.insert(LLMResponse::success(result.clone()));
        },
        |step| step.run_action(),
    )
    .await?;
    run_ctx.add_thread_patches(pending);

    let assistant = assistant_turn_message(result, step_meta, assistant_message_id);
    run_ctx.add_message(Arc::new(assistant));

    let pending =
        plugin_runtime::emit_phase_block(Phase::StepEnd, run_ctx, tool_descriptors, agent, |_| {})
            .await?;
    run_ctx.add_thread_patches(pending);
    Ok(match run_action {
        RunAction::Terminate(reason) => Some(reason),
        RunAction::Continue => None,
    })
}

/// Emit events for a pending tool-call projection.
pub(super) fn pending_tool_events(call: &SuspendedCall) -> Vec<AgentEvent> {
    vec![
        AgentEvent::ToolCallStart {
            id: call.ticket.pending.id.clone(),
            name: call.ticket.pending.name.clone(),
        },
        AgentEvent::ToolCallReady {
            id: call.ticket.pending.id.clone(),
            name: call.ticket.pending.name.clone(),
            arguments: call.ticket.pending.arguments.clone(),
        },
    ]
}

pub(super) fn has_suspended_calls(run_ctx: &RunContext) -> bool {
    !suspended_calls_from_ctx(run_ctx).is_empty()
}

pub(super) fn suspended_call_ids(run_ctx: &RunContext) -> HashSet<String> {
    suspended_calls_from_ctx(run_ctx).into_keys().collect()
}

pub(super) fn newly_suspended_call_ids(
    run_ctx: &RunContext,
    baseline_ids: &HashSet<String>,
) -> HashSet<String> {
    suspended_calls_from_ctx(run_ctx)
        .into_keys()
        .filter(|id| !baseline_ids.contains(id))
        .collect()
}

pub(super) fn suspended_call_pending_events(run_ctx: &RunContext) -> Vec<AgentEvent> {
    let mut calls: Vec<SuspendedCall> = suspended_calls_from_ctx(run_ctx).into_values().collect();
    calls.sort_by(|left, right| left.call_id.cmp(&right.call_id));
    calls
        .into_iter()
        .flat_map(|call| pending_tool_events(&call))
        .collect()
}

pub(super) fn suspended_call_pending_events_for_ids(
    run_ctx: &RunContext,
    call_ids: &HashSet<String>,
) -> Vec<AgentEvent> {
    if call_ids.is_empty() {
        return Vec::new();
    }
    let mut calls: Vec<SuspendedCall> = suspended_calls_from_ctx(run_ctx)
        .into_iter()
        .filter_map(|(call_id, call)| call_ids.contains(&call_id).then_some(call))
        .collect();
    calls.sort_by(|left, right| left.call_id.cmp(&right.call_id));
    calls
        .into_iter()
        .flat_map(|call| pending_tool_events(&call))
        .collect()
}

pub(super) struct ToolExecutionContext {
    pub(super) state: serde_json::Value,
    pub(super) run_config: tirea_contract::RunConfig,
}

pub(super) fn prepare_tool_execution_context(
    run_ctx: &RunContext,
) -> Result<ToolExecutionContext, AgentLoopError> {
    let state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let run_config = scope_with_tool_caller_context(run_ctx, &state)?;
    Ok(ToolExecutionContext { state, run_config })
}

pub(super) async fn finalize_run_end(
    run_ctx: &mut RunContext,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
) {
    plugin_runtime::emit_run_end_phase(run_ctx, tool_descriptors, agent).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunFinishedCommitPolicy {
    Required,
    BestEffort,
}

fn normalize_termination_for_suspended_calls(
    run_ctx: &RunContext,
    termination: TerminationReason,
    response: Option<String>,
) -> (TerminationReason, Option<String>) {
    let final_termination = if !matches!(
        termination,
        TerminationReason::Error(_) | TerminationReason::Cancelled
    ) && has_suspended_calls(run_ctx)
    {
        TerminationReason::Suspended
    } else {
        termination
    };
    let final_response = if final_termination == TerminationReason::Suspended {
        None
    } else {
        response
    };
    (final_termination, final_response)
}

async fn persist_run_termination(
    run_ctx: &mut RunContext,
    termination: &TerminationReason,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    agent: &dyn Agent,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
    run_finished_commit_policy: RunFinishedCommitPolicy,
) -> Result<(), AgentLoopError> {
    sync_run_lifecycle_for_termination(run_ctx, termination)?;
    finalize_run_end(run_ctx, tool_descriptors, agent).await;
    if let Err(error) = pending_delta_commit
        .commit(run_ctx, CheckpointReason::RunFinished, true)
        .await
    {
        match run_finished_commit_policy {
            RunFinishedCommitPolicy::Required => return Err(error),
            RunFinishedCommitPolicy::BestEffort => {
                tracing::warn!(error = %error, "failed to commit run-finished delta");
            }
        }
    }
    Ok(())
}

fn stream_result_from_chat_response(response: &genai::chat::ChatResponse) -> StreamResult {
    let text = response
        .first_text()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let tool_calls: Vec<crate::contracts::thread::ToolCall> = response
        .tool_calls()
        .into_iter()
        .map(|tc| {
            crate::contracts::thread::ToolCall::new(
                &tc.call_id,
                &tc.fn_name,
                tc.fn_arguments.clone(),
            )
        })
        .collect();

    StreamResult {
        text,
        tool_calls,
        usage: Some(crate::runtime::streaming::token_usage_from_genai(
            &response.usage,
        )),
    }
}

fn assistant_turn_message(
    result: &StreamResult,
    step_meta: MessageMetadata,
    message_id: Option<String>,
) -> Message {
    let mut msg = if result.tool_calls.is_empty() {
        assistant_message(&result.text)
    } else {
        assistant_tool_calls(&result.text, result.tool_calls.clone())
    }
    .with_metadata(step_meta);
    if let Some(message_id) = message_id {
        msg = msg.with_id(message_id);
    }
    msg
}

struct RunStartDrainOutcome {
    events: Vec<AgentEvent>,
    replayed: bool,
}

fn decision_result_value(action: &ResumeDecisionAction, result: &Value) -> serde_json::Value {
    if result.is_null() {
        serde_json::Value::Bool(matches!(action, ResumeDecisionAction::Resume))
    } else {
        result.clone()
    }
}

fn runtime_resume_inputs(run_ctx: &RunContext) -> HashMap<String, ToolCallResume> {
    let mut decisions = HashMap::new();
    for (call_id, state) in tool_call_states_from_ctx(run_ctx) {
        if !matches!(state.status, ToolCallStatus::Resuming) {
            continue;
        }
        let Some(mut resume) = state.resume else {
            continue;
        };
        if resume.decision_id.trim().is_empty() {
            resume.decision_id = call_id.clone();
        }
        decisions.insert(call_id, resume);
    }
    decisions
}

fn settle_orphan_resuming_tool_states(
    run_ctx: &mut RunContext,
    suspended: &HashMap<String, SuspendedCall>,
    resumes: &HashMap<String, ToolCallResume>,
) -> Result<bool, AgentLoopError> {
    let states = tool_call_states_from_ctx(run_ctx);
    let mut changed = false;

    for (call_id, resume) in resumes {
        if suspended.contains_key(call_id) {
            continue;
        }
        let Some(state) = states.get(call_id).cloned() else {
            continue;
        };
        let target_status = match &resume.action {
            ResumeDecisionAction::Cancel => ToolCallStatus::Cancelled,
            ResumeDecisionAction::Resume => ToolCallStatus::Failed,
        };
        if state.status == target_status && state.resume.as_ref() == Some(resume) {
            continue;
        }

        let Some(next_state) = transition_tool_call_state(
            Some(state.clone()),
            ToolCallStateSeed {
                call_id: call_id.as_str(),
                tool_name: state.tool_name.as_str(),
                arguments: &state.arguments,
                status: state.status,
                resume_token: state.resume_token.clone(),
            },
            ToolCallStateTransition {
                status: target_status,
                resume_token: state.resume_token.clone(),
                resume: Some(resume.clone()),
                updated_at: current_unix_millis(),
            },
        ) else {
            continue;
        };

        let base_state = run_ctx
            .snapshot()
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
        let patch = upsert_tool_call_state(&base_state, call_id, next_state)?;
        if patch.patch().is_empty() {
            continue;
        }
        run_ctx.add_thread_patch(patch);
        changed = true;
    }

    Ok(changed)
}

fn all_suspended_calls_have_resume(
    suspended: &HashMap<String, SuspendedCall>,
    resumes: &HashMap<String, ToolCallResume>,
) -> bool {
    suspended
        .keys()
        .all(|call_id| resumes.contains_key(call_id))
}

async fn drain_resuming_tool_calls_and_replay(
    run_ctx: &mut RunContext,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
) -> Result<RunStartDrainOutcome, AgentLoopError> {
    let decisions = runtime_resume_inputs(run_ctx);
    if decisions.is_empty() {
        return Ok(RunStartDrainOutcome {
            events: Vec::new(),
            replayed: false,
        });
    }

    let suspended = suspended_calls_from_ctx(run_ctx);
    let mut state_changed = false;
    if settle_orphan_resuming_tool_states(run_ctx, &suspended, &decisions)? {
        state_changed = true;
    }
    if suspended.is_empty() {
        if state_changed {
            let snapshot = run_ctx
                .snapshot()
                .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
            return Ok(RunStartDrainOutcome {
                events: vec![AgentEvent::StateSnapshot { snapshot }],
                replayed: false,
            });
        }
        return Ok(RunStartDrainOutcome {
            events: Vec::new(),
            replayed: false,
        });
    }

    if matches!(
        agent.tool_executor().decision_replay_policy(),
        DecisionReplayPolicy::BatchAllSuspended
    ) && !all_suspended_calls_have_resume(&suspended, &decisions)
    {
        if state_changed {
            let snapshot = run_ctx
                .snapshot()
                .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
            return Ok(RunStartDrainOutcome {
                events: vec![AgentEvent::StateSnapshot { snapshot }],
                replayed: false,
            });
        }
        return Ok(RunStartDrainOutcome {
            events: Vec::new(),
            replayed: false,
        });
    }

    let mut events = Vec::new();
    let mut decision_ids: Vec<String> = decisions.keys().cloned().collect();
    decision_ids.sort();

    let mut replayed = false;

    for call_id in decision_ids {
        let Some(suspended_call) = suspended.get(&call_id).cloned() else {
            continue;
        };
        let Some(decision) = decisions.get(&call_id).cloned() else {
            continue;
        };
        replayed = true;
        let decision_result = decision_result_value(&decision.action, &decision.result);
        let resume_payload = ToolCallResume {
            result: decision_result.clone(),
            ..decision.clone()
        };
        events.push(AgentEvent::ToolCallResumed {
            target_id: suspended_call.call_id.clone(),
            result: decision_result.clone(),
        });

        match decision.action {
            ResumeDecisionAction::Cancel => {
                let cancel_reason = resume_payload.reason.clone();
                if upsert_tool_call_lifecycle_state(
                    run_ctx,
                    &suspended_call,
                    ToolCallStatus::Cancelled,
                    Some(resume_payload),
                )? {
                    state_changed = true;
                }
                events.push(append_denied_tool_result_message(
                    run_ctx,
                    &suspended_call.call_id,
                    Some(&suspended_call.tool_name),
                    cancel_reason.as_deref(),
                ));
                // Cancel path skips tool execution, so no automatic scope
                // cleanup runs. Delete just the suspended_call entry; keep
                // tool_call_state (Cancelled status) for audit.
                let cleanup_path = format!(
                    "__tool_call_scope.{}.suspended_call",
                    suspended_call.call_id
                );
                let cleanup_patch = tirea_state::Patch::with_ops(vec![
                    tirea_state::Op::delete(tirea_state::parse_path(&cleanup_path)),
                ]);
                let tracked = tirea_state::TrackedPatch::new(cleanup_patch)
                    .with_source("framework:scope_cleanup");
                if !tracked.patch().is_empty() {
                    state_changed = true;
                    run_ctx.add_thread_patch(tracked);
                }
            }
            ResumeDecisionAction::Resume => {
                if upsert_tool_call_lifecycle_state(
                    run_ctx,
                    &suspended_call,
                    ToolCallStatus::Resuming,
                    Some(resume_payload.clone()),
                )? {
                    state_changed = true;
                }
                let Some(tool_call) = replay_tool_call_for_resolution(
                    run_ctx,
                    &suspended_call,
                    &ToolCallDecision {
                        target_id: suspended_call.call_id.clone(),
                        resume: resume_payload.clone(),
                    },
                ) else {
                    continue;
                };
                let state = run_ctx
                    .snapshot()
                    .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
                let tool = tools.get(&tool_call.name).cloned();
                let rt_for_replay = scope_with_tool_caller_context(run_ctx, &state)?;
                let replay_phase_ctx = ToolPhaseContext {
                    tool_descriptors,
                    agent_behavior: Some(agent.behavior()),
                    activity_manager: tirea_contract::runtime::activity::NoOpActivityManager::arc(),
                    run_config: &rt_for_replay,
                    thread_id: run_ctx.thread_id(),
                    thread_messages: run_ctx.messages(),
                    cancellation_token: None,
                };
                let replay_result = execute_single_tool_with_phases_deferred(
                    tool.as_deref(),
                    &tool_call,
                    &state,
                    &replay_phase_ctx,
                )
                .await?;

                let replay_msg_id = gen_message_id();
                let replay_msg = tool_response(&tool_call.id, &replay_result.execution.result)
                    .with_id(replay_msg_id.clone());
                run_ctx.add_message(Arc::new(replay_msg));

                if !replay_result.reminders.is_empty() {
                    let msgs: Vec<Arc<Message>> = replay_result
                        .reminders
                        .iter()
                        .map(|reminder| {
                            Arc::new(Message::internal_system(format!(
                                "<system-reminder>{}</system-reminder>",
                                reminder
                            )))
                        })
                        .collect();
                    run_ctx.add_messages(msgs);
                }

                if let Some(patch) = replay_result.execution.patch.clone() {
                    state_changed = true;
                    run_ctx.add_thread_patch(patch);
                }
                if !replay_result.pending_patches.is_empty() {
                    state_changed = true;
                    run_ctx.add_thread_patches(replay_result.pending_patches.clone());
                }
                events.push(AgentEvent::ToolCallDone {
                    id: tool_call.id.clone(),
                    result: replay_result.execution.result,
                    patch: replay_result.execution.patch,
                    message_id: replay_msg_id,
                });

                if let Some(next_suspended_call) = replay_result.suspended_call.clone() {
                    let state = run_ctx
                        .snapshot()
                        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
                    let action = AnyStateAction::new_for_call::<SuspendedCallState>(
                        SuspendedCallAction::Set(next_suspended_call.clone()),
                        &next_suspended_call.call_id,
                    );
                    let patches = reduce_state_actions(
                        vec![action],
                        &state,
                        "agent_loop",
                        &ScopeContext::run(),
                    )
                    .map_err(|e| {
                        AgentLoopError::StateError(format!(
                            "failed to reduce suspended call action: {e}"
                        ))
                    })?;
                    for patch in patches {
                        if !patch.patch().is_empty() {
                            state_changed = true;
                            run_ctx.add_thread_patch(patch);
                        }
                    }
                    for event in pending_tool_events(&next_suspended_call) {
                        events.push(event);
                    }
                }
            }
        }
    }

    // No explicit clear_suspended_call needed: terminal-outcome tool calls
    // have their entire `__tool_call_scope.<call_id>` subtree deleted by
    // automatic scope cleanup in execute_single_tool_with_phases_impl.

    if state_changed {
        let snapshot = run_ctx
            .snapshot()
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
        events.push(AgentEvent::StateSnapshot { snapshot });
    }

    Ok(RunStartDrainOutcome { events, replayed })
}

async fn drain_run_start_resume_replay(
    run_ctx: &mut RunContext,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
    tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
) -> Result<RunStartDrainOutcome, AgentLoopError> {
    drain_resuming_tool_calls_and_replay(run_ctx, tools, agent, tool_descriptors).await
}

async fn commit_run_start_and_drain_replay(
    run_ctx: &mut RunContext,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
    active_tool_descriptors: &[crate::contracts::runtime::tool_call::ToolDescriptor],
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<RunStartDrainOutcome, AgentLoopError> {
    pending_delta_commit
        .commit(run_ctx, CheckpointReason::UserMessage, false)
        .await?;

    let run_start_drain =
        drain_run_start_resume_replay(run_ctx, tools, agent, active_tool_descriptors).await?;

    if run_start_drain.replayed {
        pending_delta_commit
            .commit(run_ctx, CheckpointReason::ToolResultsCommitted, false)
            .await?;
    }

    Ok(run_start_drain)
}

fn normalize_decision_tool_result(
    response: &serde_json::Value,
    fallback_arguments: &serde_json::Value,
) -> serde_json::Value {
    match response {
        serde_json::Value::Bool(_) => fallback_arguments.clone(),
        value => value.clone(),
    }
}

fn denied_tool_result_for_call(
    run_ctx: &RunContext,
    call_id: &str,
    fallback_tool_name: Option<&str>,
    decision_reason: Option<&str>,
) -> ToolResult {
    let tool_name = fallback_tool_name
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| find_tool_call_in_messages(run_ctx, call_id).map(|call| call.name))
        .unwrap_or_else(|| "tool".to_string());
    let reason = decision_reason
        .map(str::to_string)
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "User denied the action".to_string());
    ToolResult::error(tool_name, reason)
}

fn append_denied_tool_result_message(
    run_ctx: &mut RunContext,
    call_id: &str,
    fallback_tool_name: Option<&str>,
    decision_reason: Option<&str>,
) -> AgentEvent {
    let denied_result =
        denied_tool_result_for_call(run_ctx, call_id, fallback_tool_name, decision_reason);
    let message_id = gen_message_id();
    let denied_message = tool_response(call_id, &denied_result).with_id(message_id.clone());
    run_ctx.add_message(Arc::new(denied_message));
    AgentEvent::ToolCallDone {
        id: call_id.to_string(),
        result: denied_result,
        patch: None,
        message_id,
    }
}

fn find_tool_call_in_messages(run_ctx: &RunContext, call_id: &str) -> Option<ToolCall> {
    run_ctx.messages().iter().rev().find_map(|message| {
        message
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.iter().find(|call| call.id == call_id).cloned())
    })
}

fn replay_tool_call_for_resolution(
    _run_ctx: &RunContext,
    suspended_call: &SuspendedCall,
    decision: &ToolCallDecision,
) -> Option<ToolCall> {
    if matches!(decision.resume.action, ResumeDecisionAction::Cancel) {
        return None;
    }

    match suspended_call.ticket.resume_mode {
        ToolCallResumeMode::ReplayToolCall => Some(ToolCall::new(
            suspended_call.call_id.clone(),
            suspended_call.tool_name.clone(),
            suspended_call.arguments.clone(),
        )),
        ToolCallResumeMode::UseDecisionAsToolResult | ToolCallResumeMode::PassDecisionToTool => {
            Some(ToolCall::new(
                suspended_call.call_id.clone(),
                suspended_call.tool_name.clone(),
                normalize_decision_tool_result(&decision.resume.result, &suspended_call.arguments),
            ))
        }
    }
}

fn upsert_tool_call_lifecycle_state(
    run_ctx: &mut RunContext,
    suspended_call: &SuspendedCall,
    status: ToolCallStatus,
    resume: Option<ToolCallResume>,
) -> Result<bool, AgentLoopError> {
    let current_state = tool_call_states_from_ctx(run_ctx).remove(&suspended_call.call_id);
    let Some(tool_state) = transition_tool_call_state(
        current_state,
        ToolCallStateSeed {
            call_id: &suspended_call.call_id,
            tool_name: &suspended_call.tool_name,
            arguments: &suspended_call.arguments,
            status: ToolCallStatus::Suspended,
            resume_token: Some(suspended_call.ticket.pending.id.clone()),
        },
        ToolCallStateTransition {
            status,
            resume_token: Some(suspended_call.ticket.pending.id.clone()),
            resume,
            updated_at: current_unix_millis(),
        },
    ) else {
        return Ok(false);
    };

    let base_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let patch = upsert_tool_call_state(&base_state, &suspended_call.call_id, tool_state)?;
    if patch.patch().is_empty() {
        return Ok(false);
    }
    run_ctx.add_thread_patch(patch);
    Ok(true)
}

pub(super) fn resolve_suspended_call(
    run_ctx: &mut RunContext,
    response: &ToolCallDecision,
) -> Result<Option<DecisionReplayOutcome>, AgentLoopError> {
    let suspended_calls = suspended_calls_from_ctx(run_ctx);
    if suspended_calls.is_empty() {
        return Ok(None);
    }

    let suspended_call = suspended_calls
        .get(&response.target_id)
        .cloned()
        .or_else(|| {
            suspended_calls
                .values()
                .find(|call| {
                    call.ticket.suspension.id == response.target_id
                        || call.ticket.pending.id == response.target_id
                        || call.call_id == response.target_id
                })
                .cloned()
        });
    let Some(suspended_call) = suspended_call else {
        return Ok(None);
    };

    let _ = upsert_tool_call_lifecycle_state(
        run_ctx,
        &suspended_call,
        ToolCallStatus::Resuming,
        Some(response.resume.clone()),
    )?;

    Ok(Some(DecisionReplayOutcome {
        events: Vec::new(),
        resolved_call_ids: vec![suspended_call.call_id],
    }))
}

pub(super) fn drain_decision_channel(
    run_ctx: &mut RunContext,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
) -> Result<DecisionReplayOutcome, AgentLoopError> {
    let mut disconnected = false;
    if let Some(rx) = decision_rx.as_mut() {
        loop {
            match rx.try_recv() {
                Ok(response) => pending_decisions.push_back(response),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
    }
    if disconnected {
        *decision_rx = None;
    }

    if pending_decisions.is_empty() {
        return Ok(DecisionReplayOutcome {
            events: Vec::new(),
            resolved_call_ids: Vec::new(),
        });
    }

    let mut unresolved = VecDeque::new();
    let mut events = Vec::new();
    let mut resolved_call_ids = Vec::new();
    let mut seen = HashSet::new();

    while let Some(response) = pending_decisions.pop_front() {
        if let Some(outcome) = resolve_suspended_call(run_ctx, &response)? {
            for call_id in outcome.resolved_call_ids {
                if seen.insert(call_id.clone()) {
                    resolved_call_ids.push(call_id);
                }
            }
            events.extend(outcome.events);
        } else {
            unresolved.push_back(response);
        }
    }
    *pending_decisions = unresolved;

    Ok(DecisionReplayOutcome {
        events,
        resolved_call_ids,
    })
}

async fn replay_after_decisions(
    run_ctx: &mut RunContext,
    decisions_applied: bool,
    step_tool_provider: &Arc<dyn StepToolProvider>,
    agent: &dyn Agent,
    active_tool_descriptors: &mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<Vec<AgentEvent>, AgentLoopError> {
    if !decisions_applied {
        return Ok(Vec::new());
    }

    let decision_tools = resolve_step_tool_snapshot(step_tool_provider, run_ctx).await?;
    *active_tool_descriptors = decision_tools.descriptors.clone();

    let decision_drain = drain_run_start_resume_replay(
        run_ctx,
        &decision_tools.tools,
        agent,
        active_tool_descriptors,
    )
    .await?;

    pending_delta_commit
        .commit(run_ctx, CheckpointReason::ToolResultsCommitted, false)
        .await?;

    Ok(decision_drain.events)
}

async fn apply_decisions_and_replay(
    run_ctx: &mut RunContext,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
    step_tool_provider: &Arc<dyn StepToolProvider>,
    agent: &dyn Agent,
    active_tool_descriptors: &mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<Vec<AgentEvent>, AgentLoopError> {
    Ok(drain_and_replay_decisions(
        run_ctx,
        decision_rx,
        pending_decisions,
        None,
        step_tool_provider,
        agent,
        active_tool_descriptors,
        pending_delta_commit,
    )
    .await?
    .events)
}

pub(super) struct DecisionReplayOutcome {
    events: Vec<AgentEvent>,
    resolved_call_ids: Vec<String>,
}

async fn drain_and_replay_decisions(
    run_ctx: &mut RunContext,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
    decision: Option<ToolCallDecision>,
    step_tool_provider: &Arc<dyn StepToolProvider>,
    agent: &dyn Agent,
    active_tool_descriptors: &mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<DecisionReplayOutcome, AgentLoopError> {
    if let Some(decision) = decision {
        pending_decisions.push_back(decision);
    }
    let decision_drain = drain_decision_channel(run_ctx, decision_rx, pending_decisions)?;
    let mut events = decision_drain.events;
    let replay_events = replay_after_decisions(
        run_ctx,
        !decision_drain.resolved_call_ids.is_empty(),
        step_tool_provider,
        agent,
        active_tool_descriptors,
        pending_delta_commit,
    )
    .await?;
    events.extend(replay_events);

    Ok(DecisionReplayOutcome {
        events,
        resolved_call_ids: decision_drain.resolved_call_ids,
    })
}

async fn apply_decision_and_replay(
    run_ctx: &mut RunContext,
    response: ToolCallDecision,
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
    pending_decisions: &mut VecDeque<ToolCallDecision>,
    step_tool_provider: &Arc<dyn StepToolProvider>,
    agent: &dyn Agent,
    active_tool_descriptors: &mut Vec<crate::contracts::runtime::tool_call::ToolDescriptor>,
    pending_delta_commit: &PendingDeltaCommitContext<'_>,
) -> Result<DecisionReplayOutcome, AgentLoopError> {
    drain_and_replay_decisions(
        run_ctx,
        decision_rx,
        pending_decisions,
        Some(response),
        step_tool_provider,
        agent,
        active_tool_descriptors,
        pending_delta_commit,
    )
    .await
}

async fn recv_decision(
    decision_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Option<ToolCallDecision> {
    let rx = decision_rx.as_mut()?;
    rx.recv().await
}

/// Run the full agent loop until completion, suspension, cancellation, or error.
///
/// This is the primary non-streaming entry point. Tools are passed directly
/// and used as the default tool set unless the agent's step_tool_provider is set
/// (for dynamic per-step tool resolution).
pub async fn run_loop(
    agent: &dyn Agent,
    tools: HashMap<String, Arc<dyn Tool>>,
    mut run_ctx: RunContext,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    mut decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> LoopOutcome {
    let executor = llm_executor_for_run(agent);
    let tool_executor = agent.tool_executor();
    let mut run_state = LoopRunState::new();
    let mut pending_decisions = VecDeque::new();
    let run_cancellation_token = cancellation_token;
    let mut last_text = String::new();
    let step_tool_provider = step_tool_provider_for_run(agent, tools);
    let run_identity = stream_core::resolve_stream_run_identity(&mut run_ctx);
    let run_id = run_identity.run_id;
    let parent_run_id = run_identity.parent_run_id;
    let baseline_suspended_call_ids = suspended_call_ids(&run_ctx);
    let pending_delta_commit =
        PendingDeltaCommitContext::new(&run_id, parent_run_id.as_deref(), state_committer.as_ref());
    let initial_step_tools = match resolve_step_tool_snapshot(&step_tool_provider, &run_ctx).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let msg = error.to_string();
            return build_loop_outcome(
                run_ctx,
                TerminationReason::Error(msg.clone()),
                None,
                &run_state,
                Some(outcome::LoopFailure::State(msg)),
            );
        }
    };
    let StepToolSnapshot {
        tools: initial_tools,
        descriptors: initial_descriptors,
    } = initial_step_tools;
    let mut active_tool_descriptors = initial_descriptors;

    macro_rules! terminate_run {
        ($termination:expr, $response:expr, $failure:expr) => {{
            let reason: TerminationReason = $termination;
            let (final_termination, final_response) =
                normalize_termination_for_suspended_calls(&run_ctx, reason, $response);
            if let Err(error) = persist_run_termination(
                &mut run_ctx,
                &final_termination,
                &active_tool_descriptors,
                agent,
                &pending_delta_commit,
                RunFinishedCommitPolicy::Required,
            )
            .await
            {
                let msg = error.to_string();
                return build_loop_outcome(
                    run_ctx,
                    TerminationReason::Error(msg.clone()),
                    None,
                    &run_state,
                    Some(outcome::LoopFailure::State(msg)),
                );
            }
            return build_loop_outcome(
                run_ctx,
                final_termination,
                final_response,
                &run_state,
                $failure,
            );
        }};
    }

    // Phase: RunStart
    let pending = match plugin_runtime::emit_phase_block(
        Phase::RunStart,
        &run_ctx,
        &active_tool_descriptors,
        agent,
        |_| {},
    )
    .await
    {
        Ok(pending) => pending,
        Err(error) => {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }
    };
    run_ctx.add_thread_patches(pending);
    if let Err(error) = commit_run_start_and_drain_replay(
        &mut run_ctx,
        &initial_tools,
        agent,
        &active_tool_descriptors,
        &pending_delta_commit,
    )
    .await
    {
        let msg = error.to_string();
        terminate_run!(
            TerminationReason::Error(msg.clone()),
            None,
            Some(outcome::LoopFailure::State(msg))
        );
    }
    let run_start_new_suspended = newly_suspended_call_ids(&run_ctx, &baseline_suspended_call_ids);
    if !run_start_new_suspended.is_empty() {
        terminate_run!(TerminationReason::Suspended, None, None);
    }
    loop {
        if let Err(error) = apply_decisions_and_replay(
            &mut run_ctx,
            &mut decision_rx,
            &mut pending_decisions,
            &step_tool_provider,
            agent,
            &mut active_tool_descriptors,
            &pending_delta_commit,
        )
        .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        if is_run_cancelled(run_cancellation_token.as_ref()) {
            terminate_run!(TerminationReason::Cancelled, None, None);
        }

        let step_tools = match resolve_step_tool_snapshot(&step_tool_provider, &run_ctx).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        active_tool_descriptors = step_tools.descriptors.clone();

        let prepared = match prepare_step_execution(&run_ctx, &active_tool_descriptors, agent).await
        {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        run_ctx.add_thread_patches(prepared.pending_patches);

        match prepared.run_action {
            RunAction::Continue => {}
            RunAction::Terminate(reason) => {
                let response = if matches!(reason, TerminationReason::BehaviorRequested) {
                    Some(last_text.clone())
                } else {
                    None
                };
                terminate_run!(reason, response, None);
            }
        }

        // Call LLM with unified retry + fallback model strategy.
        let messages = prepared.messages;
        let filtered_tools = prepared.filtered_tools;
        let chat_options = agent.chat_options().cloned();
        let attempt_outcome = run_llm_with_retry_and_fallback(
            agent,
            run_cancellation_token.as_ref(),
            true,
            "unknown llm error",
            |model| {
                let request =
                    build_request_for_filtered_tools(&messages, &step_tools.tools, &filtered_tools);
                let executor = executor.clone();
                let chat_options = chat_options.clone();
                async move {
                    executor
                        .exec_chat_response(&model, request, chat_options.as_ref())
                        .await
                }
            },
        )
        .await;

        let response = match attempt_outcome {
            LlmAttemptOutcome::Success {
                value, attempts, ..
            } => {
                run_state.record_llm_attempts(attempts);
                value
            }
            LlmAttemptOutcome::Cancelled => {
                append_cancellation_user_message(&mut run_ctx, CancellationStage::Inference);
                terminate_run!(TerminationReason::Cancelled, None, None);
            }
            LlmAttemptOutcome::Exhausted {
                last_error,
                attempts,
            } => {
                run_state.record_llm_attempts(attempts);
                if let Err(phase_error) = apply_llm_error_cleanup(
                    &mut run_ctx,
                    &active_tool_descriptors,
                    agent,
                    "llm_exec_error",
                    last_error.clone(),
                )
                .await
                {
                    let msg = phase_error.to_string();
                    terminate_run!(
                        TerminationReason::Error(msg.clone()),
                        None,
                        Some(outcome::LoopFailure::State(msg))
                    );
                }
                terminate_run!(
                    TerminationReason::Error(last_error.clone()),
                    None,
                    Some(outcome::LoopFailure::Llm(last_error))
                );
            }
        };

        let result = stream_result_from_chat_response(&response);
        run_state.update_from_response(&result);
        last_text = result.text.clone();

        // Add assistant message
        let assistant_msg_id = gen_message_id();
        let step_meta = step_metadata(Some(run_id.clone()), run_state.completed_steps as u32);
        let post_inference_termination = match complete_step_after_inference(
            &mut run_ctx,
            &result,
            step_meta.clone(),
            Some(assistant_msg_id.clone()),
            &active_tool_descriptors,
            agent,
        )
        .await
        {
            Ok(reason) => reason,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        if let Err(error) = pending_delta_commit
            .commit(
                &mut run_ctx,
                CheckpointReason::AssistantTurnCommitted,
                false,
            )
            .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        mark_step_completed(&mut run_state);

        // Only `Stopped` termination is deferred past tool execution so the
        // current round's tools complete (e.g. MaxRounds lets tools finish).
        // All other reasons terminate immediately before tool execution.
        if let Some(reason) = &post_inference_termination {
            if !matches!(reason, TerminationReason::Stopped(_)) {
                terminate_run!(reason.clone(), Some(last_text.clone()), None);
            }
        }

        if !result.needs_tools() {
            run_state.record_step_without_tools();
            let reason = post_inference_termination.unwrap_or(TerminationReason::NaturalEnd);
            terminate_run!(reason, Some(last_text.clone()), None);
        }

        // Execute tools with phase hooks using configured execution strategy.
        let tool_context = match prepare_tool_execution_context(&run_ctx) {
            Ok(ctx) => ctx,
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };
        let thread_messages_for_tools = run_ctx.messages().to_vec();
        let thread_version_for_tools = run_ctx.version();

        let tool_exec_future = tool_executor.execute(ToolExecutionRequest {
            tools: &step_tools.tools,
            calls: &result.tool_calls,
            state: &tool_context.state,
            tool_descriptors: &active_tool_descriptors,
            agent_behavior: Some(agent.behavior()),
            activity_manager: tirea_contract::runtime::activity::NoOpActivityManager::arc(),
            run_config: &tool_context.run_config,
            thread_id: run_ctx.thread_id(),
            thread_messages: &thread_messages_for_tools,
            state_version: thread_version_for_tools,
            cancellation_token: run_cancellation_token.as_ref(),
        });
        let results = tool_exec_future.await.map_err(AgentLoopError::from);

        let results = match results {
            Ok(r) => r,
            Err(AgentLoopError::Cancelled) => {
                append_cancellation_user_message(&mut run_ctx, CancellationStage::ToolExecution);
                terminate_run!(TerminationReason::Cancelled, None, None);
            }
            Err(e) => {
                let msg = e.to_string();
                terminate_run!(
                    TerminationReason::Error(msg.clone()),
                    None,
                    Some(outcome::LoopFailure::State(msg))
                );
            }
        };

        if let Err(_e) = apply_tool_results_to_session(
            &mut run_ctx,
            &results,
            Some(step_meta),
            tool_executor.requires_parallel_patch_conflict_check(),
        ) {
            // On error, we can't easily rollback RunContext, so just terminate
            let msg = _e.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }
        if let Err(error) = pending_delta_commit
            .commit(&mut run_ctx, CheckpointReason::ToolResultsCommitted, false)
            .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        if let Err(error) = apply_decisions_and_replay(
            &mut run_ctx,
            &mut decision_rx,
            &mut pending_decisions,
            &step_tool_provider,
            agent,
            &mut active_tool_descriptors,
            &pending_delta_commit,
        )
        .await
        {
            let msg = error.to_string();
            terminate_run!(
                TerminationReason::Error(msg.clone()),
                None,
                Some(outcome::LoopFailure::State(msg))
            );
        }

        // If ALL tools are suspended (no completed results), terminate immediately.
        if has_suspended_calls(&run_ctx) {
            let has_completed = results
                .iter()
                .any(|r| !matches!(r.outcome, crate::contracts::ToolCallOutcome::Suspended));
            if !has_completed {
                terminate_run!(TerminationReason::Suspended, None, None);
            }
        }

        // Deferred post-inference termination: tools from the current round
        // have completed; stop the loop before the next inference.
        if let Some(reason) = post_inference_termination {
            terminate_run!(reason, Some(last_text.clone()), None);
        }

        // Track tool-step metrics for loop stats and plugin consumers.
        let error_count = results
            .iter()
            .filter(|r| r.execution.result.is_error())
            .count();
        run_state.record_tool_step(&result.tool_calls, error_count);
    }
}

/// Run the agent loop with streaming output.
///
/// Returns a stream of AgentEvent for real-time updates. Tools are passed
/// directly and used as the default tool set unless the agent's step_tool_provider
/// is set (for dynamic per-step tool resolution).
pub fn run_loop_stream(
    agent: Arc<dyn Agent>,
    tools: HashMap<String, Arc<dyn Tool>>,
    run_ctx: RunContext,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
    stream_runner::run_stream(
        agent,
        tools,
        run_ctx,
        cancellation_token,
        state_committer,
        decision_rx,
    )
}

#[cfg(test)]
mod tests;
