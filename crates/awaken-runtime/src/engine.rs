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
use awaken_agent_contract::agent::state::{Command as StateCommand, Scope, Store, validate_batch};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{PendingTool, WaitingReason, WaitingTicket};
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft as EventDraft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::event::{Event as StreamEvent, Kind as StreamKind};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::agent_resolver::{AgentRequest, AgentStep};
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
use awaken_runtime_contract::llm::{
    ChatMessage, ChatRequest, ChatRole, DeltaSink, ToolCall, ToolSchema,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext};
use awaken_runtime_contract::plugin::{
    PhaseContext, PhaseHookPoint, ResolvedExecutionEnv, RunEndContext, RunEndDecision,
};
use awaken_runtime_contract::resolved::{ContextPolicy, ResolvedRun, ResolvedSpec, ToolDescriptor};
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
    let env = match runtime.resolve_plugin_env(&resolved.spec) {
        Ok(env) => env,
        Err(_) => {
            return finish(&context, &thread_id, run_id, Checkpoint::capability_bound()).await;
        }
    };

    emit(&context, &run_id, StreamKind::RunStarted).await;

    // A fresh run continues the thread's conversation: seed the transcript with
    // the committed history (when a reader is wired), then this turn's input. The
    // input is also committed, so the next turn sees this user turn too.
    let mut transcript = context
        .reader
        .as_ref()
        .map(|reader| reader.committed_messages(&thread_id))
        .unwrap_or_default();
    transcript.extend(activation.input.iter().cloned());
    let store = store_from_commands(
        context
            .reader
            .as_ref()
            .map(|reader| reader.committed_state(&thread_id))
            .unwrap_or_default(),
    );
    let checkpoint = drive(
        runtime,
        &resolved,
        &env,
        &run_id,
        &context,
        transcript,
        activation.input,
        0,
        store,
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

    let env = match runtime.resolve_plugin_env(&resolved.spec) {
        Ok(env) => env,
        Err(_) => {
            return finish(&context, &thread_id, run_id, Checkpoint::capability_bound()).await;
        }
    };

    emit(&context, &run_id, StreamKind::RunStarted).await;

    // A parked delegation resumes through the resolver, not the tool registry: the
    // resolver runs one more step with the user's input and the run continues or
    // re-parks.
    if ticket.reason == WaitingReason::Delegation {
        return resume_delegation(
            runtime,
            &ticket,
            command.result,
            &resolved,
            &env,
            &run_id,
            &thread_id,
            reader,
            &context,
        )
        .await;
    }

    // Rebuild the transcript and state from committed truth and apply the
    // resumed result. The resumed tool's own state is folded into the store so a
    // later step in this resume observes the advanced state.
    let mut transcript = reader.committed_messages(&thread_id);
    let mut store = store_from_commands(reader.committed_state(&thread_id));
    let (resumed, seed_state) =
        resume_into_messages(runtime, &env, &ticket, command.result, &store).await;
    for command in &seed_state {
        store.apply(command);
    }
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
        store,
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
            audit: checkpoint.audit,
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
    /// Permission-audit drafts produced this attempt, committed with the run's
    /// facts so an authorization decision is explainable (ADR-0030).
    audit: Vec<EventDraft>,
    end: End,
}

/// The terminal decision of one attempt. A paused run carries its ticket here;
/// an ended run carries its cause. The two are mutually exclusive by
/// construction, so the committed [`Phase`] can never disagree with the ticket.
enum End {
    /// The run paused; the ticket is committed alongside the checkpoint. Boxed
    /// because the ticket is much larger than an `EndCause`.
    Parked(Box<WaitingTicket>),
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
            audit: Vec::new(),
            end: End::Ended(cause),
        }
    }

    /// A checkpoint that re-parks the run on `ticket` (no new messages) — used when
    /// a resumed delegation parks again for more input.
    fn parked(ticket: WaitingTicket) -> Self {
        Self {
            new_messages: Vec::new(),
            staged_state: Vec::new(),
            audit: Vec::new(),
            end: End::Parked(Box::new(ticket)),
        }
    }
}

/// Build the audit draft for one gated tool call (ADR-0030). The decision label
/// is the permission-relevant view of the gate outcome.
fn permission_audit(call: &ToolCall, outcome: &GateOutcome) -> EventDraft {
    let decision = match outcome {
        GateOutcome::Allow => "allow",
        GateOutcome::Block { .. } => "deny",
        GateOutcome::Suspend { .. } => "ask",
        GateOutcome::SetResult(_) => "set_result",
        GateOutcome::Schedule { .. } => "schedule",
    };
    EventDraft {
        kind: EventKind::PermissionDecided,
        payload: serde_json::json!({
            "tool_id": call.tool_id,
            "call_id": call.call_id,
            "decision": decision,
        }),
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
    mut store: Store,
) -> Result<Checkpoint> {
    let llm = runtime.llm().ok_or_else(|| {
        Error::Execution("no model provider configured for this runtime".to_string())
    })?;

    let mut staged_state: Vec<StateCommand> = Vec::new();
    // Permission-audit drafts accumulated across the attempt's gate decisions.
    let mut audit: Vec<EventDraft> = Vec::new();
    // The loop's terminal decision. It stays `None` only if the loop runs to its
    // step ceiling, which is itself a terminus (`MaxSteps`).
    let mut end: Option<End> = None;
    // Forwards streamed text chunks to the live stream during each inference.
    let delta_sink = StreamDeltaSink { context, run_id };

    // How many times a run-end guard has steered this run. It is both the
    // guard's iteration signal and the runtime's run-scoped continuation count;
    // `max_steps` remains the hard runaway backstop, since each steer costs a step.
    // Recovered from the committed transcript by counting this run's own steer
    // feedback messages, so the count (and thus the guard's budget) survives a
    // park/resume mid-loop rather than restarting at zero.
    let steer_prefix = steer_id_prefix(run_id);
    let mut forced_continuations = transcript
        .iter()
        .filter(|message| message.id.0.starts_with(&steer_prefix))
        .count();

    // The agent's configured ceiling guards against a non-terminating tool cycle;
    // a natural-end text turn ends the loop earlier.
    //
    // A plugin whose tool set is dynamic (e.g. an MCP server firing
    // `tools/list_changed`) advances its `live_version`; when it changes, the
    // execution environment is re-resolved at this step boundary so the model
    // sees the current tool face. Static runs never re-resolve (version stays
    // `None`), so this is zero-overhead for them.
    let mut live_env: Option<ResolvedExecutionEnv> = None;
    let mut last_live_version = runtime.active_live_version(&resolved.spec.plugin_ids);
    for step in 0..resolved.spec.max_steps {
        if context.is_cancelled() {
            end = Some(End::Ended(EndCause::Cancelled));
            break;
        }

        let current_live_version = runtime.active_live_version(&resolved.spec.plugin_ids);
        if current_live_version != last_live_version {
            // Best-effort: a failed re-resolution keeps the prior environment
            // rather than aborting the run.
            if let Ok(refreshed) = runtime.resolve_plugin_env(&resolved.spec) {
                live_env = Some(refreshed);
            }
            last_live_version = current_live_version;
        }
        let env: &ResolvedExecutionEnv = live_env.as_ref().unwrap_or(env);

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

        let request = build_chat_request(&resolved.spec, &transcript, &env.dynamic_descriptors());
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

        // A text-only turn (no tool requests) is a natural end — unless a run-end
        // guard steers another turn. The runtime owns *when* the loop stops; a
        // guard supplies the predicate and any feedback. Its `detail` is opaque
        // (anti-corruption): the kernel forwards it, never interprets it.
        if calls.is_empty() {
            match consult_run_end(
                env,
                run_id,
                &transcript,
                forced_continuations,
                context.cancellation.as_ref(),
                &store,
            )
            .await
            {
                RunEndOutcome::Steer { feedback, detail } => {
                    // Live progress (best-effort) and durable truth (committed with
                    // the run): the round is both streamed and recorded, so the
                    // round history survives a crash/resume (G1/G13).
                    audit.push(continuation_event(&detail));
                    emit(
                        context,
                        run_id,
                        StreamKind::Continuation {
                            steered: true,
                            detail,
                        },
                    )
                    .await;
                    let message = feedback_message(run_id, forced_continuations, feedback);
                    transcript.push(message.clone());
                    new_messages.push(message);
                    forced_continuations += 1;
                    continue;
                }
                RunEndOutcome::Complete { detail } => {
                    audit.push(continuation_event(&detail));
                    emit(
                        context,
                        run_id,
                        StreamKind::Continuation {
                            steered: false,
                            detail,
                        },
                    )
                    .await;
                    end = Some(End::Ended(EndCause::NaturalEnd));
                    break;
                }
                RunEndOutcome::End => {
                    end = Some(End::Ended(EndCause::NaturalEnd));
                    break;
                }
            }
        }

        // Otherwise run each requested tool and feed the results back.
        for call in calls {
            let outcome = gate_decision(runtime, &call, env, &store).await;
            // Audit the decision of a real (policy-backed) gate (ADR-0030).
            if runtime.gate().is_some() {
                audit.push(permission_audit(&call, &outcome));
            }
            let output = match outcome {
                GateOutcome::Allow => match run_delegation(runtime, context, &call).await {
                    Some(Ok(AgentStep::Done { text })) => ToolOutput::ok(&call.call_id, text),
                    // The delegate parked needing input: park the parent on a
                    // Delegation ticket carrying the opaque handle (durable), resumed
                    // through the resolver.
                    Some(Ok(AgentStep::Parked { handle })) => {
                        let ticket = waiting_ticket(
                            resolved,
                            run_id,
                            &call.call_id,
                            &call,
                            WaitingReason::Delegation,
                            Some(handle),
                        );
                        end = Some(End::Parked(Box::new(ticket)));
                        emit(
                            context,
                            run_id,
                            StreamKind::Waiting {
                                reason: "delegation".to_string(),
                            },
                        )
                        .await;
                        break;
                    }
                    Some(Err(err)) => ToolOutput::error(&call.call_id, err.to_string()),
                    None => execute_tool(runtime, Some(env), &call).await,
                },
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
                        None,
                    );
                    end = Some(End::Parked(Box::new(ticket)));
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
                        None,
                    );
                    end = Some(End::Parked(Box::new(ticket)));
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

            // Post-execution reaction: fold the tool's own state into the live
            // store, then let tool-outcome hooks stage transitions and reminders.
            for command in &output.state {
                store.apply(command);
            }
            let (reactions, reminders) = collect_tool_reactions(env, &call, &output, &store).await;
            for command in &reactions {
                store.apply(command);
            }
            staged_state.extend(reactions);
            for reminder in reminders {
                transcript.push(reminder.clone());
                new_messages.push(reminder);
            }
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
        audit,
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

/// The runtime's view of a run-end consultation, folding the registered guards'
/// decisions: the first guard that steers wins; otherwise the run ends carrying
/// the last guard's completion detail; with no guards at all it just ends.
enum RunEndOutcome {
    End,
    Complete {
        detail: serde_json::Value,
    },
    Steer {
        feedback: String,
        detail: serde_json::Value,
    },
}

/// Consult the run-end continuation guards at a natural-end boundary. Guards are
/// consulted in dependency order; a `Steer` short-circuits (a guard wants another
/// turn). Their `detail` is opaque to the runtime (G2 neutrality) — forwarded,
/// never interpreted.
async fn consult_run_end(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    conversation: &[Message],
    forced_continuations: usize,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    state: &Store,
) -> RunEndOutcome {
    let guards = env.run_end_guards();
    if guards.is_empty() {
        return RunEndOutcome::End;
    }
    let mut completion = None;
    for guard in guards {
        let ctx = RunEndContext {
            run_id: run_id.clone(),
            conversation,
            forced_continuations,
            cancellation,
            state,
        };
        match guard.evaluate(&ctx).await {
            RunEndDecision::Steer { feedback, detail } => {
                return RunEndOutcome::Steer { feedback, detail };
            }
            RunEndDecision::Complete { detail } => completion = Some(detail),
        }
    }
    completion
        .map(|detail| RunEndOutcome::Complete { detail })
        .unwrap_or(RunEndOutcome::End)
}

/// A committed event for one run-end continuation round. The opaque `detail` is
/// forwarded verbatim (the kernel never interprets it, G2); committing it makes
/// the round history durable truth the host projects, not a best-effort stream.
fn continuation_event(detail: &serde_json::Value) -> EventDraft {
    EventDraft {
        kind: EventKind::Continuation,
        payload: detail.clone(),
    }
}

/// The id prefix shared by a run's steer-feedback messages. Counting committed
/// messages with this prefix recovers `forced_continuations` after a resume.
fn steer_id_prefix(run_id: &RunId) -> String {
    format!("{}-steer-", run_id.0)
}

/// The user message a steered continuation injects before looping. Its id is
/// run-scoped and distinct from the step-based assistant/tool message ids.
fn feedback_message(run_id: &RunId, nth: usize, feedback: String) -> Message {
    Message {
        id: MessageId(format!("{}{nth}", steer_id_prefix(run_id))),
        role: Role::User,
        content: vec![ContentBlock::text(feedback)],
    }
}

/// Build the committed ticket for a parked tool call. `handle` carries opaque
/// durable state for a parked delegation and is absent for ordinary tool waits.
fn waiting_ticket(
    resolved: &ResolvedRun,
    run_id: &RunId,
    ticket_id: &str,
    call: &ToolCall,
    reason: WaitingReason,
    handle: Option<serde_json::Value>,
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
            resume_handle: handle,
        }),
        deadline_ms: None,
    }
}

/// The user input carried by a delegation resume (a client result, plain input,
/// or a decision note).
fn delegation_resume_input(result: &ResumeResult) -> String {
    match result {
        ResumeResult::ToolResult(output) => output.content.clone(),
        ResumeResult::Input(text) => text.clone(),
        ResumeResult::Decision { note, .. } => note.clone().unwrap_or_default(),
    }
}

/// Resume a parked delegation: run the resolver one more step with the user's
/// input. On `Done`/error the result is injected as the delegate tool's output and
/// the run drives on; on `Parked` the run re-parks on a fresh Delegation ticket
/// carrying the new handle.
#[allow(clippy::too_many_arguments)]
async fn resume_delegation(
    runtime: &Runtime,
    ticket: &WaitingTicket,
    result: ResumeResult,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    reader: &dyn ThreadReader,
    context: &RuntimeRunContext,
) -> Result<Phase> {
    let call_id = ticket.call_id.clone().unwrap_or_default();
    let handle = ticket
        .pending_tool
        .as_ref()
        .and_then(|tool| tool.resume_handle.clone())
        .unwrap_or(serde_json::Value::Null);
    let input = delegation_resume_input(&result);

    let Some(resolver) = runtime.resolver() else {
        return finish(
            context,
            thread_id,
            run_id.clone(),
            Checkpoint::capability_bound(),
        )
        .await;
    };
    let step = resolver
        .resume(&handle, &input, context.cancellation.as_ref())
        .await;

    let synthetic = match step {
        Ok(AgentStep::Done { text }) => ResumeResult::ToolResult(ToolOutput::ok(&call_id, text)),
        Ok(AgentStep::Parked { handle }) => {
            // Re-park on a fresh Delegation ticket carrying the new handle.
            let mut reparked = ticket.clone();
            if let Some(pending) = reparked.pending_tool.as_mut() {
                pending.resume_handle = Some(handle);
            }
            return finish(
                context,
                thread_id,
                run_id.clone(),
                Checkpoint::parked(reparked),
            )
            .await;
        }
        Err(err) => ResumeResult::ToolResult(ToolOutput::error(&call_id, err.to_string())),
    };

    let mut transcript = reader.committed_messages(thread_id);
    let mut store = store_from_commands(reader.committed_state(thread_id));
    let (resumed, seed_state) = resume_into_messages(runtime, env, ticket, synthetic, &store).await;
    for command in &seed_state {
        store.apply(command);
    }
    transcript.extend(resumed.iter().cloned());
    let mut checkpoint = drive(
        runtime,
        resolved,
        env,
        run_id,
        context,
        transcript,
        resumed,
        RESUME_STEP_BASE,
        store,
    )
    .await?;
    let mut combined = seed_state;
    combined.append(&mut checkpoint.staged_state);
    checkpoint.staged_state = combined;
    finalize(context, thread_id, run_id.clone(), checkpoint).await
}

/// Turn a resumed result into the tool/user message(s) and any staged state.
/// An `allow` decision executes the pending tool now; a `deny` feeds a blocked
/// result; a `ToolResult`/`Input` is used directly.
async fn resume_into_messages(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    ticket: &WaitingTicket,
    result: ResumeResult,
    store: &Store,
) -> (Vec<Message>, Vec<StateCommand>) {
    let call_id = ticket.call_id.clone().unwrap_or_default();
    // The pending call, when the ticket carries one, so a tool-outcome hook can
    // advance a machine on the replayed result exactly like a first-time call.
    let pending_call = |output: &ToolOutput| {
        ticket.pending_tool.as_ref().map(|pending| ToolCall {
            call_id: output.call_id.clone(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        })
    };
    match result {
        ResumeResult::ToolResult(output) => {
            let mut state = output.state.clone();
            let mut messages = vec![tool_result_message_from(&call_id, &output.content)];
            if let Some(call) = pending_call(&output) {
                let mut work = store.clone();
                for command in &output.state {
                    work.apply(command);
                }
                let (reactions, reminders) =
                    collect_tool_reactions(env, &call, &output, &work).await;
                state.extend(reactions);
                messages.extend(reminders);
            }
            (messages, state)
        }
        ResumeResult::Decision { allow, note } => {
            if allow && let Some(pending) = &ticket.pending_tool {
                let call = ToolCall {
                    call_id: call_id.clone(),
                    tool_id: pending.tool_id.clone(),
                    arguments: pending.arguments.clone(),
                };
                let output = execute_tool(runtime, None, &call).await;
                let mut state = output.state.clone();
                let mut messages = vec![tool_result_message_from(&call_id, &output.content)];
                let mut work = store.clone();
                for command in &output.state {
                    work.apply(command);
                }
                let (reactions, reminders) =
                    collect_tool_reactions(env, &call, &output, &work).await;
                state.extend(reactions);
                messages.extend(reminders);
                (messages, state)
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

/// Consult the gate chain: the host permission gate first, then any
/// plugin-contributed gates in dependency order. A call runs only if every gate
/// allows it; the first non-`Allow` outcome wins, and the host permission gate is
/// absolute (a plugin gate can further restrict but never widen it, G21). An
/// absent host gate allows (used only in tests). Gates read the run's state.
async fn gate_decision(
    runtime: &Runtime,
    call: &ToolCall,
    env: &ResolvedExecutionEnv,
    state: &Store,
) -> GateOutcome {
    let ctx = PermissionContext {
        tool_id: call.tool_id.clone(),
        call_id: call.call_id.clone(),
        arguments: call.arguments.clone(),
    };
    let host = match runtime.gate() {
        Some(gate) => gate.gate(&ctx, state).await,
        None => GateOutcome::Allow,
    };
    if !matches!(host, GateOutcome::Allow) {
        return host;
    }
    for gate in env.tool_gates() {
        let outcome = gate.gate(&ctx, state).await;
        if !matches!(outcome, GateOutcome::Allow) {
            return outcome;
        }
    }
    GateOutcome::Allow
}

/// Seed the run's read-only state from committed thread truth. Thread/Shared/
/// Profile-scoped commands re-hydrate; run-scoped commands are dropped so a
/// run-scoped machine starts empty each run (G1/G13). Within the run, later
/// commands are folded into this store so a gate/hook reads accumulated state.
fn store_from_commands(commands: Vec<StateCommand>) -> Store {
    let kept: Vec<StateCommand> = commands
        .into_iter()
        .filter(|command| command.scope != Scope::Run)
        .collect();
    Store::rebuild(&kept)
}

/// Consult the tool-outcome hooks for one executed call. `state` must already
/// reflect the tool's own output state; each hook reads the folded state and may
/// stage further state and reminder messages. Returns the hooks' additional
/// state commands and messages (the tool's own output state is staged by the
/// caller).
async fn collect_tool_reactions(
    env: &ResolvedExecutionEnv,
    call: &ToolCall,
    output: &ToolOutput,
    state: &Store,
) -> (Vec<StateCommand>, Vec<Message>) {
    let mut work = state.clone();
    let mut commands: Vec<StateCommand> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    for observer in env.tool_observers() {
        let reaction = observer.after_tool(call, output, &work).await;
        for command in &reaction.state {
            work.apply(command);
            commands.push(command.clone());
        }
        messages.extend(reaction.messages);
    }
    (commands, messages)
}

/// Run `call` as a delegation when it is the tool the resolver backs, returning
/// the resolver's step; `None` means it is an ordinary tool for the registry. The
/// kernel matches by `resolver.tool_id()`, so it never hard-codes the tool id.
async fn run_delegation(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    call: &ToolCall,
) -> Option<std::result::Result<AgentStep, awaken_runtime_contract::agent_resolver::AgentError>> {
    let resolver = runtime.resolver()?;
    if resolver.tool_id() != call.tool_id {
        return None;
    }
    let request = AgentRequest {
        arguments: call.arguments.clone(),
        cancellation: context.cancellation.clone(),
    };
    Some(resolver.run(request).await)
}

/// Invoke an authorized tool, turning a missing tool or a tool error into a
/// model-visible error result rather than aborting the run. A plugin-contributed
/// dynamic tool (from `env`) takes precedence over the static registry, so an
/// MCP server's live tools resolve; `env` is `None` on the resume path, which
/// only re-runs a statically registered pending tool.
async fn execute_tool(
    runtime: &Runtime,
    env: Option<&ResolvedExecutionEnv>,
    call: &ToolCall,
) -> ToolOutput {
    let tool = env
        .and_then(|env| env.dynamic_tool(&call.tool_id))
        .or_else(|| runtime.tool(&call.tool_id).cloned());
    match tool {
        Some(tool) => match tool.invoke(call.clone()).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(&call.call_id, err.to_string()),
        },
        None => ToolOutput::error(&call.call_id, format!("unknown tool: {}", call.tool_id)),
    }
}

/// Build a model request from the resolved binding, transcript, and visible
/// tool descriptors. `dynamic` carries any plugin-contributed tools live for
/// this step (e.g. an MCP server's current tool set), merged after the pinned
/// config descriptors so the model sees both.
pub(crate) fn build_chat_request(
    spec: &ResolvedSpec,
    transcript: &[Message],
    dynamic: &[ToolDescriptor],
) -> ChatRequest {
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
    let messages = apply_context_policy(&spec.context_policy, messages);
    let tools = spec
        .tool_descriptors
        .iter()
        .chain(dynamic.iter())
        .map(to_tool_schema)
        .collect();
    ChatRequest {
        model_binding: spec.model_binding.clone(),
        messages,
        tools,
    }
}

/// Bound the model-visible message list per `policy`. Operates on the request
/// view only — the committed transcript is untouched (G13). For `KeepLast`, every
/// leading system message is kept (the agent instructions must survive), then
/// only the last `keep_last` non-system messages.
fn apply_context_policy(policy: &ContextPolicy, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let keep_last = match policy {
        ContextPolicy::KeepAll => return messages,
        ContextPolicy::KeepLast { keep_last } => *keep_last,
    };
    let system_prefix = messages
        .iter()
        .take_while(|m| m.role == ChatRole::System)
        .count();
    let rest = messages.len() - system_prefix;
    if rest <= keep_last {
        return messages;
    }
    let drop = rest - keep_last;
    let mut kept: Vec<ChatMessage> = Vec::with_capacity(system_prefix + keep_last);
    kept.extend(messages.iter().take(system_prefix).cloned());
    kept.extend(messages.into_iter().skip(system_prefix + drop));
    kept
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
        audit,
        end,
    } = checkpoint;

    // Project the attempt's `End` onto the stored authority: a pause records
    // `Phase::Waiting` and parks its ticket; a terminus records `Ended(cause)`
    // with no ticket. The two cannot disagree because `End` made them exclusive.
    let (phase, waiting) = match end {
        End::Parked(mut ticket) => {
            // The ticket carries the real thread id only at commit time.
            ticket.thread_id = thread_id.clone();
            (Phase::Waiting, Some(*ticket))
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
        // Permission-audit drafts ride the same commit as the run's facts (G1).
        events.extend(audit);
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
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
        }
    }

    fn user_message() -> Message {
        Message::text(MessageId("m1".to_string()), Role::User, "hi")
    }

    #[test]
    fn instructions_lead_the_request_as_a_system_message() {
        let request = build_chat_request(&spec("be helpful"), &[user_message()], &[]);
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
        let request = build_chat_request(&spec(""), &[user_message()], &[]);
        assert_eq!(request.messages.len(), 1);
        assert!(matches!(request.messages[0].role, ChatRole::User));
    }

    fn numbered(n: usize) -> Message {
        Message::text(MessageId(format!("m{n}")), Role::User, n.to_string())
    }

    fn spec_with(policy: ContextPolicy) -> ResolvedSpec {
        ResolvedSpec {
            context_policy: policy,
            ..spec("sys")
        }
    }

    fn user_texts(request: &ChatRequest) -> Vec<String> {
        request
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::User))
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn keep_all_sends_the_whole_transcript() {
        let transcript: Vec<Message> = (0..5).map(numbered).collect();
        let request = build_chat_request(&spec_with(ContextPolicy::KeepAll), &transcript, &[]);
        // 1 system + 5 users
        assert_eq!(request.messages.len(), 6);
    }

    #[test]
    fn keep_last_keeps_system_prefix_plus_the_last_n() {
        let transcript: Vec<Message> = (0..5).map(numbered).collect();
        let request = build_chat_request(
            &spec_with(ContextPolicy::KeepLast { keep_last: 2 }),
            &transcript,
            &[],
        );
        // system stays; only the last 2 user messages survive.
        assert!(matches!(request.messages[0].role, ChatRole::System));
        assert_eq!(user_texts(&request), vec!["3".to_string(), "4".to_string()]);
    }

    #[test]
    fn keep_last_larger_than_history_keeps_everything() {
        let transcript: Vec<Message> = (0..3).map(numbered).collect();
        let request = build_chat_request(
            &spec_with(ContextPolicy::KeepLast { keep_last: 10 }),
            &transcript,
            &[],
        );
        assert_eq!(user_texts(&request).len(), 3);
    }

    #[test]
    fn keep_last_zero_keeps_only_the_system_prefix() {
        let transcript: Vec<Message> = (0..3).map(numbered).collect();
        let request = build_chat_request(
            &spec_with(ContextPolicy::KeepLast { keep_last: 0 }),
            &transcript,
            &[],
        );
        assert_eq!(request.messages.len(), 1);
        assert!(matches!(request.messages[0].role, ChatRole::System));
    }
}
