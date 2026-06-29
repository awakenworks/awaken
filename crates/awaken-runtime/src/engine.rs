//! The async agent execution loop.
//!
//! `execute` resolves the activation, runs the model/tool phase loop, stages a
//! `ThreadCommit`, and commits durable truth. A run can park on a gate `Suspend`
//! by committing a `WaitingTicket`, then `resume` validates a `ResumeCommand`
//! against it and continues. Live progress is best-effort and never the replay
//! source (G1/G13).

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Lifecycle};
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
use awaken_runtime_contract::execution::{Error, Result, RunExecutor, RunOutcome};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatContent, ChatMessage, ChatRequest, ChatRole, ToolCall, ToolSchema,
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

/// Upper bound on model/tool steps for one run. A natural-end text turn ends the
/// loop earlier; the bound only guards against a non-terminating tool cycle.
const MAX_STEPS: usize = 16;

/// Message-id base for messages produced by a resumed attempt, kept distinct
/// from the original attempt's ids.
const RESUME_STEP_BASE: usize = 1_000;

#[async_trait]
impl RunExecutor for Runtime {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunOutcome> {
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
) -> Result<RunOutcome> {
    let resolved = runtime
        .resolve(&activation.snapshot)
        .map_err(map_resolver_error)?;

    let run_id = activation.run_id.clone();
    let thread_id = activation.thread_id.clone();

    // Cancellation observed before any model call: commit a terminal Cancelled
    // outcome instead of starting work.
    if context.is_cancelled() {
        return finish(&context, &thread_id, run_id, Outcome::cancelled()).await;
    }

    // Merge the active plugins under their capability bounds; a violation fails
    // the run closed before any model call (G30).
    let env = match runtime.resolve_plugin_env(&resolved.spec.plugin_ids) {
        Ok(env) => env,
        Err(_) => return finish(&context, &thread_id, run_id, Outcome::failed()).await,
    };

    emit(&context, &run_id, StreamKind::RunStarted).await;

    let transcript = activation.input.clone();
    let outcome = drive(
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
    finalize(&context, &thread_id, run_id, outcome).await
}

/// Resume a parked run: validate the resume against the committed ticket, rebuild
/// the transcript from committed messages, inject the resumed result, and drive
/// the loop to a new terminal/parked state (G5/G28).
pub(crate) async fn resume_run(
    runtime: &Runtime,
    command: ResumeCommand,
    reader: &dyn ThreadReader,
    context: RuntimeRunContext,
) -> Result<RunOutcome> {
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
        Err(_) => return finish(&context, &thread_id, run_id, Outcome::failed()).await,
    };

    emit(&context, &run_id, StreamKind::RunStarted).await;

    // Rebuild the transcript from committed truth and apply the resumed result.
    let mut transcript = reader.committed_messages(&thread_id);
    let (resumed, seed_state) = resume_into_messages(runtime, &ticket, command.result).await;
    transcript.extend(resumed.iter().cloned());

    let mut outcome = drive(
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
    combined.append(&mut outcome.staged_state);
    outcome.staged_state = combined;

    finalize(&context, &thread_id, run_id, outcome).await
}

/// Validate the staged state batch, then commit. A conflict fails closed and no
/// state is committed (G13).
async fn finalize(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    outcome: Outcome,
) -> Result<RunOutcome> {
    if !outcome.staged_state.is_empty() && validate_batch(&outcome.staged_state).is_err() {
        let failed = Outcome {
            lifecycle: Lifecycle::Failed,
            staged_state: Vec::new(),
            waiting: None,
            ..outcome
        };
        return finish(context, thread_id, run_id, failed).await;
    }
    finish(context, thread_id, run_id, outcome).await
}

/// The result of driving the loop for one attempt.
struct Outcome {
    lifecycle: Lifecycle,
    new_messages: Vec<Message>,
    staged_state: Vec<StateCommand>,
    waiting: Option<WaitingTicket>,
}

impl Outcome {
    fn cancelled() -> Self {
        Self {
            lifecycle: Lifecycle::Cancelled,
            new_messages: Vec::new(),
            staged_state: Vec::new(),
            waiting: None,
        }
    }

    fn failed() -> Self {
        Self {
            lifecycle: Lifecycle::Failed,
            new_messages: Vec::new(),
            staged_state: Vec::new(),
            waiting: None,
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
) -> Result<Outcome> {
    let llm = runtime.llm().ok_or_else(|| {
        Error::Execution("no model provider configured for this runtime".to_string())
    })?;

    let mut staged_state: Vec<StateCommand> = Vec::new();
    let mut lifecycle = Lifecycle::Completed;
    let mut waiting: Option<WaitingTicket> = None;

    for step in 0..MAX_STEPS {
        if context.is_cancelled() {
            lifecycle = Lifecycle::Cancelled;
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
        let response = llm
            .infer(request)
            .await
            .map_err(|err| Error::Execution(err.to_string()))?;

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
            lifecycle = Lifecycle::Cancelled;
            break;
        }

        match response.output {
            AssistantOutput::Text(text) => {
                emit(
                    context,
                    run_id,
                    StreamKind::OutputText { text: text.clone() },
                )
                .await;
                let message = assistant_message(run_id, step_base + step, text);
                transcript.push(message.clone());
                new_messages.push(message);
                break;
            }
            AssistantOutput::ToolCalls(calls) => {
                let assistant = assistant_tool_call_message(run_id, step_base + step, &calls);
                transcript.push(assistant.clone());
                new_messages.push(assistant);

                for call in calls {
                    let output = match gate_decision(runtime, &call).await {
                        GateOutcome::Allow => execute_tool(runtime, &call).await,
                        GateOutcome::Block { reason } => {
                            ToolOutput::error(&call.call_id, format!("blocked: {reason}"))
                        }
                        GateOutcome::SetResult(output) => output,
                        GateOutcome::Suspend { ticket_id } => {
                            // Park the run on a structured ticket carrying the
                            // pending call so an allow decision can run it later.
                            waiting = Some(waiting_ticket(resolved, run_id, &ticket_id, &call));
                            lifecycle = Lifecycle::Waiting;
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
                    };
                    staged_state.extend(output.state.clone());
                    let message = tool_result_message(&call, &output);
                    transcript.push(message.clone());
                    new_messages.push(message);
                }

                if waiting.is_some() {
                    break;
                }
            }
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

    Ok(Outcome {
        lifecycle,
        new_messages,
        staged_state,
        waiting,
    })
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
) -> WaitingTicket {
    WaitingTicket {
        correlation_id: ticket_id.to_string(),
        run_id: run_id.clone(),
        thread_id: ThreadId(String::new()), // filled in finish via the commit thread id
        snapshot_id: resolved.snapshot_id.0.clone(),
        catalog_fingerprint: resolved.spec.catalog_fingerprint.0.clone(),
        reason: WaitingReason::ToolPermission,
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
            (vec![tool_message(&call_id, &output.content)], state)
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
                (vec![tool_message(&call_id, &output.content)], state)
            } else {
                let reason = note.unwrap_or_else(|| "denied".to_string());
                (
                    vec![tool_message(&call_id, &format!("blocked: {reason}"))],
                    Vec::new(),
                )
            }
        }
        ResumeResult::Input(text) => (
            vec![Message {
                id: MessageId(format!("resume-input-{call_id}")),
                role: Role::User,
                content: text,
            }],
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
    ChatRequest {
        model_binding: spec.model_binding.clone(),
        messages: transcript.iter().map(to_chat_message).collect(),
        tools: spec.tool_descriptors.iter().map(to_tool_schema).collect(),
    }
}

fn to_chat_message(message: &Message) -> ChatMessage {
    ChatMessage {
        role: to_chat_role(&message.role),
        content: ChatContent::Text(message.content.clone()),
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

fn assistant_message(run_id: &RunId, step: usize, text: String) -> Message {
    Message {
        id: MessageId(format!("{}-assistant-{step}", run_id.0)),
        role: Role::Assistant,
        content: text,
    }
}

/// Record an assistant tool-call turn so the committed transcript explains why
/// tool results follow.
fn assistant_tool_call_message(run_id: &RunId, step: usize, calls: &[ToolCall]) -> Message {
    let summary: Vec<_> = calls.iter().map(|c| c.tool_id.as_str()).collect();
    Message {
        id: MessageId(format!("{}-assistant-{step}", run_id.0)),
        role: Role::Assistant,
        content: format!("tool_calls: {}", summary.join(", ")),
    }
}

fn tool_result_message(call: &ToolCall, output: &ToolOutput) -> Message {
    tool_message(&call.call_id, &output.content)
}

fn tool_message(call_id: &str, content: &str) -> Message {
    Message {
        id: MessageId(format!("tool-{call_id}")),
        role: Role::Tool,
        content: content.to_string(),
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
    outcome: Outcome,
) -> Result<RunOutcome> {
    let Outcome {
        lifecycle,
        new_messages,
        staged_state,
        waiting,
    } = outcome;

    if let Some(coordinator) = &context.commit {
        let mut events = vec![EventDraft {
            kind: EventKind::RunLifecycleChanged,
            payload: serde_json::json!({ "lifecycle": lifecycle }),
        }];
        if !staged_state.is_empty() {
            events.push(EventDraft {
                kind: EventKind::StateChanged,
                payload: serde_json::json!({ "commands": staged_state.len() }),
            });
        }
        // The ticket carries the real thread id only at commit time.
        let waiting = waiting.map(|mut ticket| {
            ticket.thread_id = thread_id.clone();
            ticket
        });
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
                lifecycle: lifecycle.clone(),
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
    Ok(RunOutcome { run_id, lifecycle })
}

fn map_resolver_error(err: resolver::Error) -> Error {
    Error::Resolution(err.to_string())
}
