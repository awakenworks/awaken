//! Claim-to-completion execution for one `RunDispatch` owned by a worker.
//!
//! Extracted from `signal_loop.rs::spawn_execution` so the signal-loop file
//! stays under the file-length cap and future ADR-0036 D9 buffer wiring has
//! room to land here without bloating the dispatch loop.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use awaken_runtime::ThreadContextSnapshot;
use awaken_runtime::{EventBuffer, ResolutionPolicy, RuntimeError};
use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::event_sink::EventSink;
use awaken_server_contract::contract::mailbox::{
    RunDispatch, RunDispatchResult, RunDispatchStatus,
};
use awaken_server_contract::contract::run::RunResolutionScope;
use awaken_server_contract::now_ms;

use crate::transport::channel_sink::ReconnectableEventSink;

use super::{
    ActiveRunGuard, Mailbox, MailboxRunOutcome, SuspensionAwareSink, TaskDoneMailboxNotify,
    classify_error, mailbox_run_identity, mailbox_run_result, normalize_mailbox_run_mode,
    record_mailbox_dispatch_completion_metrics, record_mailbox_dispatch_start_metrics,
    record_mailbox_operation_result, result_label,
};

/// Execute one claimed `RunDispatch` to completion. Runs as a `tokio::spawn`
/// task; the worker that claimed the dispatch passes ownership through this
/// function and never observes it again after `spawn_execution` returns.
pub(super) async fn run_claimed_dispatch(
    this: Arc<Mailbox>,
    dispatch: RunDispatch,
    reconnectable_sink: Arc<ReconnectableEventSink>,
    claim_token: String,
    thread_id: String,
    suspended: Arc<AtomicBool>,
) {
    let dispatch_id = dispatch.dispatch_id.clone();
    crate::metrics::inc_active_runs();
    let _guard = ActiveRunGuard;
    // Dispatch epoch check: if this dispatch was superseded between claim and
    // execution start, terminalize it and abort without entering the runtime.
    let load_start = Instant::now();
    let current_dispatch_result = this.store.load_dispatch(&dispatch_id).await;
    record_mailbox_operation_result(
        "load_dispatch",
        result_label(&current_dispatch_result),
        load_start,
    );
    let current_dispatch = match current_dispatch_result {
        Ok(Some(current_dispatch)) => current_dispatch,
        Ok(None) => {
            tracing::info!(dispatch_id, "dispatch disappeared before execution");
            this.finish_execution(&thread_id, &dispatch_id).await;
            return;
        }
        Err(error) => {
            tracing::warn!(dispatch_id, error = %error, "failed to verify dispatch before execution");
            this.finish_execution(&thread_id, &dispatch_id).await;
            return;
        }
    };
    if current_dispatch.status != RunDispatchStatus::Claimed
        || current_dispatch.claim_token.as_deref() != Some(claim_token.as_str())
    {
        tracing::info!(dispatch_id, status = ?current_dispatch.status, "dispatch no longer owned by this worker, skipping execution");
        if current_dispatch.status == RunDispatchStatus::Superseded {
            this.mark_superseded_dispatch_run_cancelled(
                &current_dispatch,
                "dispatch superseded before execution start",
            )
            .await;
        }
        this.finish_execution(&thread_id, &dispatch_id).await;
        return;
    }
    let epoch_start = Instant::now();
    let current_epoch_result = this.store.current_dispatch_epoch(&thread_id).await;
    record_mailbox_operation_result(
        "current_dispatch_epoch",
        result_label(&current_epoch_result),
        epoch_start,
    );
    match current_epoch_result {
        Ok(current_epoch) if current_dispatch.dispatch_epoch < current_epoch => {
            tracing::info!(
                dispatch_id,
                thread_id,
                dispatch_epoch = current_dispatch.dispatch_epoch,
                current_epoch,
                "dispatch superseded before execution start"
            );
            let supersede_reason = "claimed dispatch superseded before execution start";
            let supersede_start = Instant::now();
            let supersede_result = this
                .store
                .supersede_claimed(&dispatch_id, &claim_token, now_ms(), supersede_reason)
                .await;
            record_mailbox_operation_result(
                "supersede_claimed",
                result_label(&supersede_result),
                supersede_start,
            );
            if supersede_result.is_ok() {
                this.refresh_dispatch_depth_metrics().await;
                this.mark_superseded_dispatch_run_cancelled(&current_dispatch, supersede_reason)
                    .await;
            }
            this.finish_execution(&thread_id, &dispatch_id).await;
            return;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(dispatch_id, thread_id, error = %error, "failed to read dispatch epoch before execution");
            this.finish_execution(&thread_id, &dispatch_id).await;
            return;
        }
    }

    let dispatch_instance_id = uuid::Uuid::now_v7().to_string();
    let start_now = now_ms();
    record_mailbox_dispatch_start_metrics(&dispatch, start_now);
    let mut request = match this.reconstruct_run_request(&dispatch).await {
        Ok(request) => request,
        Err(error) => {
            tracing::error!(dispatch_id, error = %error, "failed to reconstruct run request from durable run record");
            let now = now_ms();
            record_mailbox_dispatch_completion_metrics(
                &dispatch,
                start_now,
                now,
                "permanent_error",
            );
            let msg = error.to_string();
            let run_result = RunDispatchResult {
                run_id: dispatch.run_id.clone(),
                dispatch_instance_id: dispatch_instance_id.clone(),
                status: awaken_server_contract::contract::lifecycle::RunStatus::Done,
                termination: Some(
                    awaken_server_contract::contract::lifecycle::TerminationReason::Error(
                        msg.clone(),
                    ),
                ),
                response: None,
                error: Some(msg.clone()),
            };
            let record_start = Instant::now();
            let record_result = this
                .store
                .record_dispatch_start(&dispatch_id, &claim_token, &dispatch_instance_id, start_now)
                .await;
            record_mailbox_operation_result(
                "record_dispatch_start",
                result_label(&record_result),
                record_start,
            );
            if let Err(error) = record_result {
                tracing::warn!(dispatch_id, error = %error, "failed to record dispatch start for reconstruction failure");
                if let Ok(Some(latest_dispatch)) = this.store.load_dispatch(&dispatch_id).await
                    && latest_dispatch.status == RunDispatchStatus::Superseded
                {
                    this.mark_superseded_dispatch_run_cancelled(
                        &latest_dispatch,
                        "dispatch superseded before reconstruction failure was recorded",
                    )
                    .await;
                }
                this.finish_execution(&thread_id, &dispatch_id).await;
                return;
            }
            let record_result_start = Instant::now();
            let record_run_result = this
                .store
                .record_run_result(&dispatch_id, &claim_token, &run_result, now)
                .await;
            record_mailbox_operation_result(
                "record_run_result",
                result_label(&record_run_result),
                record_result_start,
            );
            let dead_letter_start = Instant::now();
            let dead_letter_result = this
                .store
                .dead_letter(&dispatch_id, &claim_token, &msg, now)
                .await;
            record_mailbox_operation_result(
                "dead_letter",
                result_label(&dead_letter_result),
                dead_letter_start,
            );
            if dead_letter_result.is_ok() {
                this.refresh_dispatch_depth_metrics().await;
                if let Ok(Some(dead_letter_dispatch)) = this.store.load_dispatch(&dispatch_id).await
                    && dead_letter_dispatch.status == RunDispatchStatus::DeadLetter
                {
                    this.mark_dead_letter_dispatch_run_error(&dead_letter_dispatch)
                        .await;
                }
            }
            this.finish_execution(&thread_id, &dispatch_id).await;
            return;
        }
    };
    let is_resume = request.resume_run_id().is_some();
    // When runtime event capture is enabled, mint a per-run EventBuffer and
    // share it between the sink wrap (stages drafts here) and the runtime's
    // CheckpointCommitPlan (drains).
    let event_buffer = this
        .runtime_event_capture
        .is_some()
        .then(|| Arc::new(EventBuffer::new()));
    let sink = Arc::new(SuspensionAwareSink {
        inner: this.wrap_dispatch_runtime_event_sink(
            reconnectable_sink,
            &dispatch,
            dispatch_id.clone(),
            is_resume,
            event_buffer.clone(),
        ),
        suspended,
    });
    normalize_mailbox_run_mode(&mut request, false);
    let run_id = dispatch.run_id.clone();
    request = request
        .with_dispatch_id(dispatch_id.clone())
        .with_session_id(dispatch_instance_id.clone());
    if let Some(buffer) = event_buffer {
        request = request.with_event_buffer(buffer);
    }
    let record_start = Instant::now();
    let record_start_result = this
        .store
        .record_dispatch_start(&dispatch_id, &claim_token, &dispatch_instance_id, start_now)
        .await;
    record_mailbox_operation_result(
        "record_dispatch_start",
        result_label(&record_start_result),
        record_start,
    );
    if let Err(e) = record_start_result {
        tracing::warn!(dispatch_id, run_id, error = %e, "failed to record mailbox dispatch start; skipping execution");
        if let Ok(Some(latest_dispatch)) = this.store.load_dispatch(&dispatch_id).await
            && latest_dispatch.status == RunDispatchStatus::Superseded
        {
            this.mark_superseded_dispatch_run_cancelled(
                &latest_dispatch,
                "dispatch superseded before runtime start was recorded",
            )
            .await;
        }
        this.finish_execution(&thread_id, &dispatch_id).await;
        return;
    }
    this.record_mailbox_dispatch_event("RunSubmitted", &dispatch)
        .await;
    let thread_ctx = {
        let workers = this.workers.read().await;
        workers.get(&thread_id).and_then(|worker| {
            let w = worker.lock();
            w.thread_ctx.as_ref().map(|ctx| {
                ThreadContextSnapshot::new(
                    ctx.messages.clone(),
                    ctx.latest_run.clone(),
                    ctx.run_cache.clone(),
                )
            })
        })
    };
    let continue_run_id = request.resume_run_id().map(str::to_owned);
    let (inbox_sender, inbox_receiver) =
        awaken_runtime::inbox::inbox_channel_with_fallback(Arc::new(TaskDoneMailboxNotify::new(
            this.clone(),
            dispatch.thread_id.clone(),
            continue_run_id,
        )));
    request = request.with_inbox(inbox_sender, inbox_receiver);
    let registry_manifest = match this.run_store.load_run(&run_id).await {
        Ok(Some(record)) => record.registry_manifest.ok_or_else(|| {
            format!(
                "persistent dispatch for run '{}' has no pinned registry manifest",
                record.run_id
            )
        }),
        Ok(None) => Err(format!(
            "persistent dispatch run '{}' disappeared before resolution",
            run_id
        )),
        Err(error) => Err(format!(
            "failed to load run '{}' before persistent dispatch resolution: {error}",
            run_id
        )),
    };
    let result = match registry_manifest {
        Ok(registry_manifest) => {
            match this
                .executor
                .resolve_activation_in_scope(
                    &request,
                    ResolutionPolicy::PersistentServer,
                    RunResolutionScope::Pinned(registry_manifest),
                )
                .await
                .and_then(|plan| plan.into_replayable())
            {
                Ok(plan) => {
                    if let Some(handler) =
                        this.pending_boundary_handler(&request, &run_id, &plan.scope.manifest)
                    {
                        request = request.with_pending_boundary_handler(handler);
                    }
                    this.executor
                        .run_replayable_with_thread_context(request, plan, sink.clone(), thread_ctx)
                        .await
                }
                Err(error) => Err(awaken_runtime::loop_runner::AgentLoopError::RuntimeError(
                    RuntimeError::ResolveFailed {
                        message: error.to_string(),
                    },
                )),
            }
        }
        Err(message) => Err(awaken_runtime::loop_runner::AgentLoopError::RuntimeError(
            RuntimeError::ResolveFailed { message },
        )),
    };
    let now = now_ms();
    let run_result = mailbox_run_result(&run_id, &dispatch_instance_id, &result);
    let record_result_start = Instant::now();
    let record_run_result = this
        .store
        .record_run_result(&dispatch_id, &claim_token, &run_result, now)
        .await;
    record_mailbox_operation_result(
        "record_run_result",
        result_label(&record_run_result),
        record_result_start,
    );
    if let Err(e) = record_run_result {
        tracing::warn!(dispatch_id, run_id, error = %e, "failed to record mailbox run result");
    }
    let outcome = classify_error(&result);
    record_mailbox_dispatch_completion_metrics(&dispatch, start_now, now, outcome.metric_label());
    match outcome {
        MailboxRunOutcome::Completed => {
            let ack_start = Instant::now();
            let ack_result = this.store.ack(&dispatch_id, &claim_token, now).await;
            record_mailbox_operation_result("ack", result_label(&ack_result), ack_start);
            if let Err(e) = ack_result {
                tracing::warn!(dispatch_id, error = %e, "ack failed");
            } else {
                this.refresh_dispatch_depth_metrics().await;
            }
        }
        MailboxRunOutcome::TransientError(msg) => {
            tracing::warn!(dispatch_id, error = %msg, "run failed (transient), nacking");
            // Emit error event so the SSE stream terminates with a proper
            // RUN_ERROR instead of silently closing.
            sink.emit(AgentEvent::RunFinish {
                thread_id: dispatch.thread_id.clone(),
                run_id: run_id.clone(),
                identity: Some(mailbox_run_identity(
                    &dispatch,
                    &run_id,
                    &dispatch_instance_id,
                )),
                result: None,
                termination: awaken_server_contract::contract::lifecycle::TerminationReason::Error(
                    msg.clone(),
                ),
            })
            .await;
            let backoff_factor = 2u64.pow(dispatch.attempt_count.saturating_sub(1).min(6));
            let retry_at = now
                + (this.config.default_retry_delay_ms * backoff_factor)
                    .min(this.config.max_retry_delay_ms);
            let nack_start = Instant::now();
            let nack_result = this
                .store
                .nack(&dispatch_id, &claim_token, retry_at, &msg, now)
                .await;
            record_mailbox_operation_result("nack", result_label(&nack_result), nack_start);
            if let Err(e) = nack_result {
                tracing::warn!(dispatch_id, error = %e, "nack failed");
            } else {
                this.refresh_dispatch_depth_metrics().await;
                this.record_run_rescheduled_dispatch_by_id(&dispatch_id, "retry_backoff")
                    .await;
            }
        }
        MailboxRunOutcome::PermanentError(msg) => {
            tracing::warn!(dispatch_id, error = %msg, "run failed (permanent), dead-lettering");
            // Emit error event so the SSE stream terminates with a proper
            // RUN_ERROR. The runtime did not reach the loop, so no
            // RunFinish was emitted — we must do it here.
            sink.emit(AgentEvent::RunFinish {
                thread_id: dispatch.thread_id.clone(),
                run_id: run_id.clone(),
                identity: Some(mailbox_run_identity(
                    &dispatch,
                    &run_id,
                    &dispatch_instance_id,
                )),
                result: None,
                termination: awaken_server_contract::contract::lifecycle::TerminationReason::Error(
                    msg.clone(),
                ),
            })
            .await;
            this.record_run_errored(&dispatch, &msg).await;
            let dead_letter_start = Instant::now();
            let dead_letter_result = this
                .store
                .dead_letter(&dispatch_id, &claim_token, &msg, now)
                .await;
            record_mailbox_operation_result(
                "dead_letter",
                result_label(&dead_letter_result),
                dead_letter_start,
            );
            if let Err(e) = dead_letter_result {
                tracing::warn!(dispatch_id, error = %e, "dead_letter failed");
            } else {
                this.refresh_dispatch_depth_metrics().await;
                if let Ok(Some(dead_letter_dispatch)) = this.store.load_dispatch(&dispatch_id).await
                    && dead_letter_dispatch.status == RunDispatchStatus::DeadLetter
                {
                    this.mark_dead_letter_dispatch_run_error(&dead_letter_dispatch)
                        .await;
                }
            }
        }
    }
    this.finish_execution(&thread_id, &dispatch_id).await;
}
