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
    Command as StateCommand, Scope, StateKey, Store, validate_batch,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{PendingTool, WaitingReason, WaitingTicket};
use awaken_agent_contract::audit::draft::Draft as EventDraft;
use awaken_agent_contract::audit::run_event::RunEvent;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::agent_resolver::{AgentRequest, AgentStep};
use awaken_runtime_contract::boundary::{BoundaryOutcome, evaluate_boundary};
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatMessage, ChatRequest, ChatResponse, DeltaSink, StopReason, ThreadUsage,
    ThreadUsageKey, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext};
use awaken_runtime_contract::plugin::{
    AfterToolContext, ContextMessages, PhaseContext, PhaseHookPoint, PhaseKind,
    ResolvedExecutionEnv, RunEndContext, RunEndDecision,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ResolvedRun, ToolPresentation};
use awaken_runtime_contract::resolver::{self, RunResolver};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult, validate_resume};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor, ToolOutput};

use crate::runtime::Runtime;

mod content;
mod convert;
mod dispatch;
mod inference;
pub(crate) use convert::*;
use inference::infer_with_retry;

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
        if let Some(pause) = &context.pause {
            self.register_pause(&run_id, pause.clone());
        }
        let result = run_agent_loop(self, activation, context).await;
        self.deregister_run(&run_id);
        self.deregister_pause(&run_id);
        result
    }
}

pub(crate) async fn run_agent_loop(
    runtime: &Runtime,
    activation: RunActivation,
    context: RuntimeRunContext,
) -> Result<Phase> {
    // A database-less worker cannot warm-install the agent's published catalog, so
    // its per-session runtime has no catalog matching the run it just claimed. When
    // such a runtime trusts dispatched snapshots, adopt the one carried inline here
    // — it is content-addressed and self-describing — so the fail-closed gate below
    // passes. A no-op on a node with its own matching catalog (the gate still
    // enforces descent-from-active there).
    runtime.reconcile_dispatched_snapshot(&activation.snapshot);
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

    emit(&context, &run_id, AgentEvent::Fact(Fact::RunStarted)).await;

    // A fresh run continues the thread's conversation: seed the transcript with
    // the committed history (when a reader is wired), then this run's input —
    // but only the input NOT already in that history. A reclaimed run re-executes
    // from its activation, yet its input was committed as the first step delta by
    // the prior attempt, so it is already present: appending it again would both
    // show the model the input twice AND re-commit it (the durable accumulator's
    // watermark resets per attempt, so it cannot tell). Keyed on the stable
    // message id, a fresh run's uncommitted input passes through unchanged while a
    // reclaimed or redelivered input is dropped — input delivery is idempotent.
    let mut transcript = context
        .reader
        .as_ref()
        .map(|reader| reader.committed_messages(&thread_id))
        .unwrap_or_default();
    let committed_ids: std::collections::HashSet<_> = transcript
        .iter()
        .map(|message| message.id.clone())
        .collect();
    let fresh_input: Vec<Message> = activation
        .input
        .into_iter()
        .filter(|message| !committed_ids.contains(&message.id))
        .collect();
    transcript.extend(fresh_input.iter().cloned());
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
        fresh_input,
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
        snapshot_id: ExecutableAgentSnapshotId(ticket.snapshot_id),
        catalog_fingerprint: CatalogFingerprint(ticket.catalog_fingerprint),
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

    emit(&context, &run_id, AgentEvent::Fact(Fact::RunStarted)).await;

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

    // Rebuild committed truth, inject the resumed result, and drive on.
    drive_resumed(
        runtime,
        &resolved,
        &env,
        &run_id,
        &thread_id,
        &ticket,
        command.result,
        reader,
        &context,
    )
    .await
}

/// Rebuild the transcript and state from committed truth, inject a resumed
/// `result` for the parked ticket, and drive the loop to its next terminal/parked
/// checkpoint. Shared by the two resume entries — a validated `ResumeCommand`
/// (`resume_run`) and a delegate step folded back into the parent
/// (`resume_delegation`) — so the rebuild/inject/drive/finalize glue lives once.
#[allow(clippy::too_many_arguments)]
async fn drive_resumed(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    ticket: &WaitingTicket,
    result: ResumeResult,
    reader: &dyn ThreadReader,
    context: &RuntimeRunContext,
) -> Result<Phase> {
    let mut transcript = reader.committed_messages(thread_id);
    let mut store = store_from_commands(reader.committed_state(thread_id));
    let (resumed, seed_state) =
        resume_into_messages(runtime, env, run_id, ticket, result, &store, context).await;
    // The resumed tool's own state is folded into the store so a later step in
    // this resume observes the advanced state; it also seeds the attempt's batch
    // (kept first) so step commits and conflict validation cover it too.
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
    RunEvent::PermissionDecided {
        tool_id: call.tool_id.clone(),
        call_id: call.call_id.clone(),
        decision: outcome.decision_label().to_string(),
    }
    .into()
}

/// Handle a call to the reserved `tool_open` meta-tool (ADR-0053): mark the requested
/// tool opened for the rest of the run so its full schema is sent next step. The `name`
/// is reverse-mapped to its canonical id; an unknown or non-deferred name is reported
/// back to the model rather than silently accepted.
fn open_deferred_tool(
    presentation: &ToolPresentation,
    call: &ToolCall,
    opened: &mut std::collections::BTreeSet<String>,
) -> ToolOutput {
    let name = call
        .arguments
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let canonical = presentation.resolve(name);
    if presentation.is_deferred(canonical) {
        opened.insert(canonical.to_string());
        ToolOutput::ok(
            &call.call_id,
            format!("Tool `{name}` is now available; call it directly on your next step."),
        )
    } else {
        ToolOutput::error(&call.call_id, format!("no deferred tool named `{name}`"))
    }
}

/// The attempt's durable-progress accumulator. It owns the step-commit watermark
/// invariant: everything below a watermark is already durable, so a step commit
/// advances the watermark atomically with the delta it made durable, and the
/// returned checkpoint is only the tail beyond it. The model-facing `transcript`
/// (seeded with committed history) and the durable `new_messages` (only this
/// attempt's messages) grow together but stay distinct.
struct StepLedger {
    transcript: Vec<Message>,
    new_messages: Vec<Message>,
    staged_state: Vec<StateCommand>,
    /// Permission-audit drafts accumulated across the attempt's gate decisions.
    audit: Vec<EventDraft>,
    committed_messages: usize,
    committed_state: usize,
    committed_audit: usize,
    running_committed: bool,
}

impl StepLedger {
    fn new(
        transcript: Vec<Message>,
        new_messages: Vec<Message>,
        seed_state: Vec<StateCommand>,
    ) -> Self {
        Self {
            transcript,
            new_messages,
            staged_state: seed_state,
            audit: Vec::new(),
            committed_messages: 0,
            committed_state: 0,
            committed_audit: 0,
            running_committed: false,
        }
    }

    /// Append one message to both the model-facing transcript and the durable
    /// new-message accumulation — the two always grow in lock-step.
    fn push_message(&mut self, message: Message) {
        self.transcript.push(message.clone());
        self.new_messages.push(message);
    }

    /// Whether any completed step staged messages/state/audit not yet made durable.
    fn has_uncommitted(&self) -> bool {
        self.new_messages.len() > self.committed_messages
            || self.staged_state.len() > self.committed_state
            || self.audit.len() > self.committed_audit
    }

    /// Drop staged state back to the committed watermark: a batch that fails
    /// cumulative validation must not ride the final commit, but steps committed
    /// while the batch was still valid stay committed.
    fn rollback_state(&mut self) {
        self.staged_state.truncate(self.committed_state);
    }

    /// Commit the tail beyond every watermark under a `Running` fact, then advance
    /// the watermarks. `first` (the first delta) announces the phase change.
    async fn commit_delta(
        &mut self,
        context: &RuntimeRunContext,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<()> {
        commit_step_delta(
            context,
            thread_id,
            run_id,
            self.new_messages[self.committed_messages..].to_vec(),
            self.staged_state[self.committed_state..].to_vec(),
            self.audit[self.committed_audit..].to_vec(),
            !self.running_committed,
        )
        .await?;
        self.committed_messages = self.new_messages.len();
        self.committed_state = self.staged_state.len();
        self.committed_audit = self.audit.len();
        self.running_committed = true;
        Ok(())
    }

    /// The tail beyond the watermarks, ready for the final commit: the checkpoint
    /// must never re-append what step commits already made durable.
    fn into_checkpoint(mut self, end: End) -> Checkpoint {
        Checkpoint {
            new_messages: self.new_messages.split_off(self.committed_messages),
            staged_state: self.staged_state.split_off(self.committed_state),
            audit: self.audit.split_off(self.committed_audit),
            end,
        }
    }
}

/// Tracks the active plugins' live tool-face version and lazily re-resolves the
/// execution environment when it changes (e.g. an MCP server firing
/// `tools/list_changed`). A static run keeps `last_version` fixed and never
/// re-resolves, so it is zero-overhead; a failed re-resolution keeps the prior
/// environment rather than aborting the run.
struct LiveEnv {
    resolved: Option<ResolvedExecutionEnv>,
    last_version: Option<u64>,
}

impl LiveEnv {
    fn new(initial_version: Option<u64>) -> Self {
        Self {
            resolved: None,
            last_version: initial_version,
        }
    }

    /// The environment to use this step: the freshly re-resolved one if the live
    /// version advanced (best-effort), else `base`.
    fn current<'a>(
        &'a mut self,
        runtime: &Runtime,
        resolved: &ResolvedRun,
        base: &'a ResolvedExecutionEnv,
    ) -> &'a ResolvedExecutionEnv {
        let version = runtime.active_live_version(&resolved.spec.plugin_ids);
        if version != self.last_version {
            if let Ok(refreshed) = runtime.resolve_plugin_env(&resolved.spec) {
                self.resolved = Some(refreshed);
            }
            self.last_version = version;
        }
        self.resolved.as_ref().unwrap_or(base)
    }
}

/// Run one step's inference to a final response, applying two in-place recoveries
/// before yielding. A `MaxTokens`-truncated text-only step is continued in place:
/// the partial is committed as its own assistant message, a continuation prompt
/// follows it, and inference reruns on the grown transcript — up to a per-step
/// budget, after which the truncated step stands. A truncated step still carrying
/// tool calls skips recovery: those calls must be answered by tool results, not
/// another assistant step. On a clean pre-commit failure (no partial committed
/// this step) with a candidate remaining, it fails over to the next pool model;
/// the breaker inside `infer_with_retry` keys on `request.model_binding`, so a
/// failed-over model tracks its own circuit and usage. A failure after a committed
/// partial is terminal here — mid-stream recovery is the StreamCheckpoint/resume
/// path, not a switch to a different model (which would double-generate).
#[allow(clippy::too_many_arguments)]
async fn infer_step(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step_base: usize,
    step: usize,
    ledger: &mut StepLedger,
    prelude: &[Message],
    opened: &std::collections::BTreeSet<String>,
    delta_sink: &StreamDeltaSink<'_>,
    checkpoint_ref: Option<&CheckpointCtx<'_>>,
    step_resume: Option<StreamCheckpoint>,
    context: &RuntimeRunContext,
) -> std::result::Result<ChatResponse, awaken_runtime_contract::llm::Error> {
    let mut truncation_retries = 0;
    // Model-pool failover: the ordered candidate bindings (primary first, then
    // any pool fallbacks). Single-model agents yield exactly one, so this is a
    // no-op for them. `cand_idx` advances only on a clean pre-commit failure.
    let candidates = resolved.spec.candidate_bindings();
    let mut cand_idx = 0usize;
    let mut pending_resume = step_resume;
    loop {
        let mut request = build_chat_request(
            &resolved.spec,
            prelude,
            &ledger.transcript,
            &env.dynamic_descriptors(),
            opened,
        );
        request.model_binding = candidates[cand_idx].clone();
        match infer_with_retry(
            llm,
            request,
            runtime.retry_policy(),
            runtime.circuit_breaker(),
            delta_sink,
            checkpoint_ref,
            pending_resume.take(),
            &context.capture.decision,
            context.content_sink(),
            runtime.metrics(),
            context.reschedules.as_ref(),
        )
        .await
        {
            Ok(response) => {
                let truncated_text_only = response.stop_reason == Some(StopReason::MaxTokens)
                    && response.output.tool_calls().is_empty()
                    && !response.output.text_content().is_empty();
                if truncated_text_only && truncation_retries < runtime.max_continuation_retries() {
                    let partial = truncated_assistant_message(
                        run_id,
                        step_base + step,
                        truncation_retries,
                        response.output.blocks,
                    );
                    ledger.push_message(partial);
                    let prompt = continuation_message(run_id, step_base + step, truncation_retries);
                    ledger.push_message(prompt);
                    truncation_retries += 1;
                    continue;
                }
                break Ok(response);
            }
            Err(err) => {
                if truncation_retries == 0 && cand_idx + 1 < candidates.len() {
                    cand_idx += 1;
                    // A resume checkpoint belongs to the prior model's stream;
                    // a fresh candidate starts clean.
                    pending_resume = None;
                    continue;
                }
                break Err(err);
            }
        }
    }
}

/// Run the model/tool loop over a prepared transcript. Shared by fresh execution
/// and resume; the caller seeds the transcript, the already-produced messages,
/// and any state the resume itself staged (`seed_state`).
// The agent-invocation span in OTel GenAI terms: this is the reasoning loop that
// drives one agent run to completion, so it carries `gen_ai.operation.name =
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
    transcript: Vec<Message>,
    new_messages: Vec<Message>,
    step_base: usize,
    mut store: Store,
    seed_state: Vec<StateCommand>,
) -> Result<Checkpoint> {
    // The per-run model executor (ADR-0004) wins over the runtime's bound default:
    // the host resolves the run's model ref to an executor for this attempt and
    // injects it here, so a database-less worker runs the run's own configured model
    // without a local provider credential. The runtime only uses the executor — it
    // never learns how the model is reached. Absent → the runtime's session-resolved
    // executor.
    let llm = context
        .model_executor
        .clone()
        .or_else(|| runtime.llm().cloned())
        .ok_or_else(|| {
            Error::Execution("no model provider configured for this runtime".to_string())
        })?;

    // The attempt's durable-progress accumulator; it owns the step-commit
    // watermark invariant, so only the tail beyond a watermark is ever returned
    // or re-committed.
    let mut ledger = StepLedger::new(transcript, new_messages, seed_state);
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
    // Recover the run's forced-continuation count from committed truth: the steer
    // messages already in the transcript. The id type classifies its own kind, so
    // the loop reads a fact rather than matching an id-string convention by hand.
    let mut forced_continuations = ledger
        .transcript
        .iter()
        .filter(|message| message.id.is_steer_of(run_id))
        .count();

    // The agent's configured ceiling guards against a non-terminating tool cycle;
    // a natural-end text step ends the loop earlier. A plugin whose tool set is
    // dynamic (e.g. an MCP server firing `tools/list_changed`) advances its
    // `live_version`; `live_env` re-resolves the environment at the step boundary
    // when it changes, and is zero-overhead for static runs.
    let mut live_env = LiveEnv::new(runtime.active_live_version(&resolved.spec.plugin_ids));
    // Failed inference steps in a row (post-retry). The runtime's tolerance
    // decides when the streak is terminal; a success resets it.
    let mut consecutive_inference_failures = 0usize;
    // Deferred tools (ADR-0053) the model has opened via `tool_open` this run, by
    // canonical id: once opened, a tool's full schema is sent on subsequent steps.
    let mut opened: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
        if context.commit.is_some() && ledger.has_uncommitted() {
            if validate_batch(&ledger.staged_state).is_err() {
                ledger.rollback_state();
                end = Some(End::Ended(EndCause::Error(Failure::StateConflict)));
                break;
            }
            ledger.commit_delta(context, thread_id, run_id).await?;
        }

        if context.is_cancelled() {
            end = Some(End::Ended(EndCause::Cancelled));
            break;
        }

        // The environment to drive this step: the live-refreshed one when a
        // dynamic plugin's tool face changed, else the resolved base.
        let env = live_env.current(runtime, resolved, env);

        // Phase hooks stage state (G9/G30). A BeforeInference hook may also inject
        // request-only context (e.g. recalled memories), prepended to this
        // inference and never committed.
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepStart,
            &ledger.transcript,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::BeforeInference,
            &ledger.transcript,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;
        // Request-only context the `BeforeInference` hooks wrote to state: the
        // kernel reads it here (single chokepoint) and prepends the flattened
        // per-producer blocks to this inference, never committing them (ADR-0055).
        let prelude: Vec<Message> = ContextMessages::load_or_default(&store)
            .into_values()
            .flatten()
            .collect();

        // A persisted partial resumes only this drive's first step (see above);
        // it is consumed on the first inference call of that step.
        let step_resume = if step == 0 {
            resume_checkpoint.take()
        } else {
            None
        };
        let checkpoint_ref = checkpoint_ctx.as_ref();
        let inference = infer_step(
            &llm,
            runtime,
            resolved,
            env,
            run_id,
            step_base,
            step,
            &mut ledger,
            &prelude,
            &opened,
            &delta_sink,
            checkpoint_ref,
            step_resume,
            context,
        );
        // A cancel aborts a hung or long inference in flight rather than
        // waiting for the step boundary. Dropping the inference future may
        // abandon a half-open breaker probe; recording that reopens the
        // circuit for a later re-probe without polluting the failure count.
        let inference = match &context.cancellation {
            Some(token) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    result = inference => Some(result),
                }
            }
            None => Some(inference.await),
        };
        let Some(inference) = inference else {
            runtime
                .circuit_breaker()
                .record_abandoned_probe(&resolved.spec.model_binding.model_ref, runtime.metrics());
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

        // Record this step's token usage as committed thread truth, attributed to the
        // bound model and accumulated across steps and runs, so an adapter can surface
        // a session's usage (total or per-model) by reading thread state — the runtime
        // records the fact without naming any wire, and it survives a restart. A step
        // whose provider reported no usage records nothing.
        if let Some(step_usage) = response.usage {
            fold_thread_usage(
                &mut store,
                &mut ledger.staged_state,
                "thread usage state drifted; skipping record",
                |usage| usage.record(&resolved.spec.model_binding.model_ref, step_usage),
            );
        }

        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::AfterInference,
            &ledger.transcript,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;

        // A cancel may have landed while inference was in flight; observe it at
        // this step boundary and discard the model output.
        if context.is_cancelled() {
            end = Some(End::Ended(EndCause::Cancelled));
            break;
        }

        // Commit the assistant step verbatim — text and tool-use blocks may
        // interleave. The text already streamed to the live sink; the committed
        // message is the assembled whole.
        // Reverse the model-facing tool id back to its canonical id (ADR-0053) at the
        // single ingress, BEFORE any gate / delegation / dispatch / audit — so an alias
        // (or an MCP-tool alias) never leaks past this boundary; every internal consumer
        // sees the canonical id. Identity when the agent has no tool presentation.
        let calls: Vec<ToolCall> = response
            .output
            .tool_calls()
            .into_iter()
            .map(|mut call| {
                call.tool_id = resolved
                    .spec
                    .tool_presentation
                    .resolve(&call.tool_id)
                    .to_string();
                call
            })
            .collect();
        let assistant = assistant_message(run_id, step_base + step, response.output.blocks);
        ledger.push_message(assistant);

        // A text-only step (no tool requests) is a natural end — unless queued
        // live input or a run-end guard keeps the loop going. Queued input is
        // drained first: a message the caller already addressed to this run
        // preempts any guard's end-of-run verdict, exactly as if it had arrived
        // one step earlier.
        if calls.is_empty() {
            // The safe loop boundary (ADR-0054): drain queued live input, honour an
            // operator pause, else fall through to the run-end guard. Shared with
            // every executor via the kernel `evaluate_boundary`.
            match evaluate_boundary(context, run_id, &ledger.transcript) {
                BoundaryOutcome::Continue { fold } => {
                    for message in fold {
                        ledger.push_message(message);
                    }
                    continue;
                }
                BoundaryOutcome::Park { fold, reason } => {
                    // Commit the drained input first (so an in-flight steer is not
                    // lost), then park on a no-tool ticket — an operator pause,
                    // resumed by an explicit resume, not a tool result.
                    for message in fold {
                        ledger.push_message(message);
                    }
                    end = Some(End::Parked(Box::new(pause_ticket(
                        resolved, run_id, reason,
                    ))));
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Waiting {
                            pending_tool_use_id: None,
                        }),
                    )
                    .await;
                    break;
                }
                BoundaryOutcome::Idle => {}
            }
            // The runtime owns *when* the loop stops; a guard supplies the
            // predicate and any feedback. Its `detail` is opaque
            // (anti-corruption): the kernel forwards it, never interprets it.
            match consult_run_end(
                env,
                run_id,
                &ledger.transcript,
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
                    ledger.audit.push(continuation_event(&detail));
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Continuation {
                            steered: true,
                            detail,
                        }),
                    )
                    .await;
                    let message = feedback_message(run_id, forced_continuations, feedback);
                    ledger.push_message(message);
                    forced_continuations += 1;
                    continue;
                }
                RunEndOutcome::Complete { detail } => {
                    ledger.audit.push(continuation_event(&detail));
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Continuation {
                            steered: false,
                            detail,
                        }),
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

        // Otherwise run each requested tool and feed the results back. A call that
        // parks or fails the run returns the terminal `End` here, ending the step
        // loop; every call answered with a result returns `None` and the loop goes on.
        if let Some(reached) = dispatch::run_tool_calls(
            runtime,
            context,
            resolved,
            env,
            run_id,
            step,
            calls,
            &mut ledger.transcript,
            &mut ledger.new_messages,
            &mut ledger.staged_state,
            &mut ledger.audit,
            &mut store,
            &mut opened,
        )
        .await
        {
            end = Some(reached);
            break;
        }

        // StepEnd fires at the boundary between steps that continue the loop.
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepEnd,
            &ledger.transcript,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;
    }

    // No early terminus means the loop exhausted its step ceiling.
    let end = end.unwrap_or(End::Ended(EndCause::MaxSteps));
    Ok(ledger.into_checkpoint(end))
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
    // `first` is the per-step boundary's phase transition (nothing → Running); a
    // later increment stays Running and emits no RunPhaseChanged.
    coordinator
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            run_id.clone(),
            Phase::Running,
            first,
            messages,
            state,
            None,
            audit,
        ))
        .await
        .map_err(|err| Error::Commit(err.to_string()))?;
    Ok(())
}

/// Forwards a step's streamed text chunks to the live stream as `OutputText`
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
            AgentEvent::Delta(Delta::TextDelta {
                delta: chunk.to_string(),
            }),
        )
        .await;
    }

    async fn on_tool_call_delta(&self, call_id: &str, tool_id: &str, args_delta: &str) {
        emit(
            self.context,
            self.run_id,
            AgentEvent::Delta(Delta::ToolCallDelta {
                id: call_id.to_string(),
                name: tool_id.to_string(),
                args_delta: args_delta.to_string(),
            }),
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
/// arguments arrive as de-accumulated suffix fragments (see `on_tool_call_delta`)
/// which are appended into raw JSON text; whether it parses later decides
/// completed-vs-in-flight.
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

    async fn on_tool_call_delta(&self, call_id: &str, tool_id: &str, args_delta: &str) {
        // The provider adapter now hands a suffix `args_delta`, so accumulate them
        // per call id to rebuild the running raw JSON — a later parse decides whether
        // the call had finished when a mid-stream drop lands mid-arguments.
        {
            let mut tools = self.tools.lock().expect("continuation tools");
            match tools.iter_mut().find(|t| t.call_id == call_id) {
                Some(existing) => {
                    existing.tool_id = tool_id.to_string();
                    existing.raw_arguments.push_str(args_delta);
                }
                None => tools.push(PartialToolCall {
                    call_id: call_id.to_string(),
                    tool_id: tool_id.to_string(),
                    raw_arguments: args_delta.to_string(),
                }),
            }
        }
        self.inner
            .on_tool_call_delta(call_id, tool_id, args_delta)
            .await;
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

/// Synthesize the step the model had produced when a drop interrupted it after
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
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) {
    // The tool-result phase carries per-call data and runs via
    // `collect_tool_reactions`, never here.
    let kind = match point {
        PhaseHookPoint::StepStart => PhaseKind::StepStart,
        PhaseHookPoint::BeforeInference => PhaseKind::BeforeInference,
        PhaseHookPoint::AfterInference => PhaseKind::AfterInference,
        PhaseHookPoint::StepEnd => PhaseKind::StepEnd,
        PhaseHookPoint::AfterTool => {
            unreachable!("AfterTool phase hooks run via collect_tool_reactions")
        }
    };
    for hook in env.hooks_for(point) {
        let ctx = PhaseContext {
            run_id: run_id.clone(),
            step,
            kind: kind.clone(),
        };
        let reaction = hook.on_phase(&ctx, conversation, store).await;
        // Apply the reaction's state to the live store before the next hook, so a
        // once-per-run hook that gates on its own run-scoped key sees its own
        // earlier write and replays instead of recomputing (ADR-0055) — the same
        // apply-then-stage discipline the tool-outcome path uses. Request-only
        // context is written to `ContextMessages` here; the kernel reads it at
        // request assembly (a phase hook never returns request-only messages).
        for command in &reaction.state {
            store.apply(command);
        }
        staged_state.extend(reaction.state);
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
/// step). Their `detail` is opaque to the runtime (G2 neutrality) — forwarded,
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
    RunEvent::Continuation {
        detail: detail.clone(),
    }
    .into()
}

/// The user message a steered continuation injects before looping. Its id is
/// run-scoped and distinct from the step-based assistant/tool message ids.
fn feedback_message(run_id: &RunId, nth: usize, feedback: String) -> Message {
    Message {
        id: MessageId::steer(run_id, nth),
        role: Role::User,
        content: vec![ContentBlock::text(feedback)],
    }
}

/// A no-tool waiting ticket for an operator pause (ADR-0054): the run parks with
/// no pending tool and no call id, correlated by run id, resumed by an explicit
/// operator resume rather than a tool result. The drain/re-identify discipline
/// this used to sit next to now lives in `awaken-runtime-contract::boundary`.
fn pause_ticket(resolved: &ResolvedRun, run_id: &RunId, reason: WaitingReason) -> WaitingTicket {
    WaitingTicket {
        correlation_id: run_id.0.clone(),
        run_id: run_id.clone(),
        thread_id: ThreadId(String::new()), // filled in finish via the commit thread id
        snapshot_id: resolved.snapshot_id.0.clone(),
        catalog_fingerprint: resolved.spec.catalog_fingerprint.0.clone(),
        reason,
        call_id: None,
        pending_tool: None,
        deadline_ms: None,
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
        // Only remote (A2A) delegates park and resume, and their token spend is not
        // observable over the wire — so there is no usage to fold in here.
        Ok(AgentStep::Done { text, .. }) => {
            ResumeResult::ToolResult(ToolOutput::ok(&call_id, text))
        }
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

    // The delegate's step, folded back as the delegate tool's replayed result,
    // rebuilds committed truth and drives on — the same glue as a direct resume.
    drive_resumed(
        runtime, resolved, env, run_id, thread_id, ticket, synthetic, reader, context,
    )
    .await
}

/// Turn a resumed result into the tool/user message(s) and any staged state.
/// An `allow` decision executes the pending tool now; a `deny` feeds a blocked
/// result; a `ToolResult`/`Input` is used directly.
async fn resume_into_messages(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    ticket: &WaitingTicket,
    result: ResumeResult,
    store: &Store,
    context: &RuntimeRunContext,
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
            let call = pending_call(&output);
            fold_resume_tool_output(env, run_id, &call_id, call, &output, store).await
        }
        ResumeResult::Decision { allow, note } => {
            if allow && let Some(pending) = &ticket.pending_tool {
                let call = ToolCall {
                    call_id: call_id.clone(),
                    tool_id: pending.tool_id.clone(),
                    arguments: pending.arguments.clone(),
                };
                let output = execute_tool(runtime, None, &call, context).await;
                fold_resume_tool_output(env, run_id, &call_id, Some(call), &output, store).await
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
                MessageId::resume_input(&call_id),
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

/// Fold a resumed tool result into `(messages, state)` off a throwaway store copy
/// (the resume paths return staged effects rather than mutating the live store).
/// The result message is minted from `call_id`; when a `call` is present (the
/// replayed call the hooks advance on) its output state is applied to a `work`
/// clone and the `AfterTool` reactions/reminders are folded in. The one home for
/// the resume arms' outcome folding (`ToolResult` and an allowed `Decision`).
async fn fold_resume_tool_output(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    call_id: &str,
    call: Option<ToolCall>,
    output: &ToolOutput,
    store: &Store,
) -> (Vec<Message>, Vec<StateCommand>) {
    let mut state = output.state.clone();
    let mut messages = vec![tool_result_message_from(call_id, &output.content)];
    if let Some(call) = call {
        let mut work = store.clone();
        for command in &output.state {
            work.apply(command);
        }
        let (reactions, reminders) =
            collect_tool_reactions(env, run_id, 0, &call, output, &work).await;
        state.extend(reactions);
        messages.extend(reminders);
    }
    (messages, state)
}

/// Consult the `AfterTool` phase hooks for one executed call (ADR-0055 folds the
/// former `ToolOutcomeHook` into the phase-hook model). `state` must already
/// reflect the tool's own output state; each hook reads the folded state and the
/// executed call/output on `PhaseContext::after_tool`, and may stage further state
/// and reminder messages. Returns the hooks' additional state commands and
/// messages (the tool's own output state is staged by the caller).
async fn collect_tool_reactions(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step: usize,
    call: &ToolCall,
    output: &ToolOutput,
    state: &Store,
) -> (Vec<StateCommand>, Vec<Message>) {
    let mut work = state.clone();
    let mut commands: Vec<StateCommand> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    let ctx = PhaseContext {
        run_id: run_id.clone(),
        step,
        kind: PhaseKind::AfterTool(AfterToolContext {
            call: call.clone(),
            output: output.clone(),
        }),
    };
    for hook in env.hooks_for(PhaseHookPoint::AfterTool) {
        let reaction = hook.on_phase(&ctx, &[], &work).await;
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

/// Fold a delegate's accumulated [`ThreadUsage`] into the parent thread's committed
/// tally under [`THREAD_USAGE_STATE_KEY`]. Load-merge-store on the live `store` plus
/// the staged batch, exactly like the per-step inference recording — so the parent
/// thread's usage (and thus a session's) counts sub-agent tokens. A no-op when the
/// delegate reported none (a remote delegate or a deterministic model).
/// Load-merge-store the thread-usage cell fail-closed (ADR-0055): read the
/// committed tally, apply `mutate`, write it back to the live `store` and stage
/// the command. On a shape drift it logs `drift` and leaves the tally untouched
/// (never resets an accumulated total). The one place the usage cell is folded —
/// both per-step recording and sub-agent rollup go through here.
fn fold_thread_usage(
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
    drift: &str,
    mutate: impl FnOnce(&mut ThreadUsage),
) {
    match ThreadUsageKey::load(store) {
        Ok(mut usage) => {
            mutate(&mut usage);
            let usage_cmd = ThreadUsageKey::write(&usage);
            store.apply(&usage_cmd);
            staged_state.push(usage_cmd);
        }
        Err(error) => tracing::error!(%error, "{drift}"),
    }
}

fn merge_thread_usage(
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
    delta: &ThreadUsage,
) {
    if delta.is_empty() {
        return;
    }
    fold_thread_usage(
        store,
        staged_state,
        "thread usage state drifted; skipping sub-agent rollup",
        |usage| usage.merge(delta),
    );
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
    context: &RuntimeRunContext,
) -> ToolOutput {
    let span = tracing::Span::current();
    span.record(
        "otel.name",
        format!("execute_tool {}", call.tool_id).as_str(),
    );
    // ADR-0044 D1: the kernel calls a `ToolExecutor`; where the call runs is the
    // executor's concern. Absent a wired executor, the degenerate in-process
    // `LocalToolExecutor` reproduces the historical behavior exactly.
    let local;
    let executor: &dyn ToolExecutor = match context.tool_executor.as_deref() {
        Some(executor) => executor,
        None => {
            local = LocalToolExecutor { runtime, env };
            &local
        }
    };
    let started = std::time::Instant::now();
    // Fault isolation at the port boundary: a `RawTool` (MCP / plugin / skill — often
    // third-party) that PANICS must not take down the run. Catch the unwind here, at
    // the single execute-tool confluence, and map it to a model-visible error just like
    // an `Err` — so unknown/invalid-args/execution/panic all fail closed identically.
    use futures_util::FutureExt;
    let output = match std::panic::AssertUnwindSafe(executor.invoke(call))
        .catch_unwind()
        .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => ToolOutput::error(&call.call_id, err.to_string()),
        Err(_panic) => ToolOutput::error(
            &call.call_id,
            format!("tool `{}` panicked during execution", call.tool_id),
        ),
    };
    // OTel: a tool that returned an error (unknown tool, invocation failure, or a
    // model-visible error result) marks the span ERROR with a `gen_ai`-shaped type.
    if output.is_error {
        span.record("error.type", "tool_error");
        span.record("otel.status_code", "ERROR");
    }
    // Structure-only metric at the tool chokepoint (#2): tool id + outcome class +
    // latency. The tool id is a declared identifier, never content.
    runtime.metrics().record_tool(
        &call.tool_id,
        if output.is_error { "error" } else { "ok" },
        started.elapsed(),
    );
    output
}

/// The in-process `ToolExecutor` (ADR-0044 D1): the degenerate case where the
/// hand runs in the brain's own process. It resolves the call by id against the
/// run's dynamic tools and the runtime registry — the historical lookup — and is
/// used whenever a run wires no remote executor. Borrowing keeps it allocation-
/// free per call; it is invoked directly, never as a stored `dyn`.
struct LocalToolExecutor<'a> {
    runtime: &'a Runtime,
    env: Option<&'a ResolvedExecutionEnv>,
}

#[async_trait::async_trait]
impl ToolExecutor for LocalToolExecutor<'_> {
    async fn invoke(&self, call: &ToolCall) -> std::result::Result<ToolOutput, ToolError> {
        let tool = self
            .env
            .and_then(|env| env.dynamic_tool(&call.tool_id))
            .or_else(|| self.runtime.tool(&call.tool_id).cloned());
        match tool {
            Some(tool) => tool.invoke(call.clone()).await,
            None => Err(ToolError::Unknown(call.tool_id.clone())),
        }
    }
}

/// Best-effort live emission. A sink failure is swallowed: committed truth is
/// authoritative, not the live stream (G10/G13).
async fn emit(context: &RuntimeRunContext, run_id: &RunId, kind: AgentEvent) {
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
        // The finish boundary always transitions phase (to Ended/Waiting); the
        // permission-audit drafts ride the same commit as the run's facts (G1).
        let commit = ThreadCommit::assemble(
            thread_id.clone(),
            run_id.clone(),
            phase.clone(),
            true,
            new_messages,
            staged_state,
            waiting,
            audit,
        );
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
            AgentEvent::Fact(Fact::RunFailed {
                code: failure.code().to_string(),
                message: failure.message(),
            }),
        )
        .await;
    }
    emit(
        context,
        &run_id,
        AgentEvent::Fact(Fact::RunFinished { exhausted: false }),
    )
    .await;
    Ok(phase)
}

fn map_resolver_error(err: resolver::Error) -> Error {
    Error::Resolution(err.to_string())
}

#[cfg(test)]
mod tests;
