//! Backend-driven dispatch signal loop, claim-and-execute path, worker
//! housekeeping, and lease renewal for `Mailbox`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex as SyncMutex;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use awaken_contract::contract::mailbox::{DispatchSignalEntry, RunDispatch, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;
use awaken_contract::now_ms;

use crate::transport::channel_sink::ReconnectableEventSink;

use super::{
    DISPATCH_SIGNAL_ERROR_DELAY, DispatchAttempt, MAILBOX_DEPTH_STATUSES, Mailbox, MailboxError,
    MailboxWorker, MailboxWorkerStatus, REMOTE_CANCEL_POLL_MS, REMOTE_CANCEL_WAIT_MS,
    ThreadContext, dispatch_signal_batch_size, dispatch_signal_blocked_nack_delay,
    dispatch_signal_fetch_expires, dispatch_signal_max_concurrent_handlers, dispatch_status_label,
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
        match &result {
            Ok(()) => self.refresh_dispatch_depth_metrics().await,
            Err(error) => self.record_mailbox_submit_failed(dispatch, error).await,
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
    #[tracing::instrument(skip(self, reconnectable_sink, suspended), fields(dispatch_id = %dispatch.dispatch_id, thread_id = %thread_id))]
    pub(super) fn spawn_execution(
        self: &Arc<Self>,
        dispatch: RunDispatch,
        reconnectable_sink: Arc<ReconnectableEventSink>,
        claim_token: String,
        thread_id: String,
        suspended: Arc<AtomicBool>,
    ) {
        tokio::spawn(super::dispatch_execution::run_claimed_dispatch(
            Arc::clone(self),
            dispatch,
            reconnectable_sink,
            claim_token,
            thread_id,
            suspended,
        ));
    }

    pub(super) async fn finish_execution(self: &Arc<Self>, thread_id: &str, dispatch_id: &str) {
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
