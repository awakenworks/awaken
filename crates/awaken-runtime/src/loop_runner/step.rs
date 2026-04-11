//! Single step execution: inference, tool execution, and tool result processing.

use std::sync::Arc;

use crate::cancellation::CancellationToken;
use crate::context::{TruncationState, continuation_message, should_retry};
use crate::hooks::PhaseContext;
use crate::phase::PhaseRuntime;
use crate::registry::ResolvedAgent;
use crate::state::StateCommand;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::executor::InferenceRequest;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::inference::{InferenceOverride, LLMResponse, StreamResult};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::suspension::{SuspendTicket, ToolCallOutcome, ToolCallStatus};
use awaken_contract::contract::tool::ToolCallContext;
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;
use awaken_contract::model::Phase;

use super::actions::{
    apply_context_messages, apply_tool_filter_payloads, merge_override_payloads,
    resolve_intercept_payloads, take_context_messages,
};
use super::checkpoint::{StepCompletion, check_termination, complete_step};
use super::inference::{compact_with_llm, execute_streaming};
use super::{AgentLoopError, commit_update, now_ms, tool_result_to_content};
use crate::agent::state::{
    InferenceOverrideState, InferenceOverrideStateAction, RunLifecycle, RunLifecycleUpdate,
    ToolCallState, ToolCallStates, ToolCallStatesUpdate, ToolFilterState, ToolFilterStateAction,
};

const INTERRUPTED_TOOL_MESSAGE: &str = "[Tool execution was interrupted]";

/// Outcome of a single step.
pub(super) enum StepOutcome {
    /// The LLM responded with text only; run ends naturally.
    NaturalEnd,
    /// Tool calls were executed; continue to next step.
    Continue,
    /// A tool call was blocked; run terminates.
    Blocked(String),
    /// One or more tool calls are suspended.
    Suspended,
    /// Cancellation detected.
    Cancelled,
    /// A lifecycle hook signalled termination.
    Terminated(TerminationReason),
}

/// Context passed into each step of the agent loop.
pub(super) struct StepContext<'a> {
    pub agent: &'a mut ResolvedAgent,
    pub messages: &'a mut Vec<Arc<Message>>,
    pub runtime: &'a PhaseRuntime,
    pub sink: Arc<dyn EventSink>,
    pub checkpoint_store: Option<&'a dyn ThreadRunStore>,
    pub run_identity: &'a RunIdentity,
    pub cancellation_token: Option<&'a CancellationToken>,
    pub run_overrides: &'a Option<InferenceOverride>,
    pub total_input_tokens: &'a mut u64,
    pub total_output_tokens: &'a mut u64,
    pub truncation_state: &'a mut TruncationState,
    pub run_created_at: u64,
}

pub(super) struct ToolBatchTranscript {
    assistant_message: Option<Arc<Message>>,
    tool_messages: Vec<Arc<Message>>,
}

impl ToolBatchTranscript {
    pub(super) fn for_inference(text: String, calls: Vec<ToolCall>) -> Self {
        Self {
            assistant_message: Some(Arc::new(Message::assistant_with_tool_calls(text, calls))),
            tool_messages: Vec::new(),
        }
    }

    pub(super) fn for_resume() -> Self {
        Self {
            assistant_message: None,
            tool_messages: Vec::new(),
        }
    }

    fn visible_messages(&self, committed: &[Arc<Message>]) -> Vec<Arc<Message>> {
        let mut visible = committed.to_vec();
        if let Some(message) = &self.assistant_message {
            visible.push(Arc::clone(message));
        }
        visible.extend(self.tool_messages.iter().cloned());
        visible
    }

    fn push_tool_message(&mut self, message: Arc<Message>) {
        self.tool_messages.push(message);
    }

    pub(super) fn commit_into(self, committed: &mut Vec<Arc<Message>>) {
        if let Some(message) = self.assistant_message {
            committed.push(message);
        }
        committed.extend(self.tool_messages);
    }
}

pub(super) fn make_ctx(
    phase: Phase,
    msgs: &[Arc<Message>],
    identity: &RunIdentity,
    store: &crate::state::StateStore,
    cancellation_token: Option<&CancellationToken>,
) -> PhaseContext {
    let ctx = PhaseContext::new(phase, store.snapshot())
        .with_run_identity(identity.clone())
        .with_messages(msgs.to_vec());
    match cancellation_token {
        Some(token) => ctx.with_cancellation_token(token.clone()),
        None => ctx,
    }
}

fn tool_phase_context(
    ctx: &StepContext<'_>,
    transcript: &ToolBatchTranscript,
    phase: Phase,
    call: &ToolCall,
) -> PhaseContext {
    let visible_messages = transcript.visible_messages(ctx.messages);
    make_ctx(
        phase,
        &visible_messages,
        ctx.run_identity,
        ctx.runtime.store(),
        ctx.cancellation_token,
    )
    .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()))
}

fn active_resume_state(store: &crate::state::StateStore, call_id: &str) -> Option<ToolCallState> {
    store
        .read::<ToolCallStates>()
        .and_then(|states| states.calls.get(call_id).cloned())
        .filter(|state| state.status == ToolCallStatus::Resuming)
}

fn apply_resume_context(ctx: PhaseContext, resume_state: Option<&ToolCallState>) -> PhaseContext {
    let Some(resume_state) = resume_state else {
        return ctx;
    };

    let ctx = ctx.with_suspension(
        resume_state.suspension_id.clone(),
        resume_state.suspension_reason.clone(),
    );
    if let Some(resume_input) = resume_state.resume_input.clone() {
        ctx.with_resume_input(resume_input)
    } else {
        ctx
    }
}

fn apply_tool_resume_context(
    mut ctx: ToolCallContext,
    resume_state: Option<&ToolCallState>,
) -> ToolCallContext {
    let Some(resume_state) = resume_state else {
        return ctx;
    };

    ctx.resume_input = resume_state.resume_input.clone();
    ctx.suspension_id = resume_state.suspension_id.clone();
    ctx.suspension_reason = resume_state.suspension_reason.clone();
    ctx
}

/// Build a `StateCommand` that upserts a `ToolCallStates` entry for a given call and status.
fn tool_call_state_cmd(call: &ToolCall, status: ToolCallStatus) -> StateCommand {
    let mut cmd = StateCommand::new();
    cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Put(ToolCallState::new(
        call.id.clone(),
        call.name.clone(),
        call.arguments.clone(),
        status,
        now_ms(),
    )));
    cmd
}

/// Run a phase hook and check for termination afterwards.
async fn run_phase_and_check(
    ctx: &mut StepContext<'_>,
    phase: Phase,
) -> Result<Option<StepOutcome>, AgentLoopError> {
    let store = ctx.runtime.store();
    match ctx
        .runtime
        .run_phase_with_context(
            &ctx.agent.env,
            make_ctx(
                phase,
                ctx.messages,
                ctx.run_identity,
                store,
                ctx.cancellation_token,
            ),
        )
        .await
    {
        Ok(_) => Ok(check_termination(store).map(StepOutcome::Terminated)),
        Err(awaken_contract::StateError::Cancelled) => Ok(Some(StepOutcome::Cancelled)),
        Err(e) => Err(e.into()),
    }
}

/// Retry inference when the model hits max_tokens (truncation).
///
/// Appends the partial assistant response and a continuation prompt,
/// then re-executes inference. Repeats up to `max_continuation_retries`.
async fn recover_truncation(
    ctx: &mut StepContext<'_>,
    mut stream_result: StreamResult,
    transform_arcs: &[std::sync::Arc<
        dyn awaken_contract::contract::transform::InferenceRequestTransform,
    >],
    overrides: Option<InferenceOverride>,
) -> Result<StreamResult, AgentLoopError> {
    while should_retry(
        &stream_result,
        ctx.truncation_state,
        ctx.agent.max_continuation_retries(),
    ) {
        let partial_text = stream_result.text();
        ctx.messages
            .push(Arc::new(Message::assistant(&partial_text)));
        ctx.messages.push(Arc::new(continuation_message()));

        let has_sys = !ctx.agent.system_prompt().is_empty();
        let mut cont_messages: Vec<Message> = Vec::new();
        if has_sys {
            cont_messages.push(Message::system(ctx.agent.system_prompt()));
        }
        cont_messages.extend(ctx.messages.iter().map(|m| (**m).clone()));
        let cont_messages = awaken_contract::contract::transform::apply_transforms(
            cont_messages,
            &ctx.agent.tool_descriptors(),
            transform_arcs,
        );
        let upstream_model = effective_upstream_model(ctx.agent, overrides.as_ref())?;

        let cont_request = InferenceRequest {
            upstream_model,
            messages: cont_messages,
            tools: ctx.agent.tool_descriptors(),
            system: vec![],
            overrides: executor_overrides(overrides.clone()),
            enable_prompt_cache: false,
        };

        stream_result = execute_streaming(
            ctx.agent,
            cont_request,
            ctx.sink.as_ref(),
            ctx.cancellation_token,
            ctx.total_input_tokens,
            ctx.total_output_tokens,
        )
        .await?;
    }
    Ok(stream_result)
}

fn effective_upstream_model(
    agent: &ResolvedAgent,
    overrides: Option<&InferenceOverride>,
) -> Result<String, AgentLoopError> {
    let Some(upstream_model) = overrides.and_then(|overrides| overrides.upstream_model.as_ref())
    else {
        return Ok(agent.upstream_model.clone());
    };
    if upstream_model.trim().is_empty() {
        return Err(AgentLoopError::InferenceFailed(
            "inference override upstream_model cannot be empty".into(),
        ));
    }
    Ok(upstream_model.clone())
}

fn executor_overrides(mut overrides: Option<InferenceOverride>) -> Option<InferenceOverride> {
    if let Some(overrides) = overrides.as_mut() {
        // The primary upstream model is applied to `InferenceRequest.upstream_model`.
        // Keeping it in `InferenceRequest.overrides` would give executors two
        // sources of truth for the same routing decision.
        overrides.upstream_model = None;
    }

    overrides.filter(|overrides| !overrides.is_empty())
}

/// Run the BeforeInference phase via collect_commands, submit all actions to
/// the handler queue, and read accumulated state after EXECUTE.
///
/// Returns `Some(StepOutcome)` if a lifecycle hook triggered termination.
async fn run_before_inference(
    ctx: &mut StepContext<'_>,
) -> Result<
    (
        Option<StepOutcome>,
        Option<InferenceOverride>,
        Vec<String>,
        Vec<Vec<String>>,
    ),
    AgentLoopError,
> {
    let store = ctx.runtime.store();
    let phase_ctx = make_ctx(
        Phase::BeforeInference,
        ctx.messages,
        ctx.run_identity,
        store,
        ctx.cancellation_token,
    );

    // GATHER only — returns merged StateCommand without committing
    let cmd = ctx
        .runtime
        .collect_commands(&ctx.agent.env, phase_ctx.clone())
        .await?;

    // Submit ALL actions to the handler queue (no pre-extraction)
    if !cmd.is_empty() {
        ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
    }

    // Run EXECUTE loop — all handlers run, writing to state
    let exec_ctx = make_ctx(
        Phase::BeforeInference,
        ctx.messages,
        ctx.run_identity,
        store,
        ctx.cancellation_token,
    );
    ctx.runtime
        .run_execute_loop(&ctx.agent.env, exec_ctx)
        .await?;

    // Check termination after phase completes
    let termination = check_termination(store).map(StepOutcome::Terminated);

    // Read accumulated state written by handlers
    let tool_filter = store.read::<ToolFilterState>().unwrap_or_default();
    let override_state = store.read::<InferenceOverrideState>().unwrap_or_default();

    // Clear accumulators for next step
    let mut clear_patch = crate::state::MutationBatch::new();
    clear_patch.update::<ToolFilterState>(ToolFilterStateAction::Clear);
    clear_patch.update::<InferenceOverrideState>(InferenceOverrideStateAction::Clear);
    store.commit(clear_patch)?;

    // Build override chain: AgentSpec default < run-level < per-step plugin
    let mut overrides: Option<InferenceOverride> =
        ctx.agent
            .spec
            .reasoning_effort
            .as_ref()
            .map(|effort| InferenceOverride {
                reasoning_effort: Some(effort.clone()),
                ..Default::default()
            });
    if let Some(run_ovr) = ctx.run_overrides.clone() {
        match overrides.as_mut() {
            Some(o) => o.merge(run_ovr),
            None => overrides = Some(run_ovr),
        }
    }
    if let Some(step_override) = override_state.overrides {
        merge_override_payloads(&mut overrides, vec![step_override]);
    }

    Ok((
        termination,
        overrides,
        tool_filter.excluded,
        tool_filter.include_only,
    ))
}

struct InferencePhaseOutput {
    stream_result: StreamResult,
    duration_ms: u64,
    upstream_model: String,
}

/// Run the inference phase: compaction, request building, streaming.
async fn run_inference_phase(
    ctx: &mut StepContext<'_>,
    overrides: Option<InferenceOverride>,
    exclusion_payloads: Vec<String>,
    inclusion_payloads: Vec<Vec<String>>,
) -> Result<InferencePhaseOutput, AgentLoopError> {
    let store = ctx.runtime.store();

    // LLM compaction
    if let Some(policy) = ctx.agent.context_policy()
        && let Some(threshold) = policy.autocompact_threshold
    {
        let token_est = awaken_contract::contract::transform::estimate_tokens(ctx.messages);
        if token_est >= threshold {
            compact_with_llm(ctx.agent, ctx.messages, policy).await?;
        }
    }

    // Read context messages from persistent store (populated by AddContextMessage handler)
    let context_msgs = take_context_messages(store)?;

    let has_system_prompt = !ctx.agent.system_prompt().is_empty();
    let mut request_messages: Vec<Message> = Vec::new();
    if has_system_prompt {
        request_messages.push(Message::system(ctx.agent.system_prompt()));
    }
    request_messages.extend(ctx.messages.iter().map(|m| (**m).clone()));

    if !context_msgs.is_empty() {
        apply_context_messages(&mut request_messages, context_msgs, has_system_prompt);
    }

    let mut tools = ctx.agent.tool_descriptors();
    apply_tool_filter_payloads(&mut tools, exclusion_payloads, inclusion_payloads);
    let transform_arcs = ctx.agent.env.transform_arcs();
    let request_messages = awaken_contract::contract::transform::apply_transforms(
        request_messages,
        &tools,
        &transform_arcs,
    );

    let start = std::time::Instant::now();
    let enable_prompt_cache = ctx
        .agent
        .context_policy()
        .is_some_and(|p| p.enable_prompt_cache);
    let request_upstream_model = effective_upstream_model(ctx.agent, overrides.as_ref())?;
    let request = InferenceRequest {
        upstream_model: request_upstream_model.clone(),
        messages: request_messages,
        tools,
        system: vec![],
        overrides: executor_overrides(overrides.clone()),
        enable_prompt_cache,
    };

    // Inference
    let stream_result = execute_streaming(
        ctx.agent,
        request,
        ctx.sink.as_ref(),
        ctx.cancellation_token,
        ctx.total_input_tokens,
        ctx.total_output_tokens,
    )
    .await?;

    // Truncation recovery (separated from main inference for clarity)
    let stream_result = recover_truncation(ctx, stream_result, &transform_arcs, overrides).await?;

    let duration_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        model = %request_upstream_model,
        input_tokens = *ctx.total_input_tokens,
        output_tokens = *ctx.total_output_tokens,
        duration_ms,
        "inference_complete"
    );

    Ok(InferencePhaseOutput {
        stream_result,
        duration_ms,
        upstream_model: request_upstream_model,
    })
}

/// Run ToolGate hooks for a tool call and resolve the winning decision.
///
/// ToolGate is pure and may be re-evaluated after earlier allowed tool calls
/// commit new state in the same step.
async fn run_tool_gate(
    ctx: &mut StepContext<'_>,
    transcript: &ToolBatchTranscript,
    call: &ToolCall,
) -> Result<Option<ToolInterceptPayload>, AgentLoopError> {
    let store = ctx.runtime.store();
    let resume_state = active_resume_state(store, &call.id);
    let gate_ctx = apply_resume_context(
        tool_phase_context(ctx, transcript, Phase::ToolGate, call),
        resume_state.as_ref(),
    );

    let mut payloads = Vec::new();
    for hook in ctx.agent.env.tool_gate_hooks() {
        if let Some(payload) = hook.hook.run(&gate_ctx).await? {
            tracing::debug!(
                plugin_id = %hook.plugin_id,
                tool_name = %call.name,
                call_id = %call.id,
                payload = ?payload,
                "tool_gate_decision"
            );
            payloads.push(payload);
        }
    }

    Ok(resolve_intercept_payloads(payloads))
}

/// Run BeforeToolExecute immediately before the tool actually executes.
async fn run_before_tool_execute(
    ctx: &mut StepContext<'_>,
    transcript: &ToolBatchTranscript,
    call: &ToolCall,
) -> Result<(), AgentLoopError> {
    let store = ctx.runtime.store();
    let resume_state = active_resume_state(store, &call.id);
    let before_ctx = apply_resume_context(
        tool_phase_context(ctx, transcript, Phase::BeforeToolExecute, call),
        resume_state.as_ref(),
    );

    // GATHER only — submit ALL actions to handler queue
    let cmd = ctx
        .runtime
        .collect_commands(&ctx.agent.env, before_ctx.clone())
        .await?;

    if !cmd.is_empty() {
        ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
        // Run EXECUTE for any remaining handler-based actions
        let exec_ctx = tool_phase_context(ctx, transcript, Phase::BeforeToolExecute, call);
        let exec_ctx = apply_resume_context(exec_ctx, resume_state.as_ref());
        ctx.runtime
            .run_execute_loop(&ctx.agent.env, exec_ctx)
            .await?;
    }
    Ok(())
}

struct AppliedIntercept {
    blocked_reason: Option<String>,
    suspended: bool,
}

async fn apply_intercept_payload(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    call: &ToolCall,
    payload: ToolInterceptPayload,
) -> Result<AppliedIntercept, AgentLoopError> {
    match payload {
        ToolInterceptPayload::Block { reason } => {
            let result = awaken_contract::contract::tool::ToolResult::error(&call.name, &reason);
            let cmd = build_tool_state_command(
                ctx,
                transcript,
                call,
                &result,
                StateCommand::new(),
                ToolCallOutcome::Failed,
            )
            .await?;
            ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
            emit_tool_completion(ctx, transcript, call, &result, ToolCallOutcome::Failed).await;
            Ok(AppliedIntercept {
                blocked_reason: Some(reason),
                suspended: false,
            })
        }
        ToolInterceptPayload::Suspend(ticket) => {
            let cmd = build_suspend_state_command(call, &ticket);
            ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
            emit_suspend_completion(ctx, transcript, call, &ticket).await;
            Ok(AppliedIntercept {
                blocked_reason: None,
                suspended: true,
            })
        }
        ToolInterceptPayload::SetResult(result) => {
            let outcome = ToolCallOutcome::from_tool_result(&result);
            complete_tool_call(ctx, transcript, call, &result, StateCommand::new(), outcome)
                .await?;
            Ok(AppliedIntercept {
                blocked_reason: None,
                suspended: outcome == ToolCallOutcome::Suspended,
            })
        }
    }
}

/// Build a StateCommand for a completed tool call.
///
/// Merges three sources of actions into a single command:
/// 1. Lifecycle state update (ToolCallStates)
/// 2. Tool's own side-effects (returned via `ToolOutput.command`)
/// 3. AfterToolExecute plugin hook commands
///
/// Pure state construction — no side effects (no events, no messages, no commit).
async fn build_tool_state_command(
    ctx: &mut StepContext<'_>,
    transcript: &ToolBatchTranscript,
    call: &ToolCall,
    tool_result: &awaken_contract::contract::tool::ToolResult,
    tool_command: StateCommand,
    outcome: ToolCallOutcome,
) -> Result<StateCommand, AgentLoopError> {
    let store = ctx.runtime.store();
    let resume_state = active_resume_state(store, &call.id);
    let terminal_status = match outcome {
        ToolCallOutcome::Suspended => ToolCallStatus::Suspended,
        ToolCallOutcome::Succeeded => ToolCallStatus::Succeeded,
        ToolCallOutcome::Failed => ToolCallStatus::Failed,
    };
    let resume_mode = tool_result
        .suspension
        .as_ref()
        .map(|t| t.resume_mode)
        .or_else(|| resume_state.as_ref().map(|state| state.resume_mode))
        .unwrap_or_default();

    let mut cmd = tool_call_state_cmd(call, ToolCallStatus::Running);
    let mut next_state = ToolCallState::new(
        call.id.clone(),
        call.name.clone(),
        call.arguments.clone(),
        terminal_status,
        now_ms(),
    )
    .with_resume_mode(resume_mode);
    if let Some(ticket) = tool_result.suspension.as_ref() {
        next_state = next_state.with_suspension(
            Some(ticket.suspension.id.clone()),
            Some(ticket.suspension.action.clone()),
        );
    }
    cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Put(next_state));

    // Merge tool's own side-effects (same mechanism as plugin hooks)
    if !tool_command.is_empty() {
        cmd.extend(tool_command)?;
    }

    // Collect AfterToolExecute hook commands (same as plugin gather)
    let after_ctx = apply_resume_context(
        tool_phase_context(ctx, transcript, Phase::AfterToolExecute, call)
            .with_tool_result(tool_result.clone()),
        resume_state.as_ref(),
    );
    let after_cmd = ctx
        .runtime
        .collect_commands(&ctx.agent.env, after_ctx)
        .await?;
    if !after_cmd.is_empty() {
        cmd.extend(after_cmd)?;
    }
    Ok(cmd)
}

/// Emit events and append message for a completed tool call.
///
/// Side-effect only — no state mutation.
async fn emit_tool_completion(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    call: &ToolCall,
    tool_result: &awaken_contract::contract::tool::ToolResult,
    outcome: ToolCallOutcome,
) {
    let resume_state = active_resume_state(ctx.runtime.store(), &call.id);
    tracing::info!(
        tool_name = %call.name,
        call_id = %call.id,
        outcome = ?outcome,
        "tool_call_done"
    );

    let event = if resume_state.is_some() && outcome != ToolCallOutcome::Suspended {
        AgentEvent::ToolCallResumed {
            target_id: call.id.clone(),
            result: super::tool_result_to_resume_payload(tool_result),
        }
    } else {
        AgentEvent::ToolCallDone {
            id: call.id.clone(),
            message_id: String::new(),
            result: tool_result.clone(),
            outcome,
        }
    };
    ctx.sink.emit(event).await;

    let tool_content = tool_result_to_content(tool_result);
    transcript.push_tool_message(Arc::new(Message::tool(&call.id, tool_content)));
}

/// Complete a single tool call: build state, emit events, commit.
///
/// Convenience wrapper for the interception pipeline and incremental executor
/// where each tool commits individually.
async fn complete_tool_call(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    call: &ToolCall,
    tool_result: &awaken_contract::contract::tool::ToolResult,
    tool_command: StateCommand,
    outcome: ToolCallOutcome,
) -> Result<(), AgentLoopError> {
    let cmd =
        build_tool_state_command(ctx, transcript, call, tool_result, tool_command, outcome).await?;
    emit_tool_completion(ctx, transcript, call, tool_result, outcome).await;
    ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
    Ok(())
}

/// Build a suspend-only StateCommand (no AfterToolExecute — runs on resume).
fn build_suspend_state_command(call: &ToolCall, ticket: &SuspendTicket) -> StateCommand {
    let mut cmd = StateCommand::new();
    cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Put(
        ToolCallState::new(
            call.id.clone(),
            call.name.clone(),
            call.arguments.clone(),
            ToolCallStatus::Suspended,
            now_ms(),
        )
        .with_resume_mode(ticket.resume_mode)
        .with_suspension(
            Some(ticket.suspension.id.clone()),
            Some(ticket.suspension.action.clone()),
        ),
    ));
    cmd
}

/// Emit suspend-related events and append message.
async fn emit_suspend_completion(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    call: &ToolCall,
    ticket: &SuspendTicket,
) {
    // Emit ToolCallDone(Pending) so protocol encoders can signal the
    // frontend that a tool call is awaiting a decision.
    // - AG-UI: Pending → no event (TOOL_CALL_END was already sent)
    // - AI SDK: Pending → tool_approval_request for permission tools
    //
    // For UseDecisionAsToolResult (frontend tools), the frontend already
    // knows about the tool call from TOOL_CALL_START/ARGS/END events and
    // renders its own input UI (e.g. color picker). The ToolCallDone(Pending)
    // is still emitted for consistency — encoders that don't need it return
    // an empty Vec.
    let _ = ticket; // all modes emit the event now
    let suspend_result = awaken_contract::contract::tool::ToolResult::suspended_with(
        &call.name,
        format!("Tool '{}' suspended: awaiting approval", call.name),
        ticket.clone(),
    );
    ctx.sink
        .emit(AgentEvent::ToolCallDone {
            id: call.id.clone(),
            message_id: String::new(),
            result: suspend_result,
            outcome: ToolCallOutcome::Suspended,
        })
        .await;
    transcript.push_tool_message(Arc::new(Message::tool(
        &call.id,
        format!("Tool '{}' suspended: awaiting decision", call.name),
    )));
}

async fn complete_interrupted_tool_call(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    call: &ToolCall,
) -> Result<(), AgentLoopError> {
    let result =
        awaken_contract::contract::tool::ToolResult::error(&call.name, INTERRUPTED_TOOL_MESSAGE);
    let mut cmd = StateCommand::new();
    cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Put(ToolCallState::new(
        call.id.clone(),
        call.name.clone(),
        call.arguments.clone(),
        ToolCallStatus::Failed,
        now_ms(),
    )));
    emit_tool_completion(ctx, transcript, call, &result, ToolCallOutcome::Failed).await;
    ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
    Ok(())
}

async fn backfill_interrupted_tool_calls(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    calls: &[ToolCall],
) -> Result<(), AgentLoopError> {
    for call in calls {
        complete_interrupted_tool_call(ctx, transcript, call).await?;
    }
    Ok(())
}

struct ReadyExecutionOutcome {
    suspended: bool,
    processed_calls: usize,
}

async fn run_before_tool_execute_batch(
    ctx: &mut StepContext<'_>,
    transcript: &ToolBatchTranscript,
    calls: &[ToolCall],
) -> Result<(), AgentLoopError> {
    for call in calls {
        run_before_tool_execute(ctx, transcript, call).await?;
    }
    Ok(())
}

async fn execute_ready_tool_calls(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    allowed_calls: &[ToolCall],
) -> Result<ReadyExecutionOutcome, AgentLoopError> {
    let store = ctx.runtime.store();

    if allowed_calls.is_empty() {
        return Ok(ReadyExecutionOutcome {
            suspended: false,
            processed_calls: 0,
        });
    }

    let base_tool_ctx = ToolCallContext {
        call_id: String::new(),
        tool_name: String::new(),
        run_identity: ctx.run_identity.clone(),
        agent_spec: ctx.agent.spec.clone(),
        snapshot: store.snapshot(),
        activity_sink: Some(ctx.sink.clone()),
        cancellation_token: ctx.cancellation_token.cloned(),
        resume_input: None,
        suspension_id: None,
        suspension_reason: None,
    };

    let mut suspended = false;
    let mut processed_calls = 0usize;
    let requires_resume_context = allowed_calls
        .iter()
        .any(|call| active_resume_state(store, &call.id).is_some());

    if ctx.agent.tool_executor.requires_incremental_state() {
        for call in allowed_calls {
            run_before_tool_execute_batch(ctx, transcript, std::slice::from_ref(call)).await?;
            let resume_state = active_resume_state(store, &call.id);
            let mut tool_ctx = base_tool_ctx.clone();
            tool_ctx.call_id = call.id.clone();
            tool_ctx.tool_name = call.name.clone();
            tool_ctx.snapshot = store.snapshot();
            tool_ctx = apply_tool_resume_context(tool_ctx, resume_state.as_ref());

            let mut batch = ctx
                .agent
                .tool_executor
                .execute(&ctx.agent.tools, std::slice::from_ref(call), &tool_ctx)
                .await
                .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;
            let Some(exec_result) = batch.pop() else {
                continue;
            };

            let outcome = exec_result.outcome;
            complete_tool_call(
                ctx,
                transcript,
                &exec_result.call,
                &exec_result.result,
                exec_result.command,
                outcome,
            )
            .await?;
            processed_calls += 1;

            if outcome == ToolCallOutcome::Suspended {
                suspended = true;
                break;
            }
        }
    } else if requires_resume_context {
        let mut index = 0;
        while index < allowed_calls.len() {
            let call = &allowed_calls[index];
            let resume_state = active_resume_state(store, &call.id);

            if let Some(resume_state) = resume_state.as_ref() {
                run_before_tool_execute_batch(ctx, transcript, std::slice::from_ref(call)).await?;
                let mut tool_ctx = base_tool_ctx.clone();
                tool_ctx.call_id = call.id.clone();
                tool_ctx.tool_name = call.name.clone();
                tool_ctx.snapshot = store.snapshot();
                tool_ctx = apply_tool_resume_context(tool_ctx, Some(resume_state));

                let mut batch = ctx
                    .agent
                    .tool_executor
                    .execute(&ctx.agent.tools, std::slice::from_ref(call), &tool_ctx)
                    .await
                    .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;
                let Some(exec_result) = batch.pop() else {
                    index += 1;
                    continue;
                };

                let outcome = exec_result.outcome;
                complete_tool_call(
                    ctx,
                    transcript,
                    &exec_result.call,
                    &exec_result.result,
                    exec_result.command,
                    outcome,
                )
                .await?;
                processed_calls += 1;

                if outcome == ToolCallOutcome::Suspended {
                    suspended = true;
                    break;
                }

                index += 1;
                continue;
            }

            let segment_start = index;
            while index < allowed_calls.len()
                && active_resume_state(store, &allowed_calls[index].id).is_none()
            {
                index += 1;
            }
            let segment = &allowed_calls[segment_start..index];
            run_before_tool_execute_batch(ctx, transcript, segment).await?;
            let mut segment_ctx = base_tool_ctx.clone();
            segment_ctx.snapshot = store.snapshot();

            let exec_results = ctx
                .agent
                .tool_executor
                .execute(&ctx.agent.tools, segment, &segment_ctx)
                .await
                .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

            let mut segment_suspended = false;
            for exec_result in exec_results {
                let outcome = exec_result.outcome;
                complete_tool_call(
                    ctx,
                    transcript,
                    &exec_result.call,
                    &exec_result.result,
                    exec_result.command,
                    outcome,
                )
                .await?;
                processed_calls += 1;
                if outcome == ToolCallOutcome::Suspended {
                    suspended = true;
                    segment_suspended = true;
                }
            }

            if segment_suspended {
                break;
            }
        }
    } else {
        run_before_tool_execute_batch(ctx, transcript, allowed_calls).await?;
        let exec_results = ctx
            .agent
            .tool_executor
            .execute(&ctx.agent.tools, allowed_calls, &base_tool_ctx)
            .await
            .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

        for exec_result in exec_results {
            let outcome = exec_result.outcome;
            complete_tool_call(
                ctx,
                transcript,
                &exec_result.call,
                &exec_result.result,
                exec_result.command,
                outcome,
            )
            .await?;
            processed_calls += 1;
            if outcome == ToolCallOutcome::Suspended {
                suspended = true;
            }
        }
    }

    Ok(ReadyExecutionOutcome {
        suspended,
        processed_calls,
    })
}

struct AllowedExecutionOutcome {
    blocked_reason: Option<String>,
    suspended: bool,
    processed_calls: usize,
}

async fn execute_allowed_tool_calls(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    allowed_calls: &[ToolCall],
) -> Result<AllowedExecutionOutcome, AgentLoopError> {
    let batch = execute_ready_tool_calls(ctx, transcript, allowed_calls).await?;
    Ok(AllowedExecutionOutcome {
        blocked_reason: None,
        suspended: batch.suspended,
        processed_calls: batch.processed_calls,
    })
}

/// Execute tool calls with interception pipeline.
///
/// Each tool call runs its full lifecycle serially (before → execute → after),
/// producing a StateCommand. All commands are committed per-tool as each
/// completes — same model as plugin hooks but with per-tool commit for
/// checkpoint durability.
///
/// Returns (block_reason, any_suspended).
pub(super) async fn execute_tools_with_interception(
    ctx: &mut StepContext<'_>,
    transcript: &mut ToolBatchTranscript,
    calls: &[ToolCall],
) -> Result<(Option<String>, bool), AgentLoopError> {
    let mut suspended = false;
    let mut allowed_calls: Vec<ToolCall> = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        let mut intercept = run_tool_gate(ctx, transcript, call).await?;

        if intercept.is_some() && !allowed_calls.is_empty() {
            let batch = execute_allowed_tool_calls(ctx, transcript, &allowed_calls).await?;
            let interrupted_allowed = allowed_calls[batch.processed_calls..].to_vec();
            allowed_calls.clear();

            if let Some(reason) = batch.blocked_reason {
                backfill_interrupted_tool_calls(ctx, transcript, &calls[index..]).await?;
                return Ok((Some(reason), suspended));
            }

            if batch.suspended {
                suspended = true;
                backfill_interrupted_tool_calls(ctx, transcript, &interrupted_allowed).await?;
                backfill_interrupted_tool_calls(ctx, transcript, &calls[index..]).await?;
                return Ok((None, suspended));
            }

            intercept = run_tool_gate(ctx, transcript, call).await?;
        }

        if let Some(payload) = intercept {
            let applied = apply_intercept_payload(ctx, transcript, call, payload).await?;
            if let Some(reason) = applied.blocked_reason {
                backfill_interrupted_tool_calls(ctx, transcript, &calls[index + 1..]).await?;
                return Ok((Some(reason), suspended));
            }
            if applied.suspended {
                suspended = true;
            }
        } else {
            allowed_calls.push(call.clone());
        }
    }

    if !allowed_calls.is_empty() {
        let batch = execute_allowed_tool_calls(ctx, transcript, &allowed_calls).await?;
        if let Some(reason) = batch.blocked_reason {
            return Ok((Some(reason), suspended));
        }
        if batch.suspended {
            suspended = true;
            backfill_interrupted_tool_calls(
                ctx,
                transcript,
                &allowed_calls[batch.processed_calls..],
            )
            .await?;
        }
    }

    Ok((None, suspended))
}

/// Execute a single step of the agent loop: inference + tool execution + checkpoint.
pub(super) async fn execute_step(ctx: &mut StepContext<'_>) -> Result<StepOutcome, AgentLoopError> {
    let store = ctx.runtime.store();

    // --- Cancellation check ---
    if ctx.cancellation_token.is_some_and(|t| t.is_cancelled()) {
        commit_update::<RunLifecycle>(
            store,
            RunLifecycleUpdate::Done {
                done_reason: "cancelled".into(),
                updated_at: now_ms(),
            },
        )?;
        return Ok(StepOutcome::Cancelled);
    }

    // --- Phase hooks: StepStart ---
    if let Some(outcome) = run_phase_and_check(ctx, Phase::StepStart).await? {
        return Ok(outcome);
    }

    // --- Phase hooks: BeforeInference (collect_commands + extract actions) ---
    let (termination, overrides, exclusion_payloads, inclusion_payloads) =
        run_before_inference(ctx).await?;
    if let Some(outcome) = termination {
        return Ok(outcome);
    }

    // --- Inference ---
    let inference =
        run_inference_phase(ctx, overrides, exclusion_payloads, inclusion_payloads).await?;
    let InferencePhaseOutput {
        stream_result,
        duration_ms,
        upstream_model,
    } = inference;

    // --- Post-inference cancellation check ---
    if ctx.cancellation_token.is_some_and(|t| t.is_cancelled()) {
        ctx.sink
            .emit(AgentEvent::InferenceComplete {
                model: upstream_model.clone(),
                usage: stream_result.usage.clone(),
                duration_ms,
            })
            .await;
        commit_update::<RunLifecycle>(
            store,
            RunLifecycleUpdate::Done {
                done_reason: "cancelled".into(),
                updated_at: now_ms(),
            },
        )?;
        return Ok(StepOutcome::Cancelled);
    }
    ctx.sink
        .emit(AgentEvent::InferenceComplete {
            model: upstream_model,
            usage: stream_result.usage.clone(),
            duration_ms,
        })
        .await;

    // --- AfterInference phase ---
    let llm_response = LLMResponse::success(stream_result.clone());
    let after_inf_ctx = make_ctx(
        Phase::AfterInference,
        ctx.messages,
        ctx.run_identity,
        store,
        ctx.cancellation_token,
    )
    .with_llm_response(llm_response);
    match ctx
        .runtime
        .run_phase_with_context(&ctx.agent.env, after_inf_ctx)
        .await
    {
        Ok(_) => {}
        Err(awaken_contract::StateError::Cancelled) => return Ok(StepOutcome::Cancelled),
        Err(e) => return Err(e.into()),
    }
    if let Some(reason) = check_termination(store) {
        return Ok(StepOutcome::Terminated(reason));
    }

    // --- No tools needed: natural end ---
    if !stream_result.needs_tools() {
        ctx.messages
            .push(Arc::new(Message::assistant(stream_result.text())));
        complete_step(StepCompletion {
            store,
            runtime: ctx.runtime,
            env: &ctx.agent.env,
            sink: ctx.sink.as_ref(),
            checkpoint_store: ctx.checkpoint_store,
            messages: ctx.messages,
            run_identity: ctx.run_identity,
            run_created_at: ctx.run_created_at,
            total_input_tokens: *ctx.total_input_tokens,
            total_output_tokens: *ctx.total_output_tokens,
        })
        .await?;
        return Ok(StepOutcome::NaturalEnd);
    }

    // --- Tool calls ---
    let mut transcript =
        ToolBatchTranscript::for_inference(stream_result.text(), stream_result.tool_calls.clone());

    // Intercept + execute tool calls via unified pipeline.
    // Messages stay step-local until the batch has complete visible outputs.
    let (blocked_reason, suspended) =
        execute_tools_with_interception(ctx, &mut transcript, &stream_result.tool_calls).await?;
    transcript.commit_into(ctx.messages);

    if let Some(reason) = blocked_reason {
        commit_update::<RunLifecycle>(
            store,
            RunLifecycleUpdate::Done {
                done_reason: format!("blocked:{reason}"),
                updated_at: now_ms(),
            },
        )?;
        return Ok(StepOutcome::Blocked(reason));
    }

    if let Some(reason) = check_termination(store) {
        return Ok(StepOutcome::Terminated(reason));
    }

    if suspended {
        return Ok(StepOutcome::Suspended);
    }

    Ok(StepOutcome::Continue)
}
