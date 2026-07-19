//! The single awaiting/terminal commit boundary for a Runtime step.

use super::*;

/// Seal unfinished child/tool state, commit the final disposition, and publish
/// best-effort terminal events. Durable facts always precede live notification.
pub(super) async fn finish(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    step: RunStepResult,
) -> Result<RunState> {
    let RunStepResult {
        new_messages,
        mut staged_state,
        audit,
        disposition,
    } = step;

    let disposition = match disposition {
        RunDisposition::Awaiting(mut ticket) => {
            ticket.thread_id = thread_id.clone();
            RunDisposition::Awaiting(ticket)
        }
        other => other,
    };
    debug_assert_eq!(disposition.run_id(), &run_id);
    let run_state = disposition.state();
    let mut cancellations = Vec::new();

    // Parent termination, unfinished batch sealing, unconsumed-result discard,
    // and child-cancellation intent become one ThreadCommit.
    if matches!(run_state, RunState::Ended(_)) {
        let mut store = store_from_commands(
            context
                .reader
                .as_ref()
                .map(|reader| reader.committed_state(thread_id))
                .unwrap_or_default(),
            &run_id,
        );
        for command in &staged_state {
            store.apply(command);
        }
        if let Some(mut batch) = ActiveToolBatch::load(&store)
            .map_err(|error| Error::Execution(error.to_string()))?
            .filter(|batch| batch.run_id == run_id && batch.phase != ToolBatchPhase::Finalized)
        {
            batch.seal_on_run_end("owning run ended before every tool call settled");
            let command = ActiveToolBatch::write(&Some(batch));
            store.apply(&command);
            staged_state.push(command);
        }
        if let Some(mut registry) =
            RunDelegations::load(&store).map_err(|error| Error::Execution(error.to_string()))?
        {
            cancellations = registry.end_parent();
            let command = RunDelegations::write(&Some(registry));
            store.apply(&command);
            staged_state.push(command);
        }
        let mut results = PendingChildRunResults::load(&store)
            .map_err(|error| Error::Execution(error.to_string()))?;
        if results.discard_on_parent_end() > 0 {
            let command = PendingChildRunResults::remove();
            store.apply(&command);
            staged_state.push(command);
        }
    }

    if let Some(coordinator) = &context.commit {
        coordinator
            .commit(ThreadCommit::assemble(
                thread_id.clone(),
                disposition,
                true,
                new_messages,
                staged_state,
                audit,
            ))
            .await
            .map_err(|error| Error::Commit(error.to_string()))?;
    }

    // The terminal commit is the outbox boundary: delivery happens only after
    // the intent is durable. Failures leave `CancelRequested` in committed
    // truth and are retried by the runtime reconciliation path.
    for cancellation in cancellations {
        let _ = runtime.deliver_child_cancellation(cancellation).await;
    }

    if let RunState::Ended(EndCause::Error(failure)) = &run_state {
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
    Ok(run_state)
}
