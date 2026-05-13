//! Backend-driven dispatch signal loop, claim-and-execute path, worker
//! housekeeping, and lease renewal for `Mailbox`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex as SyncMutex;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::mailbox::{
    DispatchSignalEntry, RunDispatch, RunDispatchResult, RunDispatchStatus,
};
use awaken_contract::contract::storage::StorageError;
use awaken_contract::now_ms;
use awaken_runtime::ThreadContextSnapshot;

use crate::transport::channel_sink::ReconnectableEventSink;

use super::{
    ActiveRunGuard, DISPATCH_SIGNAL_ERROR_DELAY, DispatchAttempt, MAILBOX_DEPTH_STATUSES, Mailbox,
    MailboxError, MailboxRunOutcome, MailboxWorker, MailboxWorkerStatus, REMOTE_CANCEL_POLL_MS,
    REMOTE_CANCEL_WAIT_MS, SuspensionAwareSink, TaskDoneMailboxNotify, ThreadContext,
    classify_error, dispatch_signal_batch_size, dispatch_signal_blocked_nack_delay,
    dispatch_signal_fetch_expires, dispatch_signal_max_concurrent_handlers, dispatch_status_label,
    mailbox_run_identity, mailbox_run_result, normalize_mailbox_run_mode,
    record_mailbox_dispatch_completion_metrics, record_mailbox_dispatch_start_metrics,
    record_mailbox_operation_result, result_label, revert_claiming_to_idle,
};

impl Mailbox {
    pub(super) async fn refresh_dispatch_depth_metrics(&self) {
        for status in MAILBOX_DEPTH_STATUSES {
            match self.store.count_dispatches_by_status(status).await {
                Ok(count) => {
                    let depth = count as f64;
                    crate::metrics::set_mailbox_dispatch_depth(
                        dispatch_status_label(status),
                        depth,
                    );
                    if status == RunDispatchStatus::Queued {
                        crate::metrics::set_mailbox_queue_depth(depth);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        status = dispatch_status_label(status),
                        error = %error,
                        "mailbox dispatch depth metric unavailable"
                    );
                    return;
                }
            }
        }
    }

    pub(super) async fn enqueue_dispatch_with_metrics(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<(), StorageError> {
        let start = Instant::now();
        let result = self.store.enqueue(dispatch).await;
        record_mailbox_operation_result("enqueue", result_label(&result), start);
        if result.is_ok() {
            self.refresh_dispatch_depth_metrics().await;
        }
        result
    }

    pub(super) async fn wait_for_dispatch_not_claimed(
        &self,
        dispatch_id: &str,
    ) -> Result<bool, MailboxError> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(REMOTE_CANCEL_WAIT_MS);
        loop {
            match self.store.load_dispatch(dispatch_id).await? {
                Some(dispatch) if dispatch.status == RunDispatchStatus::Claimed => {}
                _ => return Ok(true),
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(REMOTE_CANCEL_POLL_MS)).await;
        }
    }

    // ── Dispatch signal loop ─────────────────────────────────────────

    /// Drain backend work-queue delivery signals and wake local workers.
    pub async fn run_dispatch_signal_loop(self: Arc<Self>) {
        loop {
            let pull_start = Instant::now();
            let pull_result = self
                .store
                .pull_dispatch_signals(
                    dispatch_signal_batch_size(),
                    dispatch_signal_fetch_expires(),
                )
                .await;
            record_mailbox_operation_result("signal_pull", result_label(&pull_result), pull_start);
            match pull_result {
                Ok(entries) => {
                    crate::metrics::inc_mailbox_dispatch_signal_pulled_by(entries.len() as u64);
                    self.handle_dispatch_signal_entries(entries).await;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "dispatch signal pull failed");
                    tokio::time::sleep(DISPATCH_SIGNAL_ERROR_DELAY).await;
                }
            }
        }
    }

    async fn handle_dispatch_signal_entries(self: &Arc<Self>, entries: Vec<DispatchSignalEntry>) {
        if entries.is_empty() {
            return;
        }
        let max_concurrent = dispatch_signal_max_concurrent_handlers()
            .min(entries.len())
            .max(1);
        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        let mut tasks = JoinSet::new();
        for entry in entries {
            let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
                tracing::warn!("dispatch signal concurrency limiter closed");
                break;
            };
            let mailbox = Arc::clone(self);
            tasks.spawn(async move {
                let _permit = permit;
                mailbox.handle_dispatch_signal_entry(entry).await;
            });
        }
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::warn!(error = %error, "dispatch signal handler task failed");
            }
        }
    }

    async fn handle_dispatch_signal_entry(self: Arc<Self>, entry: DispatchSignalEntry) {
        let redelivery_attempts = entry.receipt.redelivery_attempts();
        if redelivery_attempts.is_some_and(|attempts| attempts > 1) {
            crate::metrics::inc_mailbox_dispatch_signal_redelivery();
        }
        self.get_or_create_worker(&entry.thread_id).await;
        let attempt = self.try_dispatch_next(&entry.thread_id).await;
        let nack_delay = match attempt {
            DispatchAttempt::TransientError => Some(None),
            DispatchAttempt::NoEligible => {
                match self.dispatch_signal_still_available(&entry).await {
                    Ok(true) => Some(Some(dispatch_signal_blocked_nack_delay(
                        redelivery_attempts,
                    ))),
                    Ok(false) => None,
                    Err(error) => {
                        tracing::warn!(
                            thread_id = %entry.thread_id,
                            dispatch_id = %entry.dispatch_id,
                            error = %error,
                            "failed to verify unclaimed dispatch signal"
                        );
                        Some(None)
                    }
                }
            }
            DispatchAttempt::Claimed | DispatchAttempt::Busy => None,
        };
        if let Some(delay) = nack_delay {
            let nack_start = Instant::now();
            let result = if let Some(delay) = delay {
                entry.receipt.nack_with_delay(delay).await
            } else {
                entry.receipt.nack().await
            };
            record_mailbox_operation_result("signal_nack", result_label(&result), nack_start);
            if result.is_ok() {
                crate::metrics::inc_mailbox_dispatch_signal_nack(delay.is_some());
            }
            if let Err(error) = result {
                tracing::warn!(
                    thread_id = %entry.thread_id,
                    dispatch_id = %entry.dispatch_id,
                    error = %error,
                    "failed to nack dispatch signal after claim error"
                );
            }
            return;
        }
        let ack_start = Instant::now();
        let ack_result = entry.receipt.ack().await;
        record_mailbox_operation_result("signal_ack", result_label(&ack_result), ack_start);
        if ack_result.is_ok() {
            crate::metrics::inc_mailbox_dispatch_signal_ack();
        }
        if let Err(error) = ack_result {
            tracing::warn!(
                thread_id = %entry.thread_id,
                dispatch_id = %entry.dispatch_id,
                error = %error,
                "failed to ack dispatch signal"
            );
        }
    }

    async fn dispatch_signal_still_available(
        &self,
        entry: &awaken_contract::contract::mailbox::DispatchSignalEntry,
    ) -> Result<bool, StorageError> {
        let now = now_ms();
        let Some(dispatch) = self.store.load_dispatch(&entry.dispatch_id).await? else {
            return Ok(false);
        };
        Ok(dispatch.status == RunDispatchStatus::Queued && dispatch.available_at <= now)
    }

    // ── Internal: dispatch ───────────────────────────────────────────

    /// Claim a dispatch from the store and start execution.
    #[tracing::instrument(skip(self), fields(thread_id = %thread_id))]
    async fn dispatch_next_claim(self: &Arc<Self>, thread_id: &str) -> DispatchAttempt {
        let now = now_ms();
        let claim_start = Instant::now();
        let claim_result = self
            .store
            .claim(thread_id, &self.consumer_id, self.config.lease_ms, now, 1)
            .await;
        let claim_result_label = match &claim_result {
            Ok(claimed) if claimed.is_empty() => "empty",
            Ok(_) => "ok",
            Err(_) => "error",
        };
        record_mailbox_operation_result("claim", claim_result_label, claim_start);
        let claimed = match claim_result {
            Ok(c) => {
                self.refresh_dispatch_depth_metrics().await;
                c
            }
            Err(e) => {
                tracing::warn!(error = %e, thread_id, "failed to claim dispatch");
                revert_claiming_to_idle(&self.workers, thread_id).await;
                return DispatchAttempt::TransientError;
            }
        };

        let Some(dispatch) = claimed.into_iter().next() else {
            // No dispatches to claim.
            revert_claiming_to_idle(&self.workers, thread_id).await;
            return DispatchAttempt::NoEligible;
        };

        let dispatch_id = dispatch.dispatch_id.clone();
        let claim_token = dispatch.claim_token.clone().unwrap_or_default();

        // Shared flag: set by the event sink when a tool call is suspended.
        let suspended = Arc::new(AtomicBool::new(false));

        // Start lease renewal.
        let lease_handle = self.spawn_lease_renewal(
            dispatch_id.clone(),
            claim_token.clone(),
            thread_id.to_string(),
            Arc::clone(&suspended),
        );

        // Pre-warm thread context cache.
        let thread_ctx = match ThreadContext::load(self.run_store.as_ref(), thread_id).await {
            Ok(ctx) => Some(ctx),
            Err(e) => {
                tracing::warn!(thread_id, error = %e, "failed to pre-warm thread context");
                None
            }
        };

        // Create channel for background dispatch (events go nowhere unless observed).
        let (event_tx, _event_rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);
        let reconnectable_sink = Arc::new(ReconnectableEventSink::new(event_tx.clone()));

        // Update worker state.
        let worker = self.get_or_create_worker(thread_id).await;
        {
            let mut w = worker.lock();
            w.thread_ctx = thread_ctx;
            w.status = MailboxWorkerStatus::Running {
                dispatch_id: dispatch_id.clone(),
                run_id: dispatch.run_id.clone(),
                lease_handle,
                sink: Arc::clone(&reconnectable_sink),
            };
        }

        self.spawn_execution(
            dispatch,
            event_tx,
            reconnectable_sink,
            claim_token,
            thread_id.to_string(),
            suspended,
        );
        DispatchAttempt::Claimed
    }

    /// Claim from store and execute the next dispatch for this thread.
    #[tracing::instrument(skip(self), fields(thread_id = %thread_id))]
    pub(super) async fn try_dispatch_next(self: &Arc<Self>, thread_id: &str) -> DispatchAttempt {
        let worker = {
            let workers = self.workers.read().await;
            match workers.get(thread_id) {
                Some(w) => Arc::clone(w),
                None => return DispatchAttempt::NoEligible,
            }
        };

        // Atomically transition Idle → Claiming to prevent TOCTOU race.
        {
            let mut w = worker.lock();
            if !matches!(w.status, MailboxWorkerStatus::Idle) {
                return DispatchAttempt::Busy;
            }
            w.status = MailboxWorkerStatus::Claiming;
        }

        self.dispatch_next_claim(thread_id).await
    }

    /// Spawn a lease renewal task that periodically extends the lease.
    ///
    /// When the `suspended` flag is set (run is waiting for human input),
    /// the renewal uses `suspended_lease_ms` instead of the default `lease_ms`
    /// to prevent premature lease expiration during HITL scenarios.
    pub(super) fn spawn_lease_renewal(
        &self,
        dispatch_id: String,
        claim_token: String,
        thread_id: String,
        suspended: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let store = Arc::clone(&self.store);
        let runtime = Arc::clone(&self.executor);
        let lease_ms = self.config.lease_ms;
        let suspended_lease_ms = self.config.suspended_lease_ms;
        let interval = self.config.lease_renewal_interval;

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // skip initial

            loop {
                tick.tick().await;
                let now = now_ms();
                let effective_lease_ms = if suspended.load(Ordering::Acquire) {
                    suspended_lease_ms
                } else {
                    lease_ms
                };
                let renew_start = Instant::now();
                match store
                    .extend_lease(&dispatch_id, &claim_token, effective_lease_ms, now)
                    .await
                {
                    Ok(true) => {
                        record_mailbox_operation_result("lease_renewal", "ok", renew_start);
                    }
                    Ok(false) => {
                        record_mailbox_operation_result("lease_renewal", "lost", renew_start);
                        // Lease lost -- another process reclaimed.
                        tracing::warn!(dispatch_id, thread_id, "lease lost, cancelling run");
                        runtime.cancel(&thread_id);
                        break;
                    }
                    Err(e) => {
                        record_mailbox_operation_result("lease_renewal", "error", renew_start);
                        tracing::warn!(dispatch_id, error = %e, "lease extension failed");
                        break;
                    }
                }
            }
        })
    }

    /// Spawn the actual execution task for a claimed dispatch.
    #[tracing::instrument(skip(self, event_tx, reconnectable_sink, suspended), fields(dispatch_id = %dispatch.dispatch_id, thread_id = %thread_id))]
    pub(super) fn spawn_execution(
        self: &Arc<Self>,
        dispatch: RunDispatch,
        event_tx: mpsc::Sender<AgentEvent>,
        reconnectable_sink: Arc<ReconnectableEventSink>,
        claim_token: String,
        thread_id: String,
        suspended: Arc<AtomicBool>,
    ) {
        let this = Arc::clone(self);
        let dispatch_id = dispatch.dispatch_id.clone();

        tokio::spawn(async move {
            crate::metrics::inc_active_runs();
            let _guard = ActiveRunGuard;

            let sink = SuspensionAwareSink {
                inner: reconnectable_sink as Arc<dyn EventSink>,
                suspended,
            };

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
                        this.mark_superseded_dispatch_run_cancelled(
                            &current_dispatch,
                            supersede_reason,
                        )
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
                        status: awaken_contract::contract::lifecycle::RunStatus::Done,
                        termination: Some(
                            awaken_contract::contract::lifecycle::TerminationReason::Error(
                                msg.clone(),
                            ),
                        ),
                        response: None,
                        error: Some(msg.clone()),
                    };
                    let record_start = Instant::now();
                    let record_result = this
                        .store
                        .record_dispatch_start(
                            &dispatch_id,
                            &claim_token,
                            &dispatch_instance_id,
                            start_now,
                        )
                        .await;
                    record_mailbox_operation_result(
                        "record_dispatch_start",
                        result_label(&record_result),
                        record_start,
                    );
                    if let Err(error) = record_result {
                        tracing::warn!(dispatch_id, error = %error, "failed to record dispatch start for reconstruction failure");
                        if let Ok(Some(latest_dispatch)) =
                            this.store.load_dispatch(&dispatch_id).await
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
                        if let Ok(Some(dead_letter_dispatch)) =
                            this.store.load_dispatch(&dispatch_id).await
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
            normalize_mailbox_run_mode(&mut request, false);
            let run_id = dispatch.run_id.clone();
            request = request
                .with_dispatch_id(dispatch_id.clone())
                .with_session_id(dispatch_instance_id.clone());
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
            let continue_run_id = request.continue_run_id.clone();
            let (inbox_sender, inbox_receiver) = awaken_runtime::inbox::inbox_channel_with_fallback(
                Arc::new(TaskDoneMailboxNotify::new(
                    this.clone(),
                    dispatch.thread_id.clone(),
                    continue_run_id,
                )),
            );
            request = request.with_inbox(inbox_sender, inbox_receiver);

            let result = this
                .executor
                .run_with_thread_context(request, Arc::new(sink), thread_ctx)
                .await;
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
            record_mailbox_dispatch_completion_metrics(
                &dispatch,
                start_now,
                now,
                outcome.metric_label(),
            );

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
                    // Emit error event so the SSE stream terminates with a
                    // proper RUN_ERROR instead of silently closing.
                    let _ = event_tx
                        .send(AgentEvent::RunFinish {
                            thread_id: dispatch.thread_id.clone(),
                            run_id: run_id.clone(),
                            identity: Some(mailbox_run_identity(
                                &dispatch,
                                &run_id,
                                &dispatch_instance_id,
                            )),
                            result: None,
                            termination:
                                awaken_contract::contract::lifecycle::TerminationReason::Error(
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
                    }
                }
                MailboxRunOutcome::PermanentError(msg) => {
                    tracing::warn!(dispatch_id, error = %msg, "run failed (permanent), dead-lettering");
                    // Emit error event so the SSE stream terminates with a
                    // proper RUN_ERROR. The runtime did not reach the loop,
                    // so no RunFinish was emitted — we must do it here.
                    let _ = event_tx
                        .send(AgentEvent::RunFinish {
                            thread_id: dispatch.thread_id.clone(),
                            run_id: run_id.clone(),
                            identity: Some(mailbox_run_identity(
                                &dispatch,
                                &run_id,
                                &dispatch_instance_id,
                            )),
                            result: None,
                            termination:
                                awaken_contract::contract::lifecycle::TerminationReason::Error(
                                    msg.clone(),
                                ),
                        })
                        .await;
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
                        if let Ok(Some(dead_letter_dispatch)) =
                            this.store.load_dispatch(&dispatch_id).await
                            && dead_letter_dispatch.status == RunDispatchStatus::DeadLetter
                        {
                            this.mark_dead_letter_dispatch_run_error(&dead_letter_dispatch)
                                .await;
                        }
                    }
                }
            }

            this.finish_execution(&thread_id, &dispatch_id).await;
        });
    }

    async fn finish_execution(self: &Arc<Self>, thread_id: &str, dispatch_id: &str) {
        // Abort lease renewal and return the worker to Idle.
        let worker = self.get_or_create_worker(thread_id).await;
        {
            let mut w = worker.lock();
            let should_transition = matches!(
                &w.status,
                MailboxWorkerStatus::Running { dispatch_id: cid, .. } if cid == dispatch_id
            );
            if should_transition {
                // Take ownership of the old status to abort the lease handle.
                let old = std::mem::replace(&mut w.status, MailboxWorkerStatus::Idle);
                w.thread_ctx = None;
                if let MailboxWorkerStatus::Running { lease_handle, .. } = old {
                    lease_handle.abort();
                }
            }
        }

        // Try to execute the next queued dispatch for this thread.
        self.try_dispatch_next(thread_id).await;
    }

    /// Get or create a per-thread worker.
    pub(super) async fn get_or_create_worker(
        &self,
        thread_id: &str,
    ) -> Arc<SyncMutex<MailboxWorker>> {
        // Fast path: read lock.
        {
            let workers = self.workers.read().await;
            if let Some(w) = workers.get(thread_id) {
                return Arc::clone(w);
            }
        }
        // Slow path: write lock.
        let mut workers = self.workers.write().await;
        Arc::clone(
            workers
                .entry(thread_id.to_string())
                .or_insert_with(|| Arc::new(SyncMutex::new(MailboxWorker::default()))),
        )
    }
}
