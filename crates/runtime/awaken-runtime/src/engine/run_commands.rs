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
    if ticket.reason != AwaitReason::ScheduledAction {
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
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
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
            let step = RunStepResult::capability_bound(run_id.clone());
            return finish(runtime, &context, &thread_id, run_id, step).await;
        }
    };

    emit(&context, &run_id, AgentEvent::Fact(Fact::RunStarted)).await;

    // An awaiting delegation resumes through its executor, not the tool registry: the
    // executor runs one more step with the user's input and the run continues or
    // re-awaits.
    if ticket.reason == AwaitReason::Delegation {
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
