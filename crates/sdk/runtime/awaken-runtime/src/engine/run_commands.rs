// @formal entrypoint checkpoint.recovery_never_precedes_commit
// @formal entrypoint tool.publication_barrier
// @formal entrypoint tool_result.call_is_consumed_at_most_once
// @formal entrypoint tool_result.resume_receipt_is_atomic_with_ticket_consumption
// @formal entrypoint tool_result.terminal_run_rejects_late_result
//! Run command handling for native execution, resume, cancellation, stopping,
//! and committed scheduled actions.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[derive(Debug, thiserror::Error)]
#[error("client-executed tool results cannot mutate Runtime state")]
struct ClientStateMutation;

/// Caller-owned result content carries no in-process state-writer capability.
fn validate_client_state_authority(ticket: &ResumeTicket, result: &ResumeResult) -> Result<()> {
    if matches!(
        ticket.target(),
        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
            reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution,
            ..
        }
    ) && let ResumeResult::ToolResult(output) = result
        && !output.state.is_empty()
    {
        return Err(awaken_runtime_contract::diagnostic::RuntimeDiagnostic::new(
            awaken_runtime_contract::diagnostic::RuntimeErrorKind::InvalidRequest,
            "resume_client_state_mutation",
            ClientStateMutation,
        )
        .into());
    }
    Ok(())
}

#[async_trait]
impl RunExecutor for Runtime {
    #[tracing::instrument(name = "runtime.run", skip_all, fields(awaken.run.id = %activation.run_id.0))]
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        crate::configuration::validate_configuration(self)?;
        let context = activation.narrow_context(context);
        // Execution is the single ingress for fresh activations, including durable
        // dispatch. Retain the exact immutable snapshot before the run can await so
        // an in-process resume resolves the same value by id.
        self.register_snapshot(activation.snapshot.clone());
        // Physical-attempt ownership is established by the direct driver or the
        // claimed Worker before entering this executor. Keeping the registration
        // outside the native loop prevents nested generations and gives ACP/A2A
        // the same lifecycle boundary.
        run_agent_loop(self, activation, context, ResumeContinuation::NativeLoop).await
    }
}

#[async_trait]
impl RunAttemptExecutor for Runtime {
    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        resume_activated(
            self,
            activation,
            command,
            context,
            ResumeContinuation::NativeLoop,
        )
        .await
    }
}

/// A model-free owned continuation supplied with its immutable activation.
/// This state-free command role uses the same activated admission and existing
/// continuation kernel; it adds no responsibility to the Runtime service.
pub async fn resume_activated_tool_batch(
    runtime: &Runtime,
    activation: RunActivation,
    command: ResumeCommand,
    context: RuntimeRunContext,
) -> Result<RunState> {
    resume_activated(
        runtime,
        activation,
        command,
        context,
        ResumeContinuation::ToolBatch,
    )
    .await
}

/// Activated Native and model-free roles share one input admission before the
/// existing continuation kernel. Neither adapter overwrites live sources first.
pub(crate) async fn resume_activated(
    runtime: &Runtime,
    activation: RunActivation,
    command: ResumeCommand,
    context: RuntimeRunContext,
    continuation: ResumeContinuation,
) -> Result<RunState> {
    // The common resume binder checks the live source before applying the
    // activation. An eager overlay here would erase a conflicting source.
    let reader = context.reader.clone().ok_or_else(|| {
        Error::Execution("RunAttemptExecutor::resume requires committed history".to_string())
    })?;
    if let Some(ticket) = reader.resume_ticket(&command.run_id) {
        validate_activation_toolsets(&activation, &ticket)?;
    }
    // Resume is the single ingress for an activated retry. A rebuilt Runtime
    // starts without its predecessor's in-memory snapshot registry, so retain
    // the exact immutable snapshot carried by this activation before resolving
    // the committed resume ticket.
    resume_with_continuation(
        runtime,
        command,
        reader.as_ref(),
        context,
        continuation,
        Some(&activation),
    )
    .await
}

/// Bind the supplied immutable value to the existing ticket. The SDK uses
/// this same check before its callback; the continuation uses it before entry.
/// Snapshot/fingerprint retain the SDK's existing diagnostic precedence.
pub(crate) fn validate_resume_activation(
    activation: &RunActivation,
    ticket: &ResumeTicket,
) -> Result<()> {
    use awaken_runtime_contract::resume::ResumeError;
    if ticket.snapshot_id != activation.snapshot.id.0 {
        return Err(ResumeError::SnapshotMismatch.into());
    }
    if ticket.catalog_fingerprint != activation.snapshot.fingerprint.0 {
        return Err(ResumeError::FingerprintMismatch.into());
    }
    if ticket.run_id != activation.run_id {
        return Err(ResumeError::RunMismatch.into());
    }
    if ticket.thread_id != activation.thread_id {
        return Err(ResumeError::ThreadMismatch.into());
    }
    Ok(())
}

fn validate_activation_toolsets(activation: &RunActivation, ticket: &ResumeTicket) -> Result<()> {
    if ticket.inherited_toolsets.as_ref() != activation.inherited_toolsets() {
        return Err(Error::Execution(
            "resumed activation tool configuration differs from the committed ticket".into(),
        ));
    }
    Ok(())
}

/// Cancel a run that is not executing (a queued or awaiting run) by committing a
/// terminal `Cancelled` fact through the one finish boundary (G31). This clears
/// any awaiting ticket, so an awaiting run can no longer be resumed. An in-flight run
/// is cancelled cooperatively through `LiveRunControl` instead, not here.
pub(crate) async fn cancel_run(
    runtime: &Runtime,
    run_id: RunId,
    thread_id: ThreadId,
    context: RuntimeRunContext,
) -> Result<RunState> {
    let step = RunStepResult::cancelled(run_id.clone());
    finish_control(runtime, &context, &thread_id, run_id, step).await
}

/// Cancel a claimed activation before its accepted input was necessarily
/// committed by the fenced execution owner. Already-committed messages are
/// removed by identity; the remaining input and terminal Cancelled fact commit
/// atomically through the same finish boundary as ordinary execution.
pub(crate) async fn cancel_activation(
    runtime: &Runtime,
    activation: RunActivation,
    context: RuntimeRunContext,
) -> Result<RunState> {
    let run_id = activation.run_id;
    let thread_id = activation.thread_id;
    let (_, fresh_input) =
        committed_history_and_fresh_input(&context, &thread_id, activation.input);
    let step = RunStepResult::ended_with_messages(run_id.clone(), EndCause::Cancelled, fresh_input);
    finish_control(runtime, &context, &thread_id, run_id, step).await
}

/// Resolve an externally blocked coordinated child without another inference
/// step. Every unfinished call in the authoritative ToolBatch receives the
/// coordinated-child interruption result, the complete ordered result batch and
/// `NaturalEnd` are committed together, and exact terminal replay is a no-op.
pub(crate) async fn interrupt_awaiting_tools(
    runtime: &Runtime,
    run_id: RunId,
    thread_id: ThreadId,
    context: RuntimeRunContext,
) -> Result<RunState> {
    const INTERRUPTED: &str = "Tool execution was interrupted before completion. Please retry.";

    let reader = context.reader.as_ref().ok_or_else(|| {
        Error::Execution(
            "interrupting an awaiting tool batch requires committed history".to_string(),
        )
    })?;
    let committed_state = reader.committed_state(&thread_id);
    // Even terminal replay is only a logical observation, not stop evidence.
    let (store, _) =
        model_operation::recover_checkpoint(committed_state, &context, &run_id, &thread_id).await;
    match reader.run_state(&run_id) {
        Some(state @ RunState::Ended(_)) => return Ok(state),
        Some(RunState::Awaiting) => {}
        Some(RunState::Running) | None => {
            return Err(Error::Execution(
                "coordinated tool interruption requires an awaiting Run".to_string(),
            ));
        }
    }
    let ticket = reader.resume_ticket(&run_id);
    let batch = ActiveToolBatch::load(&store);
    let coherence = match (&batch, ticket.as_ref()) {
        (Err(error), _) => Err(format!("unreadable ActiveToolBatch: {error}")),
        (Ok(None), _) => Err("missing ActiveToolBatch".to_string()),
        (Ok(Some(_)), None) => Err("missing ResumeTicket".to_string()),
        (Ok(Some(batch)), Some(ticket)) => batch
            .validate_awaiting_ticket(&thread_id, ticket)
            .map_err(|error| format!("incoherent ResumeTicket: {error}"))
            .and_then(|_| {
                matches!(
                    ticket.reason(),
                    AwaitReason::ToolPermission | AwaitReason::ExternalEvent
                )
                .then_some(())
                .ok_or_else(|| {
                    "ResumeTicket does not describe an interruptible external wait".to_string()
                })
            }),
    };
    if let Err(detail) = coherence {
        tracing::error!(
            awaken.run.id = %run_id.0,
            awaken.thread.id = %thread_id.0,
            %detail,
            "quarantining an Awaiting tool Run whose durable interruption facts are corrupt"
        );
        // Removing the typed active-cell projection before `finish` lets the
        // canonical terminal boundary run even when that cell cannot deserialize.
        // The corrupt command remains in the append-only state log for audit; the
        // terminal ThreadCommit atomically consumes the waiting row and emits the
        // existing RunStateChanged(StateConflict) fact.
        let step = RunStepResult {
            new_messages: Vec::new(),
            staged_state: vec![ActiveToolBatch::remove()],
            audit: Vec::new(),
            resume: None,
            disposition: RunDisposition::ended(
                run_id.clone(),
                EndCause::Error(Failure::StateConflict),
            ),
        };
        return finish(runtime, &context, &thread_id, run_id, step).await;
    }
    let mut batch = batch
        .expect("batch coherence checked typed load")
        .expect("batch coherence checked presence");
    let calls = batch
        .calls()
        .iter()
        .map(|entry| (entry.call.clone(), entry.phase.clone()))
        .collect::<Vec<_>>();
    for (call, phase) in calls {
        let output = ToolOutput::error(&call.call_id, INTERRUPTED);
        match phase {
            ToolCallPhase::Requested => batch
                .complete_immediate(output.clone())
                .map_err(|error| Error::Execution(error.to_string()))?,
            ToolCallPhase::Executing { .. } | ToolCallPhase::Awaiting { .. } => batch
                .complete(output.clone())
                .map_err(|error| Error::Execution(error.to_string()))?,
            ToolCallPhase::Returned(_) | ToolCallPhase::KnownAborted { .. } => {
                return Err(Error::Execution(
                    "awaiting interruption cannot replace a known unmaterialized result".into(),
                ));
            }
            ToolCallPhase::Completed(_) | ToolCallPhase::Indeterminate { .. } => continue,
        }
        let result_message = tool_result_message(&batch, &call, &output)?;
        batch
            .set_result_messages(&call.call_id, vec![result_message])
            .map_err(|error| Error::Execution(error.to_string()))?;
    }
    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    let messages = batch
        .publication()
        .map_err(|error| Error::Execution(error.to_string()))?
        .messages()
        .cloned()
        .collect();
    let step = RunStepResult {
        new_messages: messages,
        staged_state: vec![ActiveToolBatch::write(&Some(batch))],
        audit: Vec::new(),
        resume: None,
        disposition: RunDisposition::ended(run_id.clone(), EndCause::NaturalEnd),
    };
    finish(runtime, &context, &thread_id, run_id, step).await
}

/// Stop a run by committing a terminal `Stopped(reason)` fact through the one
/// finish boundary (G31) — a host stop policy (budget, step ceiling) making the
/// run terminal. Like cancel, it clears any awaiting ticket, so a later resume or
/// scheduled result for the run fails closed (RS-CTRL-002, ADR-0026).
pub(crate) async fn stop_run(
    runtime: &Runtime,
    run_id: RunId,
    thread_id: ThreadId,
    reason: String,
    context: RuntimeRunContext,
) -> Result<RunState> {
    let step = RunStepResult::stopped(run_id.clone(), reason);
    finish_control(runtime, &context, &thread_id, run_id, step).await
}

/// A control outcome can end a logical Run, never supply missing execution
/// evidence. Restore the same veto as execution before committing cancel/stop.
async fn finish_control(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    step: RunStepResult,
) -> Result<RunState> {
    let committed_state = context
        .reader
        .as_ref()
        .map(|reader| reader.committed_state(thread_id))
        .unwrap_or_default();
    let _ = model_operation::recover_checkpoint(committed_state, context, &run_id, thread_id).await;
    finish(runtime, context, thread_id, run_id, step).await
}

/// Perform a committed `ScheduledAction` (ADR-0020): the run is awaiting on a
/// ticket whose reason is `ScheduledAction`, holding the deferred action as its
/// pending tool. Performing it is an allow-resume of that committed action — the
/// system runs the action and commits the resumed outcome, validated against the
/// committed request and idempotent (RS-SCH-001/004). A run awaiting for any other
/// reason, or not awaiting at all, fails closed.
pub(crate) async fn perform_scheduled_action(
    runtime: &Runtime,
    run_id: &RunId,
    reader: &dyn CommittedThreadView,
    context: RuntimeRunContext,
    now_ms: u64,
) -> Result<RunState> {
    let ticket = reader
        .resume_ticket(run_id)
        .ok_or_else(|| Error::Execution("run is not awaiting".to_string()))?;
    if ticket.reason() != AwaitReason::ScheduledAction {
        return Err(Error::Execution(
            "run is not awaiting on a scheduled action".to_string(),
        ));
    }
    let command = ResumeCommand {
        correlation_id: ticket.correlation_id,
        run_id: ticket.run_id,
        thread_id: ticket.thread_id,
        snapshot_id: ExecutableAgentSnapshotId(ticket.snapshot_id),
        catalog_fingerprint: CatalogFingerprint(ticket.catalog_fingerprint),
        result: ResumeResult::allow(),
        operation_id: None,
        context_messages: Vec::new(),
        now_ms,
    };
    resume_run(runtime, command, reader, context).await
}

/// Resume an awaiting run: validate the resume against the committed ticket, rebuild
/// the transcript from committed messages, inject the resumed result, and drive
/// the loop to a new terminal/awaiting state (G5/G28).
pub(crate) async fn resume_run(
    runtime: &Runtime,
    command: ResumeCommand,
    reader: &dyn CommittedThreadView,
    context: RuntimeRunContext,
) -> Result<RunState> {
    resume_with_continuation(
        runtime,
        command,
        reader,
        context,
        ResumeContinuation::NativeLoop,
        None,
    )
    .await
}

pub(crate) async fn resume_tool_batch(
    runtime: &Runtime,
    command: ResumeCommand,
    reader: &dyn CommittedThreadView,
    context: RuntimeRunContext,
) -> Result<RunState> {
    resume_with_continuation(
        runtime,
        command,
        reader,
        context,
        ResumeContinuation::ToolBatch,
        None,
    )
    .await
}

fn require_tool_resume_commit(context: &RuntimeRunContext) -> Result<()> {
    if context.commit.is_none() {
        return Err(Error::Execution(
            "tool-batch handoff requires a durable commit coordinator".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn recover_tool_batch(
    runtime: &Runtime,
    activation: RunActivation,
    context: RuntimeRunContext,
) -> Result<RunState> {
    crate::configuration::validate_configuration(runtime)?;
    let context = activation.narrow_context(context);
    let reader = context
        .reader
        .as_ref()
        .ok_or_else(|| Error::Execution("tool-batch recovery requires committed history".into()))?;
    if context.commit.is_none() || reader.run_state(&activation.run_id) != Some(RunState::Running) {
        return Err(Error::Execution(
            "tool-batch recovery requires a durably Running Run and commit coordinator".into(),
        ));
    }
    runtime.register_snapshot(activation.snapshot.clone());
    run_agent_loop(runtime, activation, context, ResumeContinuation::ToolBatch).await
}

/// Complete context binding after the current ticket/command validation.
/// This preserves live-source refusal, ticket attribution, supplied capability
/// overlay and exact process-local snapshot registration as one synchronous phase.
fn bind_resume_context(
    runtime: &Runtime,
    mut context: RuntimeRunContext,
    ticket: &ResumeTicket,
    activation: Option<&RunActivation>,
) -> Result<RuntimeRunContext> {
    if context.inherited_toolsets.is_some()
        && context.inherited_toolsets != ticket.inherited_toolsets
    {
        return Err(Error::Execution(
            "resume tool configuration differs from the committed ticket".into(),
        ));
    }
    context.inherited_toolsets = ticket.inherited_toolsets.clone();
    if let Some(activation) = activation {
        validate_resume_activation(activation, ticket)?;
        // An explicit supplied source is immutable. SDK source absence retains
        // the original bare context restoration above; it is not new history.
        if activation.inherited_toolsets().is_some() {
            validate_activation_toolsets(activation, ticket)?;
        }
        let inherited = context.inherited_toolsets.clone();
        context = activation.narrow_context(context);
        // Ticket attribution already passed the original live-context check.
        // Applying a capability overlay must not erase that frozen input.
        context.inherited_toolsets = inherited;
        runtime.register_snapshot(activation.snapshot.clone());
    }
    Ok(context)
}

/// Join the same frozen state with the ticket before either resume route can
/// enter a child or tool. The aggregate remains the sole coherence validator.
fn load_resumed_tool_batch(
    thread_id: &ThreadId,
    ticket: &ResumeTicket,
    store: &Store,
) -> Result<Option<ToolBatch>> {
    if ticket.tool_call().is_none() {
        return Ok(None);
    }
    let batch = ActiveToolBatch::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .ok_or_else(|| {
            Error::Execution("tool resume is missing its committed tool batch".into())
        })?;
    batch
        .validate_awaiting_ticket(thread_id, ticket)
        .map_err(|error| Error::Execution(format!("incoherent tool resume: {error}")))?;
    Ok(Some(batch))
}

/// Synchronous resume admission: preserve role-specific refusal precedence,
/// bind the exact current input once and require durable tool publication before
/// checkpoint reads, environment realization or any execution-domain entry.
fn admit_resume(
    runtime: &Runtime,
    command: &ResumeCommand,
    reader: &dyn CommittedThreadView,
    context: RuntimeRunContext,
    continuation: ResumeContinuation,
    activation: Option<&RunActivation>,
) -> Result<(ResumeTicket, RuntimeRunContext, Option<ResumeCommit>)> {
    // Preserve the original model-free role's early missing-port refusal.
    // Activated and bare roles now share this same admission.
    if continuation == ResumeContinuation::ToolBatch {
        require_tool_resume_commit(&context)?;
    }
    let ticket = reader
        .resume_ticket(&command.run_id)
        .ok_or(awaken_runtime_contract::resume::ResumeError::NotWaiting)?;
    validate_resume(&ticket, command)?;
    validate_client_state_authority(&ticket, &command.result)?;
    let context = bind_resume_context(runtime, context, &ticket, activation)?;
    // Native tool tickets need the same atomic approval/result boundary.
    // Validate command and context first, keeping their existing precedence;
    // ordinary input pauses retain their original stateless behavior.
    if continuation == ResumeContinuation::NativeLoop && ticket.tool_call().is_some() {
        require_tool_resume_commit(&context)?;
    }
    // Delegation continuation retains its execution ticket across child turns
    // and is not a ToolResult consumption. Every other named resume operation
    // is carried to the first commit that replaces this exact ticket.
    let resume_commit = (ticket.reason() != AwaitReason::Delegation)
        .then(|| ResumeCommit::new(command.operation_id.clone(), &ticket));
    if continuation == ResumeContinuation::ToolBatch
        && !matches!(
            ticket.reason(),
            AwaitReason::ToolPermission | AwaitReason::ExternalEvent
        )
    {
        return Err(Error::Execution(
            "tool-batch handoff requires an awaiting permission or client result".to_string(),
        ));
    }

    Ok((ticket, context, resume_commit))
}

pub(crate) async fn resume_with_continuation(
    runtime: &Runtime,
    command: ResumeCommand,
    reader: &dyn CommittedThreadView,
    context: RuntimeRunContext,
    continuation: ResumeContinuation,
    activation: Option<&RunActivation>,
) -> Result<RunState> {
    crate::configuration::validate_configuration(runtime)?;
    let (ticket, context, resume_commit) =
        admit_resume(runtime, &command, reader, context, continuation, activation)?;

    let context_messages = fresh_resume_context(
        &reader.committed_messages(&command.thread_id),
        command.context_messages,
    )?;
    let committed_state = reader.committed_state(&command.thread_id);
    let (store, recovery) = model_operation::recover_checkpoint(
        committed_state,
        &context,
        &command.run_id,
        &command.thread_id,
    )
    .await;
    let resume_checkpoint = match recovery? {
        model_operation::RecoveryObservation::Ready(checkpoint) => checkpoint,
        model_operation::RecoveryObservation::End(cause) => {
            return finalize::finish_with_reader(
                runtime,
                &context,
                finalize::TerminalStateRead::Committed(reader),
                &command.thread_id,
                command.run_id.clone(),
                RunStepResult::ended_with_messages(command.run_id, cause, context_messages)
                    .with_resume(resume_commit),
            )
            .await;
        }
    };
    let context = model_operation::narrow_recovered_context(&store, context, false)?;
    let snapshot = match activation {
        Some(activation) => activation.snapshot.clone(),
        None => runtime
            .snapshot_by_id(&ExecutableAgentSnapshotId(ticket.snapshot_id.clone()))
            .ok_or_else(|| Error::Resolution("snapshot for resume not found".to_string()))?,
    };
    let resolved = super::run_loop::resolve_activation_snapshot(
        runtime,
        &snapshot,
        activation.and_then(|activation| activation.model_ref_override.as_deref()),
    )?;
    let run_input = activation
        .map(|activation| std::sync::Arc::<[Message]>::from(activation.input.clone()))
        .unwrap_or_default();

    let run_id = command.run_id.clone();
    let thread_id = command.thread_id.clone();

    let env = match runtime.resolve_plugin_env_with(&resolved.spec, &context.session_plugins) {
        Ok(env) => env,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id.0,
                thread_id = %thread_id.0,
                error = %error,
                "resumed run plugin environment violates its declared capability boundary"
            );
            let mut step =
                RunStepResult::capability_bound(run_id.clone()).with_resume(resume_commit);
            step.new_messages = context_messages;
            return finalize::finish_with_reader(
                runtime,
                &context,
                finalize::TerminalStateRead::Committed(reader),
                &thread_id,
                run_id,
                step,
            )
            .await;
        }
    };

    let waiting_batch = load_resumed_tool_batch(&thread_id, &ticket, &store)?;
    emit(&context, &run_id, AgentEvent::Fact(Fact::RunStarted)).await;

    // An awaiting delegation resumes through its executor, not the tool registry: the
    // executor runs one more step with the user's input and the run continues or
    // re-awaits.
    if ticket.reason() == AwaitReason::Delegation {
        return resume_delegation(
            runtime,
            &ticket,
            command.result,
            context_messages,
            run_input,
            &resolved,
            &env,
            &run_id,
            &thread_id,
            reader,
            &context,
            resume_checkpoint,
            waiting_batch.expect("Delegation is an admitted tool wait"),
            store,
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
        resume_commit,
        context_messages,
        run_input,
        reader,
        &context,
        continuation,
        resume_checkpoint,
        None,
        waiting_batch,
        store,
    )
    .await
}
