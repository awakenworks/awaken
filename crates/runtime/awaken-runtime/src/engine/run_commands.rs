//! Run command handling for native execution, resume, cancellation, stopping,
//! and committed scheduled actions.

use super::*;

#[async_trait]
impl RunExecutor for Runtime {
    #[tracing::instrument(name = "runtime.run", skip_all, fields(awaken.run.id = %activation.run_id.0))]
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        let context = activation.narrow_context(context);
        // Execution is the single ingress for fresh activations, including durable
        // dispatch. Retain the exact immutable snapshot before the run can await so
        // an in-process resume resolves the same value by id.
        self.register_snapshot(activation.snapshot.clone());
        // Physical-attempt ownership is established by the direct driver or the
        // claimed Worker before entering this executor. Keeping the registration
        // outside the native loop prevents nested generations and gives ACP/A2A
        // the same lifecycle boundary.
        run_agent_loop(self, activation, context).await
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
        let context = activation.narrow_context(context);
        let reader = context.reader.clone().ok_or_else(|| {
            Error::Execution("RunAttemptExecutor::resume requires committed history".to_string())
        })?;
        // Resume is the single ingress for an activated retry. A rebuilt Runtime
        // starts without its predecessor's in-memory snapshot registry, so retain
        // the exact immutable snapshot carried by this activation before resolving
        // the committed resume ticket.
        self.register_snapshot(activation.snapshot.clone());
        Runtime::resume(self, command, reader.as_ref(), context).await
    }
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
    finish(runtime, &context, &thread_id, run_id, step).await
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
    finish(runtime, &context, &thread_id, run_id, step).await
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
    match reader.run_state(&run_id) {
        Some(state @ RunState::Ended(_)) => return Ok(state),
        Some(RunState::Awaiting) => {}
        Some(RunState::Running) | None => {
            return Err(Error::Execution(
                "coordinated tool interruption requires an awaiting Run".to_string(),
            ));
        }
    }
    let store = store_from_commands(reader.committed_state(&thread_id), &run_id);
    let ticket_is_coherent = reader.resume_ticket(&run_id).is_none_or(|ticket| {
        ticket.run_id == run_id
            && ticket.thread_id == thread_id
            && matches!(
                ticket.reason(),
                AwaitReason::ToolPermission | AwaitReason::ExternalEvent
            )
    });
    let batch = ActiveToolBatch::load(&store);
    let batch_is_coherent = batch.as_ref().is_ok_and(|batch| {
        batch.as_ref().is_some_and(|batch| {
            batch.run_id() == &run_id
                && batch.phase() == ToolBatchPhase::Open
                && batch.calls().iter().any(|entry| {
                    matches!(
                        entry.phase,
                        ToolCallPhase::Awaiting { ref wait }
                            if matches!(
                                wait.kind,
                                ToolWaitKind::ToolPermission | ToolWaitKind::ExternalResult
                            )
                    )
                })
        })
    });
    if !ticket_is_coherent || !batch_is_coherent {
        let detail = match &batch {
            Err(error) => format!("unreadable ActiveToolBatch: {error}"),
            Ok(None) => "missing ActiveToolBatch".to_string(),
            Ok(Some(_)) if !ticket_is_coherent => "incoherent ResumeTicket".to_string(),
            Ok(Some(_)) => "ActiveToolBatch does not contain the current external wait".to_string(),
        };
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
            ToolCallPhase::Completed(_) | ToolCallPhase::Indeterminate { .. } => continue,
        }
        batch
            .set_result_messages(&call.call_id, vec![tool_result_message(&call, &output)])
            .map_err(|error| Error::Execution(error.to_string()))?;
    }
    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    let messages = batch
        .calls()
        .iter()
        .flat_map(|entry| entry.result_messages.iter().cloned())
        .collect();
    let step = RunStepResult {
        new_messages: messages,
        staged_state: vec![ActiveToolBatch::write(&Some(batch))],
        audit: Vec::new(),
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
    finish(runtime, &context, &thread_id, run_id, step).await
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
    let ticket = reader
        .resume_ticket(&command.run_id)
        .ok_or_else(|| Error::Execution("run is not awaiting".to_string()))?;
    validate_resume(&ticket, &command).map_err(|err| Error::Execution(err.to_string()))?;

    let context_messages = fresh_resume_context(
        &reader.committed_messages(&command.thread_id),
        command.context_messages,
    )?;
    let resume_operation_id = command.operation_id.clone();

    let snapshot = runtime
        .snapshot_by_id(&ExecutableAgentSnapshotId(ticket.snapshot_id.clone()))
        .ok_or_else(|| Error::Resolution("snapshot for resume not found".to_string()))?;
    let resolved = runtime.resolve(&snapshot).map_err(map_resolver_error)?;

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
            let mut step = RunStepResult::capability_bound(run_id.clone());
            step.new_messages = context_messages;
            return finish(runtime, &context, &thread_id, run_id, step).await;
        }
    };

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
        resume_operation_id,
        context_messages,
        reader,
        &context,
    )
    .await
}
