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
use awaken_agent_contract::agent::state::{
    Command as StateCommand, Key as StateKey, MergePolicy, Scope, Store, validate_batch,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{PendingTool, WaitingReason, WaitingTicket};
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft as EventDraft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::stream_checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::event::{Event as StreamEvent, Kind as StreamKind};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::agent_resolver::{AgentRequest, AgentStep};
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatMessage, ChatRequest, ChatResponse, ChatRole, DeltaSink, StopReason,
    THREAD_USAGE_STATE_KEY, TokenUsage, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext};
use awaken_runtime_contract::plugin::{
    PhaseContext, PhaseHookPoint, ResolvedExecutionEnv, RunEndContext, RunEndDecision,
};
use awaken_runtime_contract::resolved::ResolvedRun;
use awaken_runtime_contract::resolver::{self, RunResolver};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult, validate_resume};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
use awaken_runtime_contract::tool::ToolOutput;

use crate::runtime::Runtime;

mod convert;
pub(crate) use convert::*;

/// Message-id base for messages produced by a resumed attempt, kept distinct
/// from the original attempt's ids.
const RESUME_STEP_BASE: usize = 1_000;

#[async_trait]
impl RunExecutor for Runtime {
    #[tracing::instrument(name = "runtime.run", skip_all, fields(awaken.run.id = %activation.run_id.0))]
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
        &thread_id,
        &context,
        transcript,
        activation.input,
        0,
        store,
        Vec::new(),
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

    // State staged by the resumed tool itself seeds the attempt's batch (kept
    // first), so step commits and conflict validation cover it too.
    let checkpoint = drive(
        runtime,
        &resolved,
        &env,
        &run_id,
        &thread_id,
        &context,
        transcript,
        resumed,
        RESUME_STEP_BASE,
        store,
        seed_state,
    )
    .await?;

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
/// and resume; the caller seeds the transcript, the already-produced messages,
/// and any state the resume itself staged (`seed_state`).
// The agent-invocation span in OTel GenAI terms: this is the reasoning loop that
// drives one agent turn to completion, so it carries `gen_ai.operation.name =
// "invoke_agent"` and parents the `chat` / `execute_tool` spans. `SpanKind::Internal`;
// `gen_ai.provider.name` is the neutral `awaken` (G22). Awaken run/thread ids ride
// alongside as `awaken.*` extensions.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "invoke_agent",
    skip_all,
    fields(
        otel.kind = "internal",
        gen_ai.operation.name = "invoke_agent",
        gen_ai.provider.name = "awaken",
        gen_ai.conversation.id = %thread_id.0,
        awaken.run.id = %run_id.0,
        awaken.thread.id = %thread_id.0,
    )
)]
async fn drive(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    context: &RuntimeRunContext,
    mut transcript: Vec<Message>,
    mut new_messages: Vec<Message>,
    step_base: usize,
    mut store: Store,
    seed_state: Vec<StateCommand>,
) -> Result<Checkpoint> {
    let llm = runtime.llm().ok_or_else(|| {
        Error::Execution("no model provider configured for this runtime".to_string())
    })?;

    let mut staged_state: Vec<StateCommand> = seed_state;
    // Permission-audit drafts accumulated across the attempt's gate decisions.
    let mut audit: Vec<EventDraft> = Vec::new();
    // Watermarks for step-boundary incremental commits: everything below a
    // watermark is already durable; the checkpoint returns only the tail.
    let mut committed_messages = 0usize;
    let mut committed_state = 0usize;
    let mut committed_audit = 0usize;
    let mut running_committed = false;
    // The loop's terminal decision. It stays `None` only if the loop runs to its
    // step ceiling, which is itself a terminus (`MaxSteps`).
    let mut end: Option<End> = None;
    // Forwards streamed text chunks to the live stream during each inference.
    let delta_sink = StreamDeltaSink { context, run_id };

    // Durable interrupted-stream checkpoints (Phase 3), when a store is wired.
    // A checkpoint left by a crash mid-recovery is read once here and applied to
    // this drive's first step — the only step that can be resuming, since
    // per-step commit means at most one step was ever in flight.
    let checkpoint_ctx = context
        .stream_checkpoint
        .as_ref()
        .map(|store| CheckpointCtx {
            store: store.as_ref(),
            run_id: run_id.0.clone(),
            thread_id: thread_id.0.clone(),
            model: resolved.spec.model_binding.model_ref.clone(),
        });
    let mut resume_checkpoint = match &checkpoint_ctx {
        Some(ctx) => ctx.store.get(&ctx.run_id).await,
        None => None,
    };

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
    // Failed inference steps in a row (post-retry). The runtime's tolerance
    // decides when the streak is terminal; a success resets it.
    let mut consecutive_inference_failures = 0usize;
    for step in 0..resolved.spec.max_steps {
        // Step-boundary incremental commit: everything the completed steps
        // staged — messages, state, audit — becomes durable under a `Running`
        // fact before the next inference. A crash then loses at most the step
        // in flight, and readers see committed progress mid-run. The staged
        // batch is validated cumulatively (the tail alone could hide an
        // exclusive-key conflict with an already-committed step); a conflict
        // ends the run as `finalize` would, except steps committed while the
        // batch was still valid stay committed. Without a coordinator the
        // block is inert and the whole batch rides `finalize`, as before.
        if context.commit.is_some()
            && (new_messages.len() > committed_messages
                || staged_state.len() > committed_state
                || audit.len() > committed_audit)
        {
            if validate_batch(&staged_state).is_err() {
                staged_state.truncate(committed_state);
                end = Some(End::Ended(EndCause::Error(Failure::StateConflict)));
                break;
            }
            commit_step_delta(
                context,
                thread_id,
                run_id,
                new_messages[committed_messages..].to_vec(),
                staged_state[committed_state..].to_vec(),
                audit[committed_audit..].to_vec(),
                !running_committed,
            )
            .await?;
            committed_messages = new_messages.len();
            committed_state = staged_state.len();
            committed_audit = audit.len();
            running_committed = true;
        }

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

        // Phase hooks stage state (G9/G30). A BeforeInference hook may also inject
        // request-only context (e.g. recalled memories), prepended to this
        // inference and never committed.
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepStart,
            &transcript,
            &mut staged_state,
        )
        .await;
        let prelude = run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::BeforeInference,
            &transcript,
            &mut staged_state,
        )
        .await;

        // A `MaxTokens`-truncated text-only turn is continued in place: the
        // partial text is committed as its own assistant message, a continuation
        // prompt follows it, and inference reruns on the grown transcript — up
        // to a per-step budget. A truncated turn that still carries tool calls
        // skips recovery: those calls must be answered by tool results, not
        // another assistant turn. An exhausted budget lets the truncated turn
        // stand as the step's output. A retryable inference failure retries with
        // backoff inside `infer_with_retry`; a permanent failure (or exhausted
        // retries) commits a typed terminal reason carrying the error's
        // classification code (G26).
        let mut truncation_retries = 0;
        // A persisted partial resumes only this drive's first step (see above);
        // it is consumed on the first inference call of that step.
        let step_resume = if step == 0 {
            resume_checkpoint.take()
        } else {
            None
        };
        let checkpoint_ref = checkpoint_ctx.as_ref();
        let infer_turn = async {
            let mut pending_resume = step_resume;
            loop {
                let request = build_chat_request(
                    &resolved.spec,
                    &prelude,
                    &transcript,
                    &env.dynamic_descriptors(),
                );
                match infer_with_retry(
                    llm,
                    request,
                    runtime.retry_policy(),
                    runtime.circuit_breaker(),
                    &delta_sink,
                    checkpoint_ref,
                    pending_resume.take(),
                )
                .await
                {
                    Ok(response) => {
                        let truncated_text_only = response.stop_reason
                            == Some(StopReason::MaxTokens)
                            && response.output.tool_calls().is_empty()
                            && !response.output.text_content().is_empty();
                        if truncated_text_only
                            && truncation_retries < runtime.max_continuation_retries()
                        {
                            let partial = truncated_assistant_message(
                                run_id,
                                step_base + step,
                                truncation_retries,
                                response.output.blocks,
                            );
                            transcript.push(partial.clone());
                            new_messages.push(partial);
                            let prompt =
                                continuation_message(run_id, step_base + step, truncation_retries);
                            transcript.push(prompt.clone());
                            new_messages.push(prompt);
                            truncation_retries += 1;
                            continue;
                        }
                        break Ok(response);
                    }
                    Err(err) => break Err(err),
                }
            }
        };
        // A cancel aborts a hung or long inference in flight rather than
        // waiting for the step boundary. Dropping the inference future may
        // abandon a half-open breaker probe; recording that reopens the
        // circuit for a later re-probe without polluting the failure count.
        let inference = match &context.cancellation {
            Some(token) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    result = infer_turn => Some(result),
                }
            }
            None => Some(infer_turn.await),
        };
        let Some(inference) = inference else {
            runtime
                .circuit_breaker()
                .record_abandoned_probe(&resolved.spec.model_binding.model_ref);
            end = Some(End::Ended(EndCause::Cancelled));
            break;
        };
        let response = match inference {
            Ok(response) => {
                consecutive_inference_failures = 0;
                response
            }
            Err(err) => {
                // Below the tolerance the failed step is absorbed and the next
                // step re-infers; each absorbed failure still consumes a step,
                // so `max_steps` stays the runaway backstop. At the tolerance
                // the streak ends the run with the last error's classification.
                consecutive_inference_failures += 1;
                if consecutive_inference_failures < runtime.max_consecutive_inference_failures() {
                    continue;
                }
                end = Some(End::Ended(EndCause::Error(Failure::Inference {
                    code: err.code().to_string(),
                    message: err.to_string(),
                })));
                break;
            }
        };

        // Record this step's token usage as committed thread truth, accumulated across
        // steps and turns, so an adapter can surface a session's usage by reading thread
        // state — the runtime records the fact without naming any wire, and it survives
        // a restart. A step whose provider reported no usage records nothing.
        if let Some(step_usage) = response.usage {
            let prior: TokenUsage = store
                .get(Scope::Thread, &StateKey(THREAD_USAGE_STATE_KEY.to_string()))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let total = TokenUsage {
                prompt_tokens: prior.prompt_tokens + step_usage.prompt_tokens,
                completion_tokens: prior.completion_tokens + step_usage.completion_tokens,
            };
            let usage_cmd = StateCommand::set(
                Scope::Thread,
                MergePolicy::Commutative,
                THREAD_USAGE_STATE_KEY,
                serde_json::to_value(total).expect("token usage serializes"),
            );
            store.apply(&usage_cmd);
            staged_state.push(usage_cmd);
        }

        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::AfterInference,
            &transcript,
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

        // A text-only turn (no tool requests) is a natural end — unless queued
        // live input or a run-end guard keeps the loop going. Queued input is
        // drained first: a message the caller already addressed to this run
        // preempts any guard's end-of-run verdict, exactly as if it had arrived
        // one turn earlier.
        if calls.is_empty() {
            let injected = drain_live_inbox(context, run_id, &transcript);
            if !injected.is_empty() {
                for message in injected {
                    transcript.push(message.clone());
                    new_messages.push(message);
                }
                continue;
            }
            // The runtime owns *when* the loop stops; a guard supplies the
            // predicate and any feedback. Its `detail` is opaque
            // (anti-corruption): the kernel forwards it, never interprets it.
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
            &transcript,
            &mut staged_state,
        )
        .await;
    }

    // No early terminus means the loop exhausted its step ceiling.
    let end = end.unwrap_or(End::Ended(EndCause::MaxSteps));
    // Return only the tail beyond the step-commit watermarks: the final
    // commit must not re-append what step commits already made durable.
    Ok(Checkpoint {
        new_messages: new_messages.split_off(committed_messages),
        staged_state: staged_state.split_off(committed_state),
        audit: audit.split_off(committed_audit),
        end,
    })
}

/// Commit one step's staged delta under a `Running` fact — the durable record
/// that the run is mid-flight with these steps completed. The first step
/// commit also records the phase transition into `Running`; terminal and
/// parked outcomes never come through here (they ride `finish`, where the
/// phase and any waiting ticket commit atomically).
async fn commit_step_delta(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: &RunId,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
    audit: Vec<EventDraft>,
    first: bool,
) -> Result<()> {
    let Some(coordinator) = &context.commit else {
        return Ok(());
    };
    let mut events = Vec::with_capacity(audit.len() + 2);
    if first {
        events.push(EventDraft {
            kind: EventKind::RunPhaseChanged,
            payload: serde_json::json!({ "phase": Phase::Running }),
        });
    }
    // Mirror the finish boundary: state riding a commit is announced by a
    // StateChanged event, whichever boundary commits it.
    if !state.is_empty() {
        events.push(EventDraft {
            kind: EventKind::StateChanged,
            payload: serde_json::json!({ "commands": state.len() }),
        });
    }
    events.extend(audit);
    coordinator
        .commit(ThreadCommit {
            thread_id: thread_id.clone(),
            run_fact: RunFact {
                run_id: run_id.clone(),
                phase: Phase::Running,
            },
            messages,
            state,
            events,
            outbox: Vec::new(),
            waiting: None,
        })
        .await
        .map_err(|err| Error::Commit(err.to_string()))?;
    Ok(())
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

/// The model-facing prompt appended after a confirmed partial when continuing an
/// interrupted stream, so the model resumes instead of regenerating. Same intent
/// as the in-place `MaxTokens` continuation, but for a transient mid-stream drop.
const STREAM_CONTINUATION_PROMPT: &str = "Your previous response was interrupted \
    mid-stream. Continue from exactly where you left off, without repeating any \
    text you already wrote.";

/// The current attempt's captured partial: streamed text plus any tool calls
/// seen (each with its accumulated-so-far raw argument text, last-wins by id).
struct Snapshot {
    text: String,
    tools: Vec<PartialToolCall>,
}

/// Wraps the live `DeltaSink` to also capture the current attempt's streamed
/// partial, so a mid-stream interruption can be *continued from it* rather than
/// regenerated from scratch (R1–R3 recovery). Text and tool progress are
/// forwarded to `inner` unchanged, so the live stream is untouched. Tool
/// arguments arrive as raw, accumulated JSON text (see `on_tool_call`); whether
/// they parse later decides completed-vs-in-flight.
struct ContinuationSink<'a> {
    inner: &'a dyn DeltaSink,
    text: std::sync::Mutex<String>,
    tools: std::sync::Mutex<Vec<PartialToolCall>>,
}

impl<'a> ContinuationSink<'a> {
    fn new(inner: &'a dyn DeltaSink) -> Self {
        Self {
            inner,
            text: std::sync::Mutex::new(String::new()),
            tools: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The current attempt's captured partial.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.lock().expect("continuation buffer").clone(),
            tools: self.tools.lock().expect("continuation tools").clone(),
        }
    }

    /// Reset per-attempt capture before a fresh attempt streams.
    fn reset(&self) {
        self.text.lock().expect("continuation buffer").clear();
        self.tools.lock().expect("continuation tools").clear();
    }
}

#[async_trait]
impl DeltaSink for ContinuationSink<'_> {
    async fn on_text(&self, chunk: &str) {
        // Guard drops at the statement end, never held across the await below.
        self.text
            .lock()
            .expect("continuation buffer")
            .push_str(chunk);
        self.inner.on_text(chunk).await;
    }

    async fn on_tool_call(&self, call_id: &str, tool_id: &str, arguments: &serde_json::Value) {
        // Mid-stream, a provider hands tool arguments as raw accumulated JSON
        // text (a `Value::String`), which may be incomplete if the drop lands
        // mid-arguments. Keep the raw text, last-wins per call id (the provider
        // resends the running accumulation), so a later parse decides whether the
        // call had finished.
        let raw = match arguments {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        {
            let mut tools = self.tools.lock().expect("continuation tools");
            match tools.iter_mut().find(|t| t.call_id == call_id) {
                Some(existing) => {
                    existing.tool_id = tool_id.to_string();
                    existing.raw_arguments = raw;
                }
                None => tools.push(PartialToolCall {
                    call_id: call_id.to_string(),
                    tool_id: tool_id.to_string(),
                    raw_arguments: raw,
                }),
            }
        }
        self.inner.on_tool_call(call_id, tool_id, arguments).await;
    }
}

/// The tool calls whose accumulated arguments parse as complete JSON — i.e. the
/// model finished emitting them before the drop, so they can be executed as-is
/// (R2). A call still in flight (unparseable / empty-named args) is excluded.
fn parse_completed(tools: &[PartialToolCall]) -> Vec<ToolCall> {
    tools
        .iter()
        .filter(|tool| !tool.tool_id.is_empty())
        .filter_map(|tool| {
            serde_json::from_str::<serde_json::Value>(&tool.raw_arguments)
                .ok()
                .map(|arguments| ToolCall {
                    call_id: tool.call_id.clone(),
                    tool_id: tool.tool_id.clone(),
                    arguments,
                })
        })
        .collect()
}

/// Synthesize the turn the model had produced when a drop interrupted it after
/// its tool calls were complete (R2): the salvaged text followed by the completed
/// tool-use blocks, stopped for tool use. The engine's tool loop runs these
/// without another model round-trip.
fn synthesized_tool_response(text: &str, calls: Vec<ToolCall>) -> ChatResponse {
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::text(text.to_string()));
    }
    for call in calls {
        blocks.push(ContentBlock::tool_use(
            call.call_id,
            call.tool_id,
            call.arguments,
        ));
    }
    ChatResponse {
        output: AssistantOutput::from_blocks(blocks),
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
    }
}

/// Rebuild a request that carries the confirmed partial as an assistant prefix
/// followed by a continuation prompt, so the model continues rather than
/// regenerates. Request-only: these two messages are never committed to the
/// transcript — a transient interruption is not a real turn boundary.
fn continuation_request(request: &ChatRequest, prefix: &str) -> ChatRequest {
    let mut messages = request.messages.clone();
    messages.push(ChatMessage {
        role: ChatRole::Assistant,
        content: vec![ContentBlock::text(prefix.to_string())],
    });
    messages.push(ChatMessage {
        role: ChatRole::User,
        content: vec![ContentBlock::text(STREAM_CONTINUATION_PROMPT.to_string())],
    });
    ChatRequest {
        messages,
        ..request.clone()
    }
}

/// Prepend the confirmed partial `prefix` onto a continued response so the
/// committed turn is the whole text. An empty prefix returns the response
/// unchanged — the common, non-interrupted path pays nothing.
fn stitch_prefix(response: ChatResponse, prefix: &str) -> ChatResponse {
    if prefix.is_empty() {
        return response;
    }
    let mut blocks = response.output.blocks;
    match blocks.first_mut() {
        Some(ContentBlock::Text { text }) => *text = format!("{prefix}{text}"),
        _ => blocks.insert(0, ContentBlock::text(prefix.to_string())),
    }
    ChatResponse {
        output: AssistantOutput::from_blocks(blocks),
        ..response
    }
}

/// Per-run wiring for durable interrupted-stream checkpoints (Phase 3), present
/// only when the run context supplies a `StreamCheckpointStore`. Holds the keys a
/// flush needs; the store handle is borrowed for the run's duration.
struct CheckpointCtx<'a> {
    store: &'a dyn StreamCheckpointStore,
    run_id: String,
    thread_id: String,
    model: String,
}

impl CheckpointCtx<'_> {
    fn checkpoint(&self, text: String, tools: Vec<PartialToolCall>) -> StreamCheckpoint {
        StreamCheckpoint {
            run_id: self.run_id.clone(),
            thread_id: self.thread_id.clone(),
            model: self.model.clone(),
            partial_text: text,
            partial_tools: tools,
        }
    }
}

/// Call inference, streaming chunks to `sink` as they arrive and retrying a
/// retryable failure per the policy (exponential backoff with jitter, honoring a
/// server `Retry-After`). A permanent failure returns immediately (G26). Every
/// attempt first passes the model's circuit breaker: while its circuit is open
/// the call fails fast as a retryable provider error instead of burning the
/// retry budget against a provider that is already down.
///
/// A stream cut off mid-flight is recovered from its partial rather than
/// regenerated: text-only continues from an assistant prefix + continuation
/// prompt (R1); a still-open tool call is dropped and the text continues (R3);
/// tool calls that finished before the drop are executed without re-inferring
/// (R2); nothing salvageable restarts clean (R4). When a `checkpoint` store is
/// wired, the partial is flushed durably at the interruption boundary (Phase 3)
/// so a crash *during* recovery resumes in a fresh process (via `resume`) instead
/// of re-running the step. The checkpoint is deleted the instant recovery
/// concludes here — it survives only the crash window this guards.
// The `chat` inference span lives at this retry seam, not inside a provider, so
// every executor (echo/probe as well as the real genai client) yields one span per
// logical model call. It follows the OTel GenAI semantic conventions: span name
// `chat {model}`, `SpanKind::Client`, `gen_ai.operation.name = "chat"`, and the
// `gen_ai.usage.*` / `gen_ai.response.finish_reasons` recorded from the committed
// `ChatResponse`. Retries happen inside `infer_with_retry_inner`, within this span.
// `gen_ai.provider.name` is the neutral `awaken` (the runtime never names a concrete
// provider SDK — G22); the bound model is the real routing identity.
#[tracing::instrument(
    name = "chat",
    skip_all,
    fields(
        otel.name = tracing::field::Empty,
        otel.kind = "client",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "awaken",
        gen_ai.request.model = %request.model_binding.model_ref,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        error.type = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
)]
async fn infer_with_retry(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    policy: &crate::retry::LlmRetryPolicy,
    breaker: &crate::circuit_breaker::CircuitBreaker,
    sink: &dyn DeltaSink,
    checkpoint: Option<&CheckpointCtx<'_>>,
    resume: Option<StreamCheckpoint>,
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    let span = tracing::Span::current();
    // OTel GenAI span name is the templated `{operation} {model}` (SHOULD).
    span.record(
        "otel.name",
        format!("chat {}", request.model_binding.model_ref).as_str(),
    );
    let result =
        infer_with_retry_inner(llm, request, policy, breaker, sink, checkpoint, resume).await;
    // Any return means recovery concluded in-process, so the checkpoint (if any)
    // is spent. It survives only a crash *before* this line — mid-recovery —
    // which is exactly the cross-process window Phase 3 guards.
    if let Some(ctx) = checkpoint {
        ctx.store.delete(&ctx.run_id).await;
    }
    match &result {
        Ok(response) => {
            if let Some(usage) = &response.usage {
                span.record("gen_ai.usage.input_tokens", usage.prompt_tokens as i64);
                span.record("gen_ai.usage.output_tokens", usage.completion_tokens as i64);
            }
            if let Some(reason) = &response.stop_reason {
                span.record(
                    "gen_ai.response.finish_reasons",
                    format!("{reason:?}").as_str(),
                );
            }
        }
        // OTel: on a failed inference, tag `error.type` (the neutral taxonomy code)
        // and mark the span status ERROR so the trace surfaces the failure.
        Err(err) => {
            span.record("error.type", err.code());
            span.record("otel.status_code", "ERROR");
        }
    }
    result
}

async fn infer_with_retry_inner(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    request: ChatRequest,
    policy: &crate::retry::LlmRetryPolicy,
    breaker: &crate::circuit_breaker::CircuitBreaker,
    sink: &dyn DeltaSink,
    checkpoint: Option<&CheckpointCtx<'_>>,
    resume: Option<StreamCheckpoint>,
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    let model = request.model_binding.model_ref.clone();
    let sink = ContinuationSink::new(sink);
    // Text confirmed from prior interrupted attempts; grows as continuation
    // proceeds. Empty means "start (or restart) clean".
    let mut prefix = String::new();

    // Cross-process resume: reproduce in-process the plan a fresh interruption
    // would have taken from this persisted partial.
    if let Some(cp) = resume {
        let completed = parse_completed(&cp.partial_tools);
        if !completed.is_empty() {
            // R2: the model had finished its tool calls before the crash — run
            // them without re-inferring.
            return Ok(synthesized_tool_response(&cp.partial_text, completed));
        }
        // R1/R3: continue from the recovered text (any in-flight tool dropped).
        // R4 (empty text) leaves the prefix empty, i.e. a clean start.
        prefix = cp.partial_text;
    }

    let mut attempt = 0;
    loop {
        if let Err(reason) = breaker.check(&model) {
            return Err(awaken_runtime_contract::llm::Error::Provider(reason));
        }
        let attempt_request = if prefix.is_empty() {
            request.clone()
        } else {
            continuation_request(&request, &prefix)
        };
        sink.reset();
        match llm.infer_streaming(attempt_request, &sink).await {
            Ok(response) => {
                breaker.record_success(&model);
                return Ok(stitch_prefix(response, &prefix));
            }
            Err(err) => {
                // Only retryable errors speak to provider health; the counted
                // set must stay exactly `is_retryable`, or permanent faults
                // (bad key, overlong prompt) would trip the breaker.
                let retryable = err.is_retryable();
                if retryable {
                    breaker.record_failure(&model);
                }
                let snapshot = sink.snapshot();
                // The whole text so far: prior confirmed prefix + this attempt.
                let combined_text = format!("{prefix}{}", snapshot.text);

                if retryable {
                    // Boundary flush (Phase 3): persist the whole in-flight partial
                    // so a crash during recovery resumes here. Best-effort — a
                    // store fault must never turn a recoverable blip into a failure.
                    if let Some(ctx) = checkpoint {
                        ctx.store
                            .put(ctx.checkpoint(combined_text.clone(), snapshot.tools.clone()))
                            .await;
                    }
                    // R2: tool calls finished before the drop → execute them now
                    // rather than re-inferring, even if the retry budget is spent.
                    let completed = parse_completed(&snapshot.tools);
                    if !completed.is_empty() {
                        return Ok(synthesized_tool_response(&combined_text, completed));
                    }
                }

                if retryable && attempt < policy.max_retries {
                    // R1/R3/R4 collapse: continue from the combined text (empty ⇒
                    // clean restart); any still-open tool call is dropped.
                    prefix = combined_text;
                    tokio::time::sleep(policy.delay_before_retry(&err, attempt)).await;
                    attempt += 1;
                } else {
                    return Err(err);
                }
            }
        }
    }
}

/// Run every hook registered for one phase point, in dependency order, staging
/// their state commands. Hooks return data; they never write a store (G9/G30).
/// Run every phase hook registered for `point`, staging their state commands and
/// collecting their request-only context messages. The context is meaningful only
/// at `BeforeInference` (where the caller prepends it to the request); other points
/// discard it.
async fn run_phase_hooks(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step: usize,
    point: PhaseHookPoint,
    conversation: &[Message],
    staged_state: &mut Vec<StateCommand>,
) -> Vec<Message> {
    let mut context = Vec::new();
    for hook in env.hooks_for(point) {
        let ctx = PhaseContext {
            run_id: run_id.clone(),
            step,
            point,
        };
        let reaction = hook.on_phase(&ctx, conversation).await;
        staged_state.extend(reaction.state);
        context.extend(reaction.context);
    }
    context
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

/// The id prefix shared by a run's live-inbox injections — same discipline as
/// `steer_id_prefix`: run-scoped, so counting committed messages with this
/// prefix keeps ids unique across a park/resume of the same run.
fn inbox_id_prefix(run_id: &RunId) -> String {
    format!("{}-inbox-", run_id.0)
}

/// Take everything queued on the attempt's live inbox and re-identify it for
/// the transcript. Content and role pass through untouched; only the id is
/// replaced, because caller-supplied ids carry no uniqueness promise inside
/// the committed thread. Empty (or absent) inbox means no messages.
fn drain_live_inbox(
    context: &RuntimeRunContext,
    run_id: &RunId,
    transcript: &[Message],
) -> Vec<Message> {
    let Some(inbox) = context.live_inbox.as_ref() else {
        return Vec::new();
    };
    let drained = inbox.drain_at_boundary();
    if drained.is_empty() {
        return Vec::new();
    }
    let prefix = inbox_id_prefix(run_id);
    let base = transcript
        .iter()
        .filter(|message| message.id.0.starts_with(&prefix))
        .count();
    drained
        .into_iter()
        .enumerate()
        .map(|(nth, entry)| Message {
            id: MessageId(format!("{prefix}{}", base + nth)),
            role: entry.message.role,
            content: entry.message.content,
        })
        .collect()
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
    let checkpoint = drive(
        runtime,
        resolved,
        env,
        run_id,
        thread_id,
        context,
        transcript,
        resumed,
        RESUME_STEP_BASE,
        store,
        seed_state,
    )
    .await?;
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
// OTel GenAI tool span: name `execute_tool {tool}`, `SpanKind::Internal`,
// `gen_ai.operation.name = "execute_tool"` with the tool name + call id.
#[tracing::instrument(
    name = "execute_tool",
    skip_all,
    fields(
        otel.name = tracing::field::Empty,
        otel.kind = "internal",
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.name = %call.tool_id,
        gen_ai.tool.call.id = %call.call_id,
        error.type = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
)]
async fn execute_tool(
    runtime: &Runtime,
    env: Option<&ResolvedExecutionEnv>,
    call: &ToolCall,
) -> ToolOutput {
    let span = tracing::Span::current();
    span.record(
        "otel.name",
        format!("execute_tool {}", call.tool_id).as_str(),
    );
    let tool = env
        .and_then(|env| env.dynamic_tool(&call.tool_id))
        .or_else(|| runtime.tool(&call.tool_id).cloned());
    let output = match tool {
        Some(tool) => match tool.invoke(call.clone()).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(&call.call_id, err.to_string()),
        },
        None => ToolOutput::error(&call.call_id, format!("unknown tool: {}", call.tool_id)),
    };
    // OTel: a tool that returned an error (unknown tool, invocation failure, or a
    // model-visible error result) marks the span ERROR with a `gen_ai`-shaped type.
    if output.is_error {
        span.record("error.type", "tool_error");
        span.record("otel.status_code", "ERROR");
    }
    output
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
            outbox: Vec::new(),
            waiting,
        };
        coordinator
            .commit(commit)
            .await
            .map_err(|err| Error::Commit(err.to_string()))?;
    }

    // A fault is announced before the close signal, so a live consumer can
    // categorize the failure by code while still keying stream teardown on
    // the single terminal `RunFinished`.
    if let Phase::Ended(EndCause::Error(failure)) = &phase {
        emit(
            context,
            &run_id,
            StreamKind::RunFailed {
                code: failure.code().to_string(),
                message: failure.message(),
            },
        )
        .await;
    }
    emit(context, &run_id, StreamKind::RunFinished).await;
    Ok(phase)
}

fn map_resolver_error(err: resolver::Error) -> Error {
    Error::Resolution(err.to_string())
}

#[cfg(test)]
mod tests;
