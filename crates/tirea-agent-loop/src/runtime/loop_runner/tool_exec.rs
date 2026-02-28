use super::core::{
    drain_agent_append_user_messages, set_agent_suspended_calls, transition_tool_call_state,
    ToolCallStateSeed, ToolCallStateTransition,
};
use super::plugin_runtime::emit_tool_phase;
use super::{
    Agent, AgentLoopError, BaseAgent, RunCancellationToken, TOOL_SCOPE_CALLER_MESSAGES_KEY,
    TOOL_SCOPE_CALLER_STATE_KEY, TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use crate::contracts::runtime::plugin::agent::AgentBehavior;
use crate::contracts::runtime::plugin::phase::{reduce_state_actions, AnyStateAction};
use crate::contracts::runtime::plugin::phase::{Phase, StepContext, ToolContext};
use crate::contracts::runtime::tool_call::{Tool, ToolDescriptor, ToolResult};
use crate::contracts::runtime::{
    ActivityManager, PendingToolCall, SuspendTicket, SuspendedCall, ToolCallResumeMode,
};
use crate::contracts::runtime::{
    DecisionReplayPolicy, StreamResult, ToolCallOutcome, ToolCallStatus, ToolExecution,
    ToolExecutionEffect, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError,
};
use crate::contracts::thread::Thread;
use crate::contracts::thread::{Message, MessageMetadata, ToolCall};
use crate::contracts::{RunContext, Suspension};
use crate::engine::convert::tool_response;
use crate::engine::tool_execution::collect_patches;
use crate::runtime::run_context::{await_or_cancel, is_cancelled, CancelAware};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tirea_state::{Patch, PatchExt, TrackedPatch};

/// Outcome of the public `execute_tools*` family of functions.
///
/// Tool execution can complete normally or suspend while waiting for
/// external resolution (e.g. human-in-the-loop approval).
#[derive(Debug)]
pub enum ExecuteToolsOutcome {
    /// All tool calls completed (successfully or with tool-level errors).
    Completed(Thread),
    /// Execution suspended on a tool call awaiting external decision.
    Suspended {
        thread: Thread,
        suspended_call: SuspendedCall,
    },
}

impl ExecuteToolsOutcome {
    /// Extract the thread from either variant.
    pub fn into_thread(self) -> Thread {
        match self {
            Self::Completed(t) | Self::Suspended { thread: t, .. } => t,
        }
    }

    /// Returns `true` when execution suspended on a tool call.
    pub fn is_suspended(&self) -> bool {
        matches!(self, Self::Suspended { .. })
    }
}

pub(super) struct AppliedToolResults {
    pub(super) suspended_calls: Vec<SuspendedCall>,
    pub(super) state_snapshot: Option<Value>,
}

#[derive(Clone)]
pub(super) struct ToolPhaseContext<'a> {
    pub(super) tool_descriptors: &'a [ToolDescriptor],
    pub(super) agent_behavior: Option<&'a dyn AgentBehavior>,
    pub(super) activity_manager: Arc<dyn ActivityManager>,
    pub(super) run_config: &'a tirea_contract::RunConfig,
    pub(super) thread_id: &'a str,
    pub(super) thread_messages: &'a [Arc<Message>],
    pub(super) cancellation_token: Option<&'a RunCancellationToken>,
}

impl<'a> ToolPhaseContext<'a> {
    pub(super) fn from_request(request: &'a ToolExecutionRequest<'a>) -> Self {
        Self {
            tool_descriptors: request.tool_descriptors,
            agent_behavior: request.agent_behavior,
            activity_manager: request.activity_manager.clone(),
            run_config: request.run_config,
            thread_id: request.thread_id,
            thread_messages: request.thread_messages,
            cancellation_token: request.cancellation_token,
        }
    }
}

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn suspended_call_from_tool_result(call: &ToolCall, result: &ToolResult) -> SuspendedCall {
    if let Some(mut explicit) = result.suspension() {
        if explicit.pending.id.trim().is_empty() || explicit.pending.name.trim().is_empty() {
            explicit.pending =
                PendingToolCall::new(call.id.clone(), call.name.clone(), call.arguments.clone());
        }
        return SuspendedCall::new(call, explicit);
    }

    let mut suspension = Suspension::new(&call.id, format!("tool:{}", call.name))
        .with_parameters(call.arguments.clone());
    if let Some(message) = result.message.as_ref() {
        suspension = suspension.with_message(message.clone());
    }

    SuspendedCall::new(
        call,
        SuspendTicket::new(
            suspension,
            PendingToolCall::new(call.id.clone(), call.name.clone(), call.arguments.clone()),
            ToolCallResumeMode::ReplayToolCall,
        ),
    )
}

fn persist_tool_call_status(
    step: &StepContext<'_>,
    call: &ToolCall,
    status: ToolCallStatus,
    suspended_call: Option<&SuspendedCall>,
) -> Result<(), AgentLoopError> {
    let current_state = step.ctx().tool_call_state_for(&call.id).map_err(|e| {
        AgentLoopError::StateError(format!(
            "failed to read tool call state for '{}' before setting {:?}: {e}",
            call.id, status
        ))
    })?;
    let previous_status = current_state
        .as_ref()
        .map(|state| state.status)
        .unwrap_or(ToolCallStatus::New);
    let current_resume_token = current_state
        .as_ref()
        .and_then(|state| state.resume_token.clone());
    let current_resume = current_state
        .as_ref()
        .and_then(|state| state.resume.clone());

    let (next_resume_token, next_resume) = match status {
        ToolCallStatus::Running => {
            if matches!(previous_status, ToolCallStatus::Resuming) {
                (current_resume_token.clone(), current_resume.clone())
            } else {
                (None, None)
            }
        }
        ToolCallStatus::Suspended => (
            suspended_call
                .map(|entry| entry.ticket.pending.id.clone())
                .or(current_resume_token.clone()),
            None,
        ),
        ToolCallStatus::Succeeded
        | ToolCallStatus::Failed
        | ToolCallStatus::Cancelled
        | ToolCallStatus::New
        | ToolCallStatus::Resuming => (current_resume_token, current_resume),
    };

    let Some(runtime_state) = transition_tool_call_state(
        current_state,
        ToolCallStateSeed {
            call_id: &call.id,
            tool_name: &call.name,
            arguments: &call.arguments,
            status: ToolCallStatus::New,
            resume_token: None,
        },
        ToolCallStateTransition {
            status,
            resume_token: next_resume_token,
            resume: next_resume,
            updated_at: now_unix_millis(),
        },
    ) else {
        return Err(AgentLoopError::StateError(format!(
            "invalid tool call status transition for '{}': {:?} -> {:?}",
            call.id, previous_status, status
        )));
    };

    step.ctx()
        .set_tool_call_state_for(&call.id, runtime_state)
        .map_err(|e| {
            AgentLoopError::StateError(format!(
                "failed to persist tool call state for '{}' as {:?}: {e}",
                call.id, status
            ))
        })
}

fn map_tool_executor_error(err: AgentLoopError, thread_id: &str) -> ToolExecutorError {
    match err {
        AgentLoopError::Cancelled => ToolExecutorError::Cancelled {
            thread_id: thread_id.to_string(),
        },
        other => ToolExecutorError::Failed {
            message: other.to_string(),
        },
    }
}

/// Executes all tool calls concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelToolExecutionMode {
    BatchApproval,
    Streaming,
}

/// Executes all tool calls concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParallelToolExecutor {
    mode: ParallelToolExecutionMode,
}

impl ParallelToolExecutor {
    pub const fn batch_approval() -> Self {
        Self {
            mode: ParallelToolExecutionMode::BatchApproval,
        }
    }

    pub const fn streaming() -> Self {
        Self {
            mode: ParallelToolExecutionMode::Streaming,
        }
    }

    fn mode_name(self) -> &'static str {
        match self.mode {
            ParallelToolExecutionMode::BatchApproval => "parallel_batch_approval",
            ParallelToolExecutionMode::Streaming => "parallel_streaming",
        }
    }
}

impl Default for ParallelToolExecutor {
    fn default() -> Self {
        Self::streaming()
    }
}

#[async_trait]
impl ToolExecutor for ParallelToolExecutor {
    async fn execute(
        &self,
        request: ToolExecutionRequest<'_>,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError> {
        let thread_id = request.thread_id;
        let phase_ctx = ToolPhaseContext::from_request(&request);
        execute_tools_parallel_with_phases(request.tools, request.calls, request.state, phase_ctx)
            .await
            .map_err(|e| map_tool_executor_error(e, thread_id))
    }

    fn name(&self) -> &'static str {
        self.mode_name()
    }

    fn requires_parallel_patch_conflict_check(&self) -> bool {
        true
    }

    fn decision_replay_policy(&self) -> DecisionReplayPolicy {
        match self.mode {
            ParallelToolExecutionMode::BatchApproval => DecisionReplayPolicy::BatchAllSuspended,
            ParallelToolExecutionMode::Streaming => DecisionReplayPolicy::Immediate,
        }
    }
}

/// Executes tool calls one-by-one in call order.
#[derive(Debug, Clone, Copy, Default)]
pub struct SequentialToolExecutor;

#[async_trait]
impl ToolExecutor for SequentialToolExecutor {
    async fn execute(
        &self,
        request: ToolExecutionRequest<'_>,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError> {
        let thread_id = request.thread_id;
        let phase_ctx = ToolPhaseContext::from_request(&request);
        execute_tools_sequential_with_phases(request.tools, request.calls, request.state, phase_ctx)
            .await
            .map_err(|e| map_tool_executor_error(e, thread_id))
    }

    fn name(&self) -> &'static str {
        "sequential"
    }
}

fn validate_parallel_state_patch_conflicts(
    results: &[ToolExecutionResult],
) -> Result<(), AgentLoopError> {
    for (left_idx, left) in results.iter().enumerate() {
        let mut left_patches: Vec<&TrackedPatch> = Vec::new();
        if let Some(ref patch) = left.execution.patch {
            left_patches.push(patch);
        }
        left_patches.extend(left.pending_patches.iter());

        if left_patches.is_empty() {
            continue;
        }

        for right in results.iter().skip(left_idx + 1) {
            let mut right_patches: Vec<&TrackedPatch> = Vec::new();
            if let Some(ref patch) = right.execution.patch {
                right_patches.push(patch);
            }
            right_patches.extend(right.pending_patches.iter());

            if right_patches.is_empty() {
                continue;
            }

            for left_patch in &left_patches {
                for right_patch in &right_patches {
                    let conflicts = left_patch.patch().conflicts_with(right_patch.patch());
                    if let Some(conflict) = conflicts.first() {
                        return Err(AgentLoopError::StateError(format!(
                            "conflicting parallel state patches between '{}' and '{}' at {}",
                            left.execution.call.id, right.execution.call.id, conflict.path
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

pub(super) fn apply_tool_results_to_session(
    run_ctx: &mut RunContext,
    results: &[ToolExecutionResult],
    metadata: Option<MessageMetadata>,
    check_parallel_patch_conflicts: bool,
) -> Result<AppliedToolResults, AgentLoopError> {
    apply_tool_results_impl(
        run_ctx,
        results,
        metadata,
        check_parallel_patch_conflicts,
        None,
    )
}

pub(super) fn apply_tool_results_impl(
    run_ctx: &mut RunContext,
    results: &[ToolExecutionResult],
    metadata: Option<MessageMetadata>,
    check_parallel_patch_conflicts: bool,
    tool_msg_ids: Option<&HashMap<String, String>>,
) -> Result<AppliedToolResults, AgentLoopError> {
    // Collect all suspended calls from results.
    let suspended: Vec<SuspendedCall> = results
        .iter()
        .filter_map(|r| {
            if matches!(r.outcome, ToolCallOutcome::Suspended) {
                r.suspended_call.clone()
            } else {
                None
            }
        })
        .collect();

    if check_parallel_patch_conflicts {
        validate_parallel_state_patch_conflicts(results)?;
    }

    // Collect patches from completed tools and plugin pending patches.
    let mut patches: Vec<TrackedPatch> = collect_patches(
        &results
            .iter()
            .map(|r| r.execution.clone())
            .collect::<Vec<_>>(),
    );
    let mut merged_pending_patch = Patch::new();
    for r in results {
        for pending in &r.pending_patches {
            merged_pending_patch.extend(pending.patch().clone());
        }
    }
    if !merged_pending_patch.is_empty() {
        patches.push(TrackedPatch::new(merged_pending_patch).with_source("agent_loop"));
    }
    let mut state_changed = !patches.is_empty();
    run_ctx.add_thread_patches(patches);

    // Add tool result messages for all executions.
    let tool_messages: Vec<Arc<Message>> = results
        .iter()
        .flat_map(|r| {
            let is_suspended = matches!(r.outcome, ToolCallOutcome::Suspended);
            let mut msgs = if is_suspended {
                vec![Message::tool(
                    &r.execution.call.id,
                    format!(
                        "Tool '{}' is awaiting approval. Execution paused.",
                        r.execution.call.name
                    ),
                )]
            } else {
                let mut tool_msg = tool_response(&r.execution.call.id, &r.execution.result);
                if let Some(id) = tool_msg_ids.and_then(|ids| ids.get(&r.execution.call.id)) {
                    tool_msg = tool_msg.with_id(id.clone());
                }
                vec![tool_msg]
            };
            for reminder in &r.reminders {
                msgs.push(Message::internal_system(format!(
                    "<system-reminder>{}</system-reminder>",
                    reminder
                )));
            }
            if let Some(ref meta) = metadata {
                for msg in &mut msgs {
                    msg.metadata = Some(meta.clone());
                }
            }
            msgs.into_iter().map(Arc::new).collect::<Vec<_>>()
        })
        .collect();

    run_ctx.add_messages(tool_messages);
    let appended_count = drain_agent_append_user_messages(run_ctx, results, metadata.as_ref())?;
    if appended_count > 0 {
        state_changed = true;
    }
    let existing_suspended = run_ctx.suspended_calls();

    if !suspended.is_empty() {
        let mut merged_suspended = existing_suspended;
        for call in &suspended {
            merged_suspended.insert(call.call_id.clone(), call.clone());
        }
        let state = run_ctx
            .snapshot()
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
        let patch =
            set_agent_suspended_calls(&state, merged_suspended.into_values().collect::<Vec<_>>())?;
        if !patch.patch().is_empty() {
            state_changed = true;
            run_ctx.add_thread_patch(patch);
        }
        let state_snapshot = if state_changed {
            Some(
                run_ctx
                    .snapshot()
                    .map_err(|e| AgentLoopError::StateError(e.to_string()))?,
            )
        } else {
            None
        };
        return Ok(AppliedToolResults {
            suspended_calls: suspended,
            state_snapshot,
        });
    }

    // Keep unresolved suspended calls until explicit resolution.
    //
    // Do not emit a synthetic "clear suspended calls" patch when there are
    // no suspended calls in state. That no-op clear generated one redundant
    // control-state patch per tool execution and inflated patch histories.

    let state_snapshot = if state_changed {
        Some(
            run_ctx
                .snapshot()
                .map_err(|e| AgentLoopError::StateError(e.to_string()))?,
        )
    } else {
        None
    };

    Ok(AppliedToolResults {
        suspended_calls: Vec::new(),
        state_snapshot,
    })
}

fn tool_result_metadata_from_run_ctx(run_ctx: &RunContext) -> Option<MessageMetadata> {
    let run_id = run_ctx
        .run_config
        .value("run_id")
        .and_then(|v| v.as_str().map(String::from))
        .or_else(|| {
            run_ctx.messages().iter().rev().find_map(|m| {
                m.metadata
                    .as_ref()
                    .and_then(|meta| meta.run_id.as_ref().cloned())
            })
        });

    let step_index = run_ctx
        .messages()
        .iter()
        .rev()
        .find_map(|m| m.metadata.as_ref().and_then(|meta| meta.step_index));

    if run_id.is_none() && step_index.is_none() {
        None
    } else {
        Some(MessageMetadata { run_id, step_index })
    }
}

#[allow(dead_code)]
pub(super) fn next_step_index(run_ctx: &RunContext) -> u32 {
    run_ctx
        .messages()
        .iter()
        .filter_map(|m| m.metadata.as_ref().and_then(|meta| meta.step_index))
        .max()
        .map(|v| v.saturating_add(1))
        .unwrap_or(0)
}

pub(super) fn step_metadata(run_id: Option<String>, step_index: u32) -> MessageMetadata {
    MessageMetadata {
        run_id,
        step_index: Some(step_index),
    }
}

/// Execute tool calls (simplified version without plugins).
///
/// This is the simpler API for tests and cases where no behavior is needed.
pub async fn execute_tools(
    thread: Thread,
    result: &StreamResult,
    tools: &HashMap<String, Arc<dyn Tool>>,
    parallel: bool,
) -> Result<ExecuteToolsOutcome, AgentLoopError> {
    let parallel_executor = ParallelToolExecutor::streaming();
    let sequential_executor = SequentialToolExecutor;
    let executor: &dyn ToolExecutor = if parallel {
        &parallel_executor
    } else {
        &sequential_executor
    };
    execute_tools_with_agent_and_executor(thread, result, tools, executor, None).await
}

/// Execute tool calls with phase-based plugin hooks.
pub async fn execute_tools_with_config(
    thread: Thread,
    result: &StreamResult,
    tools: &HashMap<String, Arc<dyn Tool>>,
    agent: &dyn Agent,
) -> Result<ExecuteToolsOutcome, AgentLoopError> {
    execute_tools_with_agent_and_executor(
        thread,
        result,
        tools,
        agent.tool_executor().as_ref(),
        Some(agent.behavior()),
    )
    .await
}

pub(super) fn scope_with_tool_caller_context(
    run_ctx: &RunContext,
    state: &Value,
) -> Result<tirea_contract::RunConfig, AgentLoopError> {
    let mut rt = run_ctx.run_config.clone();
    if rt.value(TOOL_SCOPE_CALLER_THREAD_ID_KEY).is_none() {
        rt.set(
            TOOL_SCOPE_CALLER_THREAD_ID_KEY,
            run_ctx.thread_id().to_string(),
        )
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    }
    if rt.value(TOOL_SCOPE_CALLER_STATE_KEY).is_none() {
        rt.set(TOOL_SCOPE_CALLER_STATE_KEY, state.clone())
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    }
    if rt.value(TOOL_SCOPE_CALLER_MESSAGES_KEY).is_none() {
        rt.set(TOOL_SCOPE_CALLER_MESSAGES_KEY, run_ctx.messages().to_vec())
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    }
    Ok(rt)
}

/// Execute tool calls with behavior hooks.
pub async fn execute_tools_with_behaviors(
    thread: Thread,
    result: &StreamResult,
    tools: &HashMap<String, Arc<dyn Tool>>,
    parallel: bool,
    behavior: Arc<dyn AgentBehavior>,
) -> Result<ExecuteToolsOutcome, AgentLoopError> {
    let executor: Arc<dyn ToolExecutor> = if parallel {
        Arc::new(ParallelToolExecutor::streaming())
    } else {
        Arc::new(SequentialToolExecutor)
    };
    let agent = BaseAgent::default()
        .with_behavior(behavior)
        .with_tool_executor(executor);
    execute_tools_with_config(thread, result, tools, &agent).await
}

async fn execute_tools_with_agent_and_executor(
    thread: Thread,
    result: &StreamResult,
    tools: &HashMap<String, Arc<dyn Tool>>,
    executor: &dyn ToolExecutor,
    behavior: Option<&dyn AgentBehavior>,
) -> Result<ExecuteToolsOutcome, AgentLoopError> {
    // Build RunContext from thread for internal use
    let rebuilt_state = thread
        .rebuild_state()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let mut run_ctx = RunContext::new(
        &thread.id,
        rebuilt_state.clone(),
        thread.messages.clone(),
        tirea_contract::RunConfig::default(),
    );

    let tool_descriptors: Vec<ToolDescriptor> =
        tools.values().map(|t| t.descriptor().clone()).collect();
    // Run the RunStart phase via behavior dispatch
    if let Some(behavior) = behavior {
        let run_start_patches = super::plugin_runtime::behavior_run_phase_block(
            &run_ctx,
            &tool_descriptors,
            behavior,
            &[Phase::RunStart],
            |_| {},
            |_| (),
        )
        .await?
        .1;
        if !run_start_patches.is_empty() {
            run_ctx.add_thread_patches(run_start_patches);
        }
    }

    let replay_executor: Arc<dyn ToolExecutor> = match executor.decision_replay_policy() {
        DecisionReplayPolicy::BatchAllSuspended => Arc::new(ParallelToolExecutor::batch_approval()),
        DecisionReplayPolicy::Immediate => Arc::new(ParallelToolExecutor::streaming()),
    };
    let replay_config = BaseAgent::default().with_tool_executor(replay_executor);
    let replay = super::drain_resuming_tool_calls_and_replay(
        &mut run_ctx,
        tools,
        &replay_config,
        &tool_descriptors,
    )
    .await?;

    if replay.replayed {
        let suspended = run_ctx.suspended_calls().values().next().cloned();
        let delta = run_ctx.take_delta();
        let mut out_thread = thread;
        for msg in delta.messages {
            out_thread = out_thread.with_message((*msg).clone());
        }
        out_thread = out_thread.with_patches(delta.patches);
        return if let Some(first) = suspended {
            Ok(ExecuteToolsOutcome::Suspended {
                thread: out_thread,
                suspended_call: first,
            })
        } else {
            Ok(ExecuteToolsOutcome::Completed(out_thread))
        };
    }

    if result.tool_calls.is_empty() {
        let delta = run_ctx.take_delta();
        let mut out_thread = thread;
        for msg in delta.messages {
            out_thread = out_thread.with_message((*msg).clone());
        }
        out_thread = out_thread.with_patches(delta.patches);
        return Ok(ExecuteToolsOutcome::Completed(out_thread));
    }

    let current_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let rt_for_tools = scope_with_tool_caller_context(&run_ctx, &current_state)?;
    let results = executor
        .execute(ToolExecutionRequest {
            tools,
            calls: &result.tool_calls,
            state: &current_state,
            tool_descriptors: &tool_descriptors,
            agent_behavior: behavior,
            activity_manager: tirea_contract::runtime::activity::NoOpActivityManager::arc(),
            run_config: &rt_for_tools,
            thread_id: run_ctx.thread_id(),
            thread_messages: run_ctx.messages(),
            state_version: run_ctx.version(),
            cancellation_token: None,
        })
        .await?;

    let metadata = tool_result_metadata_from_run_ctx(&run_ctx);
    let applied = apply_tool_results_to_session(
        &mut run_ctx,
        &results,
        metadata,
        executor.requires_parallel_patch_conflict_check(),
    )?;
    let suspended = applied.suspended_calls.into_iter().next();

    // Reconstruct thread from RunContext delta
    let delta = run_ctx.take_delta();
    let mut out_thread = thread;
    for msg in delta.messages {
        out_thread = out_thread.with_message((*msg).clone());
    }
    out_thread = out_thread.with_patches(delta.patches);

    if let Some(first) = suspended {
        Ok(ExecuteToolsOutcome::Suspended {
            thread: out_thread,
            suspended_call: first,
        })
    } else {
        Ok(ExecuteToolsOutcome::Completed(out_thread))
    }
}

/// Execute tools in parallel with phase hooks.
pub(super) async fn execute_tools_parallel_with_phases(
    tools: &HashMap<String, Arc<dyn Tool>>,
    calls: &[crate::contracts::thread::ToolCall],
    state: &Value,
    phase_ctx: ToolPhaseContext<'_>,
) -> Result<Vec<ToolExecutionResult>, AgentLoopError> {
    use futures::future::join_all;

    if is_cancelled(phase_ctx.cancellation_token) {
        return Err(cancelled_error(phase_ctx.thread_id));
    }

    // Clone run config for parallel tasks (RunConfig is Clone).
    let run_config_owned = phase_ctx.run_config.clone();
    let thread_id = phase_ctx.thread_id.to_string();
    let thread_messages = Arc::new(phase_ctx.thread_messages.to_vec());
    let tool_descriptors = phase_ctx.tool_descriptors.to_vec();
    let agent = phase_ctx.agent_behavior;

    let futures = calls.iter().map(|call| {
        let tool = tools.get(&call.name).cloned();
        let state = state.clone();
        let call = call.clone();
        let tool_descriptors = tool_descriptors.clone();
        let activity_manager = phase_ctx.activity_manager.clone();
        let rt = run_config_owned.clone();
        let sid = thread_id.clone();
        let thread_messages = thread_messages.clone();

        async move {
            execute_single_tool_with_phases(
                tool.as_deref(),
                &call,
                &state,
                &ToolPhaseContext {
                    tool_descriptors: &tool_descriptors,
                    agent_behavior: agent,
                    activity_manager,
                    run_config: &rt,
                    thread_id: &sid,
                    thread_messages: thread_messages.as_slice(),
                    cancellation_token: None,
                },
            )
            .await
        }
    });

    let join_future = join_all(futures);
    let results = match await_or_cancel(phase_ctx.cancellation_token, join_future).await {
        CancelAware::Cancelled => return Err(cancelled_error(&thread_id)),
        CancelAware::Value(results) => results,
    };
    let results: Vec<ToolExecutionResult> = results.into_iter().collect::<Result<_, _>>()?;
    Ok(results)
}

/// Execute tools sequentially with phase hooks.
pub(super) async fn execute_tools_sequential_with_phases(
    tools: &HashMap<String, Arc<dyn Tool>>,
    calls: &[crate::contracts::thread::ToolCall],
    initial_state: &Value,
    phase_ctx: ToolPhaseContext<'_>,
) -> Result<Vec<ToolExecutionResult>, AgentLoopError> {
    use tirea_state::apply_patch;

    if is_cancelled(phase_ctx.cancellation_token) {
        return Err(cancelled_error(phase_ctx.thread_id));
    }

    let mut state = initial_state.clone();
    let mut results = Vec::with_capacity(calls.len());

    for call in calls {
        let tool = tools.get(&call.name).cloned();
        let call_phase_ctx = ToolPhaseContext {
            tool_descriptors: phase_ctx.tool_descriptors,
            agent_behavior: phase_ctx.agent_behavior,
            activity_manager: phase_ctx.activity_manager.clone(),
            run_config: phase_ctx.run_config,
            thread_id: phase_ctx.thread_id,
            thread_messages: phase_ctx.thread_messages,
            cancellation_token: None,
        };
        let result = match await_or_cancel(
            phase_ctx.cancellation_token,
            execute_single_tool_with_phases(tool.as_deref(), call, &state, &call_phase_ctx),
        )
        .await
        {
            CancelAware::Cancelled => return Err(cancelled_error(phase_ctx.thread_id)),
            CancelAware::Value(result) => result?,
        };

        // Apply patch to state for next tool
        if let Some(ref patch) = result.execution.patch {
            state = apply_patch(&state, patch.patch()).map_err(|e| {
                AgentLoopError::StateError(format!(
                    "failed to apply tool patch for call '{}': {}",
                    result.execution.call.id, e
                ))
            })?;
        }
        // Apply pending patches from plugins to state for next tool
        for pp in &result.pending_patches {
            state = apply_patch(&state, pp.patch()).map_err(|e| {
                AgentLoopError::StateError(format!(
                    "failed to apply plugin patch for call '{}': {}",
                    result.execution.call.id, e
                ))
            })?;
        }

        results.push(result);

        if results
            .last()
            .is_some_and(|r| matches!(r.outcome, ToolCallOutcome::Suspended))
        {
            break;
        }
    }

    Ok(results)
}

/// Execute a single tool with phase hooks.
pub(super) async fn execute_single_tool_with_phases(
    tool: Option<&dyn Tool>,
    call: &crate::contracts::thread::ToolCall,
    state: &Value,
    phase_ctx: &ToolPhaseContext<'_>,
) -> Result<ToolExecutionResult, AgentLoopError> {
    // Create ToolCallContext for plugin phases
    let doc = tirea_state::DocCell::new(state.clone());
    let ops = std::sync::Mutex::new(Vec::new());
    let pending_messages = std::sync::Mutex::new(Vec::new());
    let plugin_scope = phase_ctx.run_config;
    let mut plugin_tool_call_ctx = crate::contracts::ToolCallContext::new(
        &doc,
        &ops,
        "plugin_phase",
        "plugin:tool_phase",
        plugin_scope,
        &pending_messages,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
    );
    if let Some(token) = phase_ctx.cancellation_token {
        plugin_tool_call_ctx = plugin_tool_call_ctx.with_cancellation_token(token);
    }

    // Create StepContext for this tool
    let mut step = StepContext::new(
        plugin_tool_call_ctx,
        phase_ctx.thread_id,
        phase_ctx.thread_messages,
        phase_ctx.tool_descriptors.to_vec(),
    );
    step.tool = Some(ToolContext::new(call));
    // Phase: BeforeToolExecute
    emit_tool_phase(
        Phase::BeforeToolExecute,
        &mut step,
        phase_ctx.agent_behavior,
        &doc,
    )
    .await?;

    // Check if blocked or pending
    let (mut execution, outcome, suspended_call, mut tool_state_actions) = if step.tool_blocked() {
        let reason = step
            .tool
            .as_ref()
            .and_then(|t| t.block_reason.clone())
            .unwrap_or_else(|| "Blocked by plugin".to_string());
        (
            ToolExecution {
                call: call.clone(),
                result: ToolResult::error(&call.name, reason),
                patch: None,
            },
            ToolCallOutcome::Failed,
            None,
            Vec::new(),
        )
    } else if let Some(plugin_result) = step.tool_result().cloned() {
        let outcome = ToolCallOutcome::from_tool_result(&plugin_result);
        (
            ToolExecution {
                call: call.clone(),
                result: plugin_result,
                patch: None,
            },
            outcome,
            None,
            Vec::new(),
        )
    } else if tool.is_none() {
        (
            ToolExecution {
                call: call.clone(),
                result: ToolResult::error(&call.name, format!("Tool '{}' not found", call.name)),
                patch: None,
            },
            ToolCallOutcome::Failed,
            None,
            Vec::new(),
        )
    } else if let Err(e) = tool.unwrap().validate_args(&call.arguments) {
        // Argument validation failed — return error to the LLM
        (
            ToolExecution {
                call: call.clone(),
                result: ToolResult::error(&call.name, e.to_string()),
                patch: None,
            },
            ToolCallOutcome::Failed,
            None,
            Vec::new(),
        )
    } else if step.tool_pending() {
        let Some(suspend_ticket) = step.tool.as_ref().and_then(|t| t.suspend_ticket.clone()) else {
            return Err(AgentLoopError::StateError(
                "tool is pending but suspend ticket is missing".to_string(),
            ));
        };
        (
            ToolExecution {
                call: call.clone(),
                result: ToolResult::suspended(
                    &call.name,
                    "Execution suspended; awaiting external decision",
                ),
                patch: None,
            },
            ToolCallOutcome::Suspended,
            Some(SuspendedCall::new(call, suspend_ticket)),
            Vec::new(),
        )
    } else {
        persist_tool_call_status(&step, call, ToolCallStatus::Running, None)?;
        // Execute the tool with its own ToolCallContext
        let tool_doc = tirea_state::DocCell::new(state.clone());
        let tool_ops = std::sync::Mutex::new(Vec::new());
        let tool_pending_msgs = std::sync::Mutex::new(Vec::new());
        let mut tool_ctx = crate::contracts::ToolCallContext::new(
            &tool_doc,
            &tool_ops,
            &call.id,
            format!("tool:{}", call.name),
            plugin_scope,
            &tool_pending_msgs,
            phase_ctx.activity_manager.clone(),
        );
        if let Some(token) = phase_ctx.cancellation_token {
            tool_ctx = tool_ctx.with_cancellation_token(token);
        }
        let effect = match tool
            .unwrap()
            .execute_effect(call.arguments.clone(), &tool_ctx)
            .await
        {
            Ok(effect) => effect,
            Err(e) => ToolExecutionEffect::from(ToolResult::error(&call.name, e.to_string())),
        };
        let result = effect.result;

        let patch = tool_ctx.take_patch();
        let patch = if patch.patch().is_empty() {
            None
        } else {
            Some(patch)
        };
        let outcome = ToolCallOutcome::from_tool_result(&result);

        let suspended_call = if matches!(outcome, ToolCallOutcome::Suspended) {
            Some(suspended_call_from_tool_result(call, &result))
        } else {
            None
        };

        (
            ToolExecution {
                call: call.clone(),
                result,
                patch,
            },
            outcome,
            suspended_call,
            effect.state_actions,
        )
    };

    // Set tool result in context
    step.set_tool_result(execution.result.clone());

    // Phase: AfterToolExecute
    emit_tool_phase(
        Phase::AfterToolExecute,
        &mut step,
        phase_ctx.agent_behavior,
        &doc,
    )
    .await?;

    match outcome {
        ToolCallOutcome::Suspended => {
            persist_tool_call_status(
                &step,
                call,
                ToolCallStatus::Suspended,
                suspended_call.as_ref(),
            )?;
        }
        ToolCallOutcome::Succeeded => {
            persist_tool_call_status(&step, call, ToolCallStatus::Succeeded, None)?;
        }
        ToolCallOutcome::Failed => {
            persist_tool_call_status(&step, call, ToolCallStatus::Failed, None)?;
        }
    }

    // Flush plugin state ops into pending patches
    let plugin_patch = step.ctx().take_patch();
    if !plugin_patch.patch().is_empty() {
        step.emit_patch(plugin_patch);
    }

    let had_promoted_tool_patch = execution.patch.is_some();
    if let Some(tool_patch) = execution.patch.take() {
        tracing::warn!(
            tool_name = %execution.call.name,
            tool_call_id = %execution.call.id,
            "tool emitted direct state patch via ToolCallContext; promote to ToolExecutionEffect state_actions for unified reducer path"
        );
        tool_state_actions.insert(0, AnyStateAction::Patch(tool_patch));
    }

    let phase_patch_actions = std::mem::take(&mut step.pending_patches)
        .into_iter()
        .map(AnyStateAction::Patch);
    tool_state_actions.extend(phase_patch_actions);

    let mut pending_patches =
        reduce_tool_state_actions(state, tool_state_actions, &format!("tool:{}", call.name))?;
    if had_promoted_tool_patch {
        let Some(tool_patch) = pending_patches.first().cloned() else {
            return Err(AgentLoopError::StateError(format!(
                "tool '{}' emitted a direct patch but reducer output was empty",
                call.name
            )));
        };
        execution.patch = Some(tool_patch);
        let _ = pending_patches.remove(0);
    }

    Ok(ToolExecutionResult {
        execution,
        outcome,
        suspended_call,
        reminders: step.system_reminders.clone(),
        pending_patches,
    })
}

fn reduce_tool_state_actions(
    base_state: &Value,
    actions: Vec<AnyStateAction>,
    source: &str,
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    reduce_state_actions(actions, base_state, source).map_err(|e| {
        AgentLoopError::StateError(format!("failed to reduce tool state actions: {e}"))
    })
}

fn cancelled_error(_thread_id: &str) -> AgentLoopError {
    AgentLoopError::Cancelled
}
