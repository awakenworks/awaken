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
use awaken_contract::contract::suspension::{ToolCallOutcome, ToolCallResumeMode, ToolCallStatus};
use awaken_contract::contract::tool::ToolCallContext;
use awaken_contract::model::Phase;

use super::actions::{
    apply_context_messages, apply_tool_filter_payloads, extract_actions, merge_override_payloads,
    resolve_intercept_payloads, take_context_messages,
};
use super::checkpoint::{check_termination, complete_step};
use super::inference::{compact_with_llm, execute_streaming};
use super::{AgentLoopError, commit_update, now_ms, tool_result_to_content};
use crate::agent::state::{ExcludeTool, IncludeOnlyTools, SetInferenceOverride};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};
use awaken_contract::contract::tool_intercept::ToolInterceptAction;

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

/// Build a `StateCommand` that upserts a `ToolCallStates` entry for a given call and status.
fn tool_call_state_cmd(call: &ToolCall, status: ToolCallStatus) -> StateCommand {
    let mut cmd = StateCommand::new();
    cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        status,
        updated_at: now_ms(),
        resume_mode: ToolCallResumeMode::default(),
    });
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

        let cont_request = InferenceRequest {
            model: ctx.agent.model.clone(),
            messages: cont_messages,
            tools: ctx.agent.tool_descriptors(),
            system: vec![],
            overrides: ctx.run_overrides.clone(),
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

/// Run the BeforeInference phase via collect_commands, extract loop-consumed
/// action payloads, submit remaining command, and run EXECUTE for AddContextMessage.
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
    let mut cmd = ctx
        .runtime
        .collect_commands(&ctx.agent.env, phase_ctx.clone())
        .await?;

    // Extract known action payloads directly from the command
    let override_payloads = extract_actions::<SetInferenceOverride>(&mut cmd.scheduled_actions);
    let exclusion_payloads = extract_actions::<ExcludeTool>(&mut cmd.scheduled_actions);
    let inclusion_payloads = extract_actions::<IncludeOnlyTools>(&mut cmd.scheduled_actions);

    // Commit remaining state mutations + unrecognized actions (including AddContextMessage)
    if !cmd.is_empty() {
        ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
    }

    // Run EXECUTE loop for remaining actions (e.g. AddContextMessage handler)
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

    // Merge override payloads with run-level overrides
    let mut overrides = ctx.run_overrides.clone();
    merge_override_payloads(&mut overrides, override_payloads);

    Ok((
        termination,
        overrides,
        exclusion_payloads,
        inclusion_payloads,
    ))
}

/// Run the inference phase: compaction, request building, streaming.
/// Returns the stream result and wall-clock duration in milliseconds.
async fn run_inference_phase(
    ctx: &mut StepContext<'_>,
    overrides: Option<InferenceOverride>,
    exclusion_payloads: Vec<String>,
    inclusion_payloads: Vec<Vec<String>>,
) -> Result<(StreamResult, u64), AgentLoopError> {
    let store = ctx.runtime.store();

    // LLM compaction
    if let Some(policy) = ctx.agent.context_policy()
        && let Some(threshold) = policy.autocompact_threshold
    {
        let token_est = awaken_contract::contract::transform::estimate_tokens_arc(ctx.messages);
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
    let request = InferenceRequest {
        model: ctx.agent.model.clone(),
        messages: request_messages,
        tools,
        system: vec![],
        overrides,
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
    let stream_result = recover_truncation(ctx, stream_result, &transform_arcs).await?;

    let duration_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        model = %ctx.agent.model,
        input_tokens = *ctx.total_input_tokens,
        output_tokens = *ctx.total_output_tokens,
        duration_ms,
        "inference_complete"
    );

    Ok((stream_result, duration_ms))
}

/// Run BeforeToolExecute phase via collect_commands and check for intercept.
///
/// Returns `Some(payload)` if any hook scheduled a `ToolInterceptAction`,
/// `None` if the tool should execute normally.
async fn intercept_tool_call(
    ctx: &mut StepContext<'_>,
    call: &ToolCall,
) -> Result<Option<awaken_contract::contract::tool_intercept::ToolInterceptPayload>, AgentLoopError>
{
    let store = ctx.runtime.store();
    let before_ctx = make_ctx(
        Phase::BeforeToolExecute,
        ctx.messages,
        ctx.run_identity,
        store,
        ctx.cancellation_token,
    )
    .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()));

    // GATHER only — extract intercept actions directly
    let mut cmd = ctx
        .runtime
        .collect_commands(&ctx.agent.env, before_ctx.clone())
        .await?;
    let intercept_payloads = extract_actions::<ToolInterceptAction>(&mut cmd.scheduled_actions);

    // Commit remaining state mutations + other actions
    if !cmd.is_empty() {
        ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
        // Run EXECUTE for any remaining handler-based actions
        let exec_ctx = make_ctx(
            Phase::BeforeToolExecute,
            ctx.messages,
            ctx.run_identity,
            store,
            ctx.cancellation_token,
        )
        .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()));
        ctx.runtime
            .run_execute_loop(&ctx.agent.env, exec_ctx)
            .await?;
    }

    // Resolve winning intercept from extracted payloads
    Ok(resolve_intercept_payloads(intercept_payloads))
}

/// Complete a tool call: update lifecycle state, emit event via Effect,
/// run AfterToolExecute phase, and append tool message.
///
/// Handles the complete lifecycle: state transition, event emission,
/// AfterToolExecute phase hooks, and message append.
async fn complete_tool_call(
    ctx: &mut StepContext<'_>,
    call: &ToolCall,
    tool_result: &awaken_contract::contract::tool::ToolResult,
    outcome: ToolCallOutcome,
) -> Result<(), AgentLoopError> {
    let store = ctx.runtime.store();
    let terminal_status = match outcome {
        ToolCallOutcome::Suspended => ToolCallStatus::Suspended,
        ToolCallOutcome::Succeeded => ToolCallStatus::Succeeded,
        ToolCallOutcome::Failed => ToolCallStatus::Failed,
    };

    // State transition: Running → terminal status
    let mut lifecycle_cmd = tool_call_state_cmd(call, ToolCallStatus::Running);
    lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        status: terminal_status,
        updated_at: now_ms(),
        resume_mode: ToolCallResumeMode::default(),
    });

    tracing::info!(
        tool_name = %call.name,
        call_id = %call.id,
        outcome = ?outcome,
        "tool_call_done"
    );

    // Event emission
    ctx.sink
        .emit(AgentEvent::ToolCallDone {
            id: call.id.clone(),
            message_id: String::new(),
            result: tool_result.clone(),
            outcome,
        })
        .await;

    // AfterToolExecute phase
    let after_ctx = make_ctx(
        Phase::AfterToolExecute,
        ctx.messages,
        ctx.run_identity,
        store,
        ctx.cancellation_token,
    )
    .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()))
    .with_tool_result(tool_result.clone());
    let after_cmd = ctx
        .runtime
        .collect_commands(&ctx.agent.env, after_ctx)
        .await?;
    if !after_cmd.is_empty() {
        lifecycle_cmd.extend(after_cmd)?;
    }

    // Commit state
    ctx.runtime
        .submit_command(&ctx.agent.env, lifecycle_cmd)
        .await?;

    // Append tool message to conversation
    let tool_content = tool_result_to_content(tool_result);
    ctx.messages
        .push(Arc::new(Message::tool(&call.id, tool_content)));

    Ok(())
}

/// Execute tool calls with interception pipeline.
///
/// For each tool call:
/// 1. Run BeforeToolExecute phase (hooks + action handlers)
/// 2. Check for intercept (Block/Suspend/SetResult)
/// 3. If no intercept: execute the tool normally
/// 4. Run AfterToolExecute phase
///
/// Returns whether any call was suspended.
async fn execute_tools_with_interception(
    ctx: &mut StepContext<'_>,
    calls: &[ToolCall],
) -> Result<(Option<String>, bool), AgentLoopError> {
    use awaken_contract::contract::tool_intercept::ToolInterceptPayload;

    let store = ctx.runtime.store();
    let mut suspended = false;
    let mut allowed_calls: Vec<ToolCall> = Vec::new();

    for call in calls {
        // --- Interception check ---
        let intercept = intercept_tool_call(ctx, call).await?;

        match intercept {
            Some(ToolInterceptPayload::Block { reason }) => {
                // Block: result determined (failure), run full lifecycle including AfterToolExecute
                let result =
                    awaken_contract::contract::tool::ToolResult::error(&call.name, &reason);
                complete_tool_call(ctx, call, &result, ToolCallOutcome::Failed).await?;
                return Ok((Some(reason), suspended));
            }
            Some(ToolInterceptPayload::Suspend(ticket)) => {
                // Suspend: result NOT yet determined — mark as suspended, do NOT run AfterToolExecute.
                // AfterToolExecute runs later when resume/cancel decision arrives.
                let mut cmd = StateCommand::new();
                cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    status: ToolCallStatus::Suspended,
                    updated_at: now_ms(),
                    resume_mode: ticket.resume_mode,
                });
                ctx.runtime.submit_command(&ctx.agent.env, cmd).await?;
                ctx.messages.push(Arc::new(Message::tool(
                    &call.id,
                    format!("Tool '{}' suspended: awaiting decision", call.name),
                )));
                suspended = true;
                continue;
            }
            Some(ToolInterceptPayload::SetResult(result)) => {
                // SetResult: result determined (externally), run full lifecycle including AfterToolExecute
                let outcome = ToolCallOutcome::from_tool_result(&result);
                complete_tool_call(ctx, call, &result, outcome).await?;
                if outcome == ToolCallOutcome::Suspended {
                    suspended = true;
                }
                continue;
            }
            None => {
                // No intercept — execute tool normally
            }
        }

        // Collect calls that passed interception for execution
        allowed_calls.push(call.clone());
    }

    // Execute allowed calls via the configured executor strategy
    if allowed_calls.is_empty() {
        return Ok((None, suspended));
    }

    let base_tool_ctx = ToolCallContext {
        call_id: String::new(),
        tool_name: String::new(),
        run_identity: ctx.run_identity.clone(),
        agent_spec: ctx.agent.spec.clone(),
        snapshot: store.snapshot(),
        activity_sink: Some(ctx.sink.clone()),
    };

    // Incremental-state executors: execute one-by-one, submit state after each
    if ctx.agent.tool_executor.requires_incremental_state() {
        for call in &allowed_calls {
            let mut tool_ctx = base_tool_ctx.clone();
            tool_ctx.call_id = call.id.clone();
            tool_ctx.tool_name = call.name.clone();
            tool_ctx.snapshot = store.snapshot();

            let batch = ctx
                .agent
                .tool_executor
                .execute(&ctx.agent.tools, std::slice::from_ref(call), &tool_ctx)
                .await
                .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;
            let Some(exec_result) = batch.first() else {
                continue;
            };
            let is_suspended = exec_result.outcome == ToolCallOutcome::Suspended;

            complete_tool_call(
                ctx,
                &exec_result.call,
                &exec_result.result,
                exec_result.outcome,
            )
            .await?;

            if is_suspended {
                suspended = true;
                break;
            }
        }
        return Ok((None, suspended));
    }

    // Parallel executors: batch execute, then process results
    let exec_results = ctx
        .agent
        .tool_executor
        .execute(&ctx.agent.tools, &allowed_calls, &base_tool_ctx)
        .await
        .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

    for exec_result in &exec_results {
        complete_tool_call(
            ctx,
            &exec_result.call,
            &exec_result.result,
            exec_result.outcome,
        )
        .await?;
        if exec_result.outcome == ToolCallOutcome::Suspended {
            suspended = true;
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
    let (stream_result, duration_ms) =
        run_inference_phase(ctx, overrides, exclusion_payloads, inclusion_payloads).await?;

    // --- Post-inference cancellation check ---
    if ctx.cancellation_token.is_some_and(|t| t.is_cancelled()) {
        ctx.sink
            .emit(AgentEvent::InferenceComplete {
                model: ctx.agent.model.clone(),
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
            model: ctx.agent.model.clone(),
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
        complete_step(
            store,
            ctx.runtime,
            &ctx.agent.env,
            ctx.sink.as_ref(),
            ctx.checkpoint_store,
            ctx.messages,
            ctx.run_identity,
            ctx.run_created_at,
            *ctx.total_input_tokens,
            *ctx.total_output_tokens,
        )
        .await?;
        return Ok(StepOutcome::NaturalEnd);
    }

    // --- Tool calls ---
    ctx.messages
        .push(Arc::new(Message::assistant_with_tool_calls(
            stream_result.text(),
            stream_result.tool_calls.clone(),
        )));

    // Intercept + execute tool calls via unified pipeline
    let (blocked_reason, suspended) =
        execute_tools_with_interception(ctx, &stream_result.tool_calls).await?;

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
