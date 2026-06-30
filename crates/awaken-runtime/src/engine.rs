//! The async agent execution loop.
//!
//! `execute` resolves the activation, runs the model/tool phase loop, stages a
//! `ThreadCommit`, and commits durable truth. A run can park on a gate `Suspend`
//! by committing a `WaitingTicket`, then `resume` validates a `ResumeCommand`
//! against it and continues. Live progress is best-effort and never the replay
//! source (G1/G13).

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, validate_batch};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{PendingTool, WaitingReason, WaitingTicket};
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft as EventDraft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::event::{Event as StreamEvent, Kind as StreamKind};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
use awaken_runtime_contract::llm::{
    ChatMessage, ChatRequest, ChatRole, DeltaSink, ToolCall, ToolSchema,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext};
use awaken_runtime_contract::plugin::{PhaseContext, PhaseHookPoint, ResolvedExecutionEnv};
use awaken_runtime_contract::resolved::{ResolvedRun, ResolvedSpec, ToolDescriptor};
use awaken_runtime_contract::resolver::{self, RunResolver};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult, validate_resume};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
use awaken_runtime_contract::tool::ToolOutput;

use crate::runtime::Runtime;

/// Message-id base for messages produced by a resumed attempt, kept distinct
/// from the original attempt's ids.
const RESUME_STEP_BASE: usize = 1_000;

#[async_trait]
impl RunExecutor for Runtime {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        // Track this run's cancellation token so live control can steer it, and
        // always deregister on the way out.
        let run_id = activation.run_id.clone();
        if let Some(token) = &context.cancellation {
            self.register_run(&run_id, token.clone());
        }
        let result = run_agent_loop(self, activation, context).await;
        self.deregister_run(&run_id);
        result
    }
}

pub(crate) async fn run_agent_loop(
    runtime: &Runtime,
    activation: RunActivation,
    context: RuntimeRunContext,
) -> Result<Phase> {
    let resolved = runtime
        .resolve(&activation.snapshot)
        .map_err(map_resolver_error)?;

    let run_id = activation.run_id.clone();
    let thread_id = activation.thread_id.clone();

    // Cancellation observed before any model call: commit a terminal Cancelled
    // outcome instead of starting work.
    if context.is_cancelled() {
        return finish(&context, &thread_id, run_id, Checkpoint::cancelled()).await;
    }

    // Merge the active plugins under their capability bounds; a violation fails
    // the run closed before any model call (G30).
    let env = match runtime.resolve_plugin_env(&resolved.spec.plugin_ids) {
        Ok(env) => env,
        Err(_) => {
            return finish(&context, &thread_id, run_id, Checkpoint::capability_bound()).await;
        }
    };

    emit(&context, &run_id, StreamKind::RunStarted).await;

    let transcript = activation.input.clone();
    let checkpoint = drive(
        runtime,
        &resolved,
        &env,
        &run_id,
        &context,
        transcript,
        Vec::new(),
        0,
    )
    .await?;
    finalize(&context, &thread_id, run_id, checkpoint).await
}

/// Cancel a run that is not executing (a queued or parked run) by committing a
/// terminal `Cancelled` fact through the one finish boundary (G31). This clears
/// any waiting ticket, so a parked run can no longer be resumed. An in-flight run
/// is cancelled cooperatively through `LiveRunControl` instead, not here.
pub(crate) async fn cancel_run(
    run_id: RunId,
    thread_id: ThreadId,
    context: RuntimeRunContext,
) -> Result<Phase> {
    finish(&context, &thread_id, run_id, Checkpoint::cancelled()).await
}

/// Stop a run by committing a terminal `Stopped(reason)` fact through the one
/// finish boundary (G31) — a host stop policy (budget, step ceiling) making the
/// run terminal. Like cancel, it clears any waiting ticket, so a later resume or
/// scheduled result for the run fails closed (RS-CTRL-002, ADR-0026).
pub(crate) async fn stop_run(
    run_id: RunId,
    thread_id: ThreadId,
    reason: String,
    context: RuntimeRunContext,
) -> Result<Phase> {
    finish(&context, &thread_id, run_id, Checkpoint::stopped(reason)).await
}

/// Perform a committed `ScheduledAction` (ADR-0020): the run is parked on a
/// ticket whose reason is `ScheduledAction`, holding the deferred action as its
/// pending tool. Performing it is an allow-resume of that committed action — the
/// system runs the action and commits the resumed outcome, validated against the
/// committed request and idempotent (RS-SCH-001/004). A run parked for any other
/// reason, or not parked at all, fails closed.
pub(crate) async fn perform_scheduled_action(
    runtime: &Runtime,
    run_id: &RunId,
    reader: &dyn ThreadReader,
    context: RuntimeRunContext,
    now_ms: u64,
) -> Result<Phase> {
    let ticket = reader
        .waiting_ticket(run_id)
        .ok_or_else(|| Error::Execution("run is not waiting".to_string()))?;
    if ticket.reason != WaitingReason::ScheduledAction {
        return Err(Error::Execution(
            "run is not parked on a scheduled action".to_string(),
        ));
    }
    let command = ResumeCommand {
        correlation_id: ticket.correlation_id,
        run_id: ticket.run_id,
        thread_id: ticket.thread_id,
        snapshot_id: ticket.snapshot_id,
        catalog_fingerprint: ticket.catalog_fingerprint,
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
        now_ms,
    };
    resume_run(runtime, command, reader, context).await
}

/// Resume a parked run: validate the resume against the committed ticket, rebuild
/// the transcript from committed messages, inject the resumed result, and drive
/// the loop to a new terminal/parked state (G5/G28).
pub(crate) async fn resume_run(
    runtime: &Runtime,
    command: ResumeCommand,
    reader: &dyn ThreadReader,
    context: RuntimeRunContext,
) -> Result<Phase> {
    let ticket = reader
        .waiting_ticket(&command.run_id)
        .ok_or_else(|| Error::Execution("run is not waiting".to_string()))?;
    validate_resume(&ticket, &command).map_err(|err| Error::Execution(err.to_string()))?;

    let snapshot = runtime
        .snapshot_by_id(&ExecutableAgentSnapshotId(ticket.snapshot_id.clone()))
        .ok_or_else(|| Error::Resolution("snapshot for resume not found".to_string()))?;
    let resolved = runtime.resolve(&snapshot).map_err(map_resolver_error)?;

    let run_id = command.run_id.clone();
    let thread_id = command.thread_id.clone();

    let env = match runtime.resolve_plugin_env(&resolved.spec.plugin_ids) {
        Ok(env) => env,
        Err(_) => {
            return finish(&context, &thread_id, run_id, Checkpoint::capability_bound()).await;
        }
    };

    emit(&context, &run_id, StreamKind::RunStarted).await;

    // Rebuild the transcript from committed truth and apply the resumed result.
    let mut transcript = reader.committed_messages(&thread_id);
    let (resumed, seed_state) = resume_into_messages(runtime, &ticket, command.result).await;
    transcript.extend(resumed.iter().cloned());

    let mut checkpoint = drive(
        runtime,
        &resolved,
        &env,
        &run_id,
        &context,
        transcript,
        resumed,
        RESUME_STEP_BASE,
    )
    .await?;
    // State staged by the resumed tool itself is committed too (kept first).
    let mut combined = seed_state;
    combined.append(&mut checkpoint.staged_state);
    checkpoint.staged_state = combined;

    finalize(&context, &thread_id, run_id, checkpoint).await
}

/// Validate the staged state batch, then commit. A conflict fails closed: the
/// attempt becomes a `StateConflict` fault, drops any pause, and commits no
/// state (G13). A run whose state did not commit cleanly must not be resumable.
async fn finalize(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    checkpoint: Checkpoint,
) -> Result<Phase> {
    if !checkpoint.staged_state.is_empty() && validate_batch(&checkpoint.staged_state).is_err() {
        let failed = Checkpoint {
            new_messages: checkpoint.new_messages,
            staged_state: Vec::new(),
            end: End::Ended(EndCause::Error(Failure::StateConflict)),
        };
        return finish(context, thread_id, run_id, failed).await;
    }
    finish(context, thread_id, run_id, checkpoint).await
}

/// What one attempt at driving the loop resolved to, ready to commit: the new
/// messages, the staged state, and where the run goes next.
struct Checkpoint {
    new_messages: Vec<Message>,
    staged_state: Vec<StateCommand>,
    end: End,
}

/// The terminal decision of one attempt. A paused run carries its ticket here;
/// an ended run carries its cause. The two are mutually exclusive by
/// construction, so the committed [`Phase`] can never disagree with the ticket.
enum End {
    /// The run paused; the ticket is committed alongside the checkpoint.
    Parked(WaitingTicket),
    /// The run reached a terminus through one cause.
    Ended(EndCause),
}

impl Checkpoint {
    fn cancelled() -> Self {
        Self::ended(EndCause::Cancelled)
    }

    fn stopped(reason: String) -> Self {
        Self::ended(EndCause::Stopped(reason))
    }

    fn capability_bound() -> Self {
        Self::ended(EndCause::Error(Failure::CapabilityBound))
    }

    fn ended(cause: EndCause) -> Self {
        Self {
            new_messages: Vec::new(),
            staged_state: Vec::new(),
            end: End::Ended(cause),
        }
    }
}

/// Run the model/tool loop over a prepared transcript. Shared by fresh execution
/// and resume; the caller seeds the transcript and the already-produced messages.
#[allow(clippy::too_many_arguments)]
async fn drive(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    context: &RuntimeRunContext,
    mut transcript: Vec<Message>,
    mut new_messages: Vec<Message>,
    step_base: usize,
) -> Result<Checkpoint> {
    let llm = runtime.llm().ok_or_else(|| {
        Error::Execution("no model provider configured for this runtime".to_string())
    })?;

    let mut staged_state: Vec<StateCommand> = Vec::new();
    // The loop's terminal decision. It stays `None` only if the loop runs to its
    // step ceiling, which is itself a terminus (`MaxSteps`).
    let mut end: Option<End> = None;
    // Forwards streamed text chunks to the live stream during each inference.
    let delta_sink = StreamDeltaSink { context, run_id };

    // The agent's configured ceiling guards against a non-terminating tool cycle;
    // a natural-end text turn ends the loop earlier.
    for step in 0..resolved.spec.max_steps {
        if context.is_cancelled() {
            end = Some(End::Ended(EndCause::Cancelled));
            break;
        }

        // Phase hooks contribute state through the commit path only (G9/G30).
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepStart,
            &mut staged_state,
        )
        .await;
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::BeforeInference,
            &mut staged_state,
        )
        .await;

        let request = build_chat_request(&resolved.spec, &transcript);
        // A transient inference failure retries with backoff; a permanent failure
        // (or exhausted retries) commits a typed terminal reason (G26).
        let response =
            match infer_with_retry(llm, request, runtime.infer_retries(), &delta_sink).await {
                Ok(response) => response,
                Err(err) => {
                    end = Some(End::Ended(EndCause::Error(Failure::Inference(
                        err.to_string(),
                    ))));
                    break;
                }
            };

        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::AfterInference,
            &mut staged_state,
        )
        .await;

        // A cancel may have landed while inference was in flight; observe it at
        // this step boundary and discard the model output.
        if context.is_cancelled() {
            end = Some(End::Ended(EndCause::Cancelled));
            break;
        }

        // Commit the assistant turn verbatim — text and tool-use blocks may
        // interleave. The text already streamed to the live sink; the committed
        // message is the assembled whole.
        let calls = response.output.tool_calls();
        let assistant = assistant_message(run_id, step_base + step, response.output.blocks);
        transcript.push(assistant.clone());
        new_messages.push(assistant);

        // A text-only turn (no tool requests) is a natural end.
        if calls.is_empty() {
            end = Some(End::Ended(EndCause::NaturalEnd));
            break;
        }

        // Otherwise run each requested tool and feed the results back.
        for call in calls {
            let output = match gate_decision(runtime, &call).await {
                GateOutcome::Allow => execute_tool(runtime, &call).await,
                GateOutcome::Block { reason } => {
                    ToolOutput::error(&call.call_id, format!("blocked: {reason}"))
                }
                GateOutcome::SetResult(output) => output,
                GateOutcome::Suspend { ticket_id } => {
                    // Park the run on a structured ticket carrying the pending
                    // call so an allow decision can run it later.
                    let ticket = waiting_ticket(
                        resolved,
                        run_id,
                        &ticket_id,
                        &call,
                        WaitingReason::ToolPermission,
                    );
                    end = Some(End::Parked(ticket));
                    emit(
                        context,
                        run_id,
                        StreamKind::Waiting {
                            reason: "tool_permission".to_string(),
                        },
                    )
                    .await;
                    break;
                }
                GateOutcome::Schedule {
                    correlation_id,
                    action_kind,
                } => {
                    // A plugin-owned scheduled-action kind must be in the resolved
                    // environment; an absent kind (its plugin not selected) fails
                    // the run closed (RS-SCH-005, ADR-0027).
                    if let Some(kind) = &action_kind
                        && !env.permits_action_kind(kind)
                    {
                        end = Some(End::Ended(EndCause::Error(Failure::CapabilityBound)));
                        break;
                    }
                    // Commit a ScheduledAction (ADR-0020): the call is deferred and
                    // performed later from the committed request, not decided.
                    let ticket = waiting_ticket(
                        resolved,
                        run_id,
                        &correlation_id,
                        &call,
                        WaitingReason::ScheduledAction,
                    );
                    end = Some(End::Parked(ticket));
                    emit(
                        context,
                        run_id,
                        StreamKind::Waiting {
                            reason: "scheduled_action".to_string(),
                        },
                    )
                    .await;
                    break;
                }
            };
            staged_state.extend(output.state.clone());
            let message = tool_result_message(&call, &output);
            transcript.push(message.clone());
            new_messages.push(message);
        }

        if end.is_some() {
            break;
        }

        // StepEnd fires at the boundary between steps that continue the loop.
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepEnd,
            &mut staged_state,
        )
        .await;
    }

    // No early terminus means the loop exhausted its step ceiling.
    let end = end.unwrap_or(End::Ended(EndCause::MaxSteps));
    Ok(Checkpoint {
        new_messages,
        staged_state,
        end,
    })
}

/// Forwards a turn's streamed text chunks to the live stream as `OutputText`
/// events. Live progress only — the committed message comes from the returned
/// response, never these chunks (G10/G13).
struct StreamDeltaSink<'a> {
    context: &'a RuntimeRunContext,
    run_id: &'a RunId,
}

#[async_trait]
impl DeltaSink for StreamDeltaSink<'_> {
    async fn on_text(&self, chunk: &str) {
        emit(
            self.context,
            self.run_id,
            StreamKind::OutputText {
                text: chunk.to_string(),
            },
        )
        .await;
    }

    async fn on_tool_call(&self, call_id: &str, tool_id: &str, arguments: &serde_json::Value) {
        emit(
            self.context,
            self.run_id,
            StreamKind::ToolCall {
                call_id: call_id.to_string(),
                tool_id: tool_id.to_string(),
                arguments: arguments.clone(),
            },
        )
        .await;
    }
}

/// Call inference, streaming text chunks to `sink` as they arrive and retrying a
/// transient failure up to `retries` times with a short linear backoff. A
/// permanent failure returns immediately (G26).
async fn infer_with_retry(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    retries: usize,
    sink: &dyn DeltaSink,
) -> std::result::Result<
    awaken_runtime_contract::llm::ChatResponse,
    awaken_runtime_contract::llm::Error,
> {
    let mut attempt = 0;
    loop {
        match llm.infer_streaming(request.clone(), sink).await {
            Ok(response) => return Ok(response),
            Err(err) if err.is_retryable() && attempt < retries => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10 * attempt as u64)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Run every hook registered for one phase point, in dependency order, staging
/// their state commands. Hooks return data; they never write a store (G9/G30).
async fn run_phase_hooks(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step: usize,
    point: PhaseHookPoint,
    staged_state: &mut Vec<StateCommand>,
) {
    for hook in env.hooks_for(point) {
        let ctx = PhaseContext {
            run_id: run_id.clone(),
            step,
            point,
        };
        staged_state.extend(hook.on_phase(&ctx).await);
    }
}

/// Build the committed ticket for a parked tool call.
fn waiting_ticket(
    resolved: &ResolvedRun,
    run_id: &RunId,
    ticket_id: &str,
    call: &ToolCall,
    reason: WaitingReason,
) -> WaitingTicket {
    WaitingTicket {
        correlation_id: ticket_id.to_string(),
        run_id: run_id.clone(),
        thread_id: ThreadId(String::new()), // filled in finish via the commit thread id
        snapshot_id: resolved.snapshot_id.0.clone(),
        catalog_fingerprint: resolved.spec.catalog_fingerprint.0.clone(),
        reason,
        call_id: Some(call.call_id.clone()),
        pending_tool: Some(PendingTool {
            tool_id: call.tool_id.clone(),
            arguments: call.arguments.clone(),
        }),
        deadline_ms: None,
    }
}

/// Turn a resumed result into the tool/user message(s) and any staged state.
/// An `allow` decision executes the pending tool now; a `deny` feeds a blocked
/// result; a `ToolResult`/`Input` is used directly.
async fn resume_into_messages(
    runtime: &Runtime,
    ticket: &WaitingTicket,
    result: ResumeResult,
) -> (Vec<Message>, Vec<StateCommand>) {
    let call_id = ticket.call_id.clone().unwrap_or_default();
    match result {
        ResumeResult::ToolResult(output) => {
            let state = output.state.clone();
            (
                vec![tool_result_message_from(&call_id, &output.content)],
                state,
            )
        }
        ResumeResult::Decision { allow, note } => {
            if allow && let Some(pending) = &ticket.pending_tool {
                let call = ToolCall {
                    call_id: call_id.clone(),
                    tool_id: pending.tool_id.clone(),
                    arguments: pending.arguments.clone(),
                };
                let output = execute_tool(runtime, &call).await;
                let state = output.state.clone();
                (
                    vec![tool_result_message_from(&call_id, &output.content)],
                    state,
                )
            } else {
                let reason = note.unwrap_or_else(|| "denied".to_string());
                (
                    vec![tool_result_message_from(
                        &call_id,
                        &format!("blocked: {reason}"),
                    )],
                    Vec::new(),
                )
            }
        }
        ResumeResult::Input(text) => (
            vec![Message::text(
                MessageId(format!("resume-input-{call_id}")),
                Role::User,
                text,
            )],
            Vec::new(),
        ),
    }
}

/// Consult the permission gate; an absent gate allows (used only in tests).
async fn gate_decision(runtime: &Runtime, call: &ToolCall) -> GateOutcome {
    match runtime.gate() {
        Some(gate) => {
            let ctx = PermissionContext {
                tool_id: call.tool_id.clone(),
                call_id: call.call_id.clone(),
                arguments: call.arguments.clone(),
            };
            gate.gate(&ctx).await
        }
        None => GateOutcome::Allow,
    }
}

/// Invoke an authorized tool, turning a missing tool or a tool error into a
/// model-visible error result rather than aborting the run.
async fn execute_tool(runtime: &Runtime, call: &ToolCall) -> ToolOutput {
    match runtime.tool(&call.tool_id) {
        Some(tool) => match tool.invoke(call.clone()).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(&call.call_id, err.to_string()),
        },
        None => ToolOutput::error(&call.call_id, format!("unknown tool: {}", call.tool_id)),
    }
}

/// Build a model request from the resolved binding, transcript, and visible
/// tool descriptors.
pub(crate) fn build_chat_request(spec: &ResolvedSpec, transcript: &[Message]) -> ChatRequest {
    // The agent's instructions lead the request as a system message, ahead of the
    // transcript. Empty instructions contribute no system message.
    let mut messages = Vec::with_capacity(transcript.len() + 1);
    if !spec.instructions.is_empty() {
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: vec![ContentBlock::text(spec.instructions.clone())],
        });
    }
    messages.extend(transcript.iter().map(to_chat_message));
    ChatRequest {
        model_binding: spec.model_binding.clone(),
        messages,
        tools: spec.tool_descriptors.iter().map(to_tool_schema).collect(),
    }
}

fn to_chat_message(message: &Message) -> ChatMessage {
    ChatMessage {
        role: to_chat_role(&message.role),
        content: message.content.clone(),
    }
}

fn to_chat_role(role: &Role) -> ChatRole {
    match role {
        Role::System => ChatRole::System,
        Role::User => ChatRole::User,
        Role::Assistant => ChatRole::Assistant,
        Role::Tool => ChatRole::Tool,
    }
}

/// Project a pinned descriptor into the model-visible schema. The real
/// description and JSON Schema travel to the model, so it can call tools with
/// arguments; the descriptor's `content_hash` stays internal (G3/G8).
fn to_tool_schema(descriptor: &ToolDescriptor) -> ToolSchema {
    ToolSchema {
        id: descriptor.id.clone(),
        description: descriptor.description.clone(),
        parameters: descriptor.parameters.clone(),
    }
}

/// The committed assistant turn: its content blocks verbatim (text and tool-use
/// interleaved), so the transcript explains both what was said and what was
/// called.
fn assistant_message(run_id: &RunId, step: usize, blocks: Vec<ContentBlock>) -> Message {
    Message {
        id: MessageId(format!("{}-assistant-{step}", run_id.0)),
        role: Role::Assistant,
        content: blocks,
    }
}

fn tool_result_message(call: &ToolCall, output: &ToolOutput) -> Message {
    tool_result_message_from(&call.call_id, &output.content)
}

/// A tool-role message carrying a structured `ToolResult` block addressed to the
/// originating call, so the model sees a real tool result rather than loose text.
fn tool_result_message_from(call_id: &str, text: &str) -> Message {
    Message {
        id: MessageId(format!("tool-{call_id}")),
        role: Role::Tool,
        content: vec![ContentBlock::tool_result(
            call_id.to_string(),
            vec![ContentBlock::text(text.to_string())],
        )],
    }
}

/// Best-effort live emission. A sink failure is swallowed: committed truth is
/// authoritative, not the live stream (G10/G13).
async fn emit(context: &RuntimeRunContext, run_id: &RunId, kind: StreamKind) {
    if let Some(sink) = &context.stream_sink {
        let _ = sink
            .send(StreamEvent {
                run_id: run_id.clone(),
                kind,
            })
            .await;
    }
}

/// Stage and commit the terminal/parked checkpoint, emit `RunFinished`, and
/// return the outcome. Commit only runs when a coordinator is wired.
async fn finish(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    checkpoint: Checkpoint,
) -> Result<Phase> {
    let Checkpoint {
        new_messages,
        staged_state,
        end,
    } = checkpoint;

    // Project the attempt's `End` onto the stored authority: a pause records
    // `Phase::Waiting` and parks its ticket; a terminus records `Ended(cause)`
    // with no ticket. The two cannot disagree because `End` made them exclusive.
    let (phase, waiting) = match end {
        End::Parked(mut ticket) => {
            // The ticket carries the real thread id only at commit time.
            ticket.thread_id = thread_id.clone();
            (Phase::Waiting, Some(ticket))
        }
        End::Ended(cause) => (Phase::Ended(cause), None),
    };

    if let Some(coordinator) = &context.commit {
        let mut events = vec![EventDraft {
            kind: EventKind::RunPhaseChanged,
            payload: serde_json::json!({ "phase": phase }),
        }];
        if !staged_state.is_empty() {
            events.push(EventDraft {
                kind: EventKind::StateChanged,
                payload: serde_json::json!({ "commands": staged_state.len() }),
            });
        }
        if waiting.is_some() {
            events.push(EventDraft {
                kind: EventKind::RunWaiting,
                payload: serde_json::json!({ "run_id": run_id.0 }),
            });
        }
        let commit = ThreadCommit {
            thread_id: thread_id.clone(),
            run_fact: RunFact {
                run_id: run_id.clone(),
                phase: phase.clone(),
            },
            messages: new_messages,
            state: staged_state,
            events,
            waiting,
        };
        coordinator
            .commit(commit)
            .await
            .map_err(|err| Error::Commit(err.to_string()))?;
    }

    emit(context, &run_id, StreamKind::RunFinished).await;
    Ok(phase)
}

fn map_resolver_error(err: resolver::Error) -> Error {
    Error::Resolution(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding};

    fn spec(instructions: &str) -> ResolvedSpec {
        ResolvedSpec {
            catalog_fingerprint: CatalogFingerprint("c".to_string()),
            instructions: instructions.to_string(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_instance_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            tool_descriptors: Vec::new(),
            plugin_ids: Vec::new(),
        }
    }

    fn user_message() -> Message {
        Message::text(MessageId("m1".to_string()), Role::User, "hi")
    }

    #[test]
    fn instructions_lead_the_request_as_a_system_message() {
        let request = build_chat_request(&spec("be helpful"), &[user_message()]);
        assert_eq!(request.messages.len(), 2);
        assert!(matches!(request.messages[0].role, ChatRole::System));
        assert_eq!(
            request.messages[0].content,
            vec![ContentBlock::text("be helpful")]
        );
        assert!(matches!(request.messages[1].role, ChatRole::User));
    }

    #[test]
    fn empty_instructions_contribute_no_system_message() {
        let request = build_chat_request(&spec(""), &[user_message()]);
        assert_eq!(request.messages.len(), 1);
        assert!(matches!(request.messages[0].role, ChatRole::User));
    }
}
