//! Framework-managed lifecycle for `Mailbox`: startup recovery plus
//! sweep / GC maintenance loops.

use std::sync::Arc;
use std::time::{Duration, Instant};

use awaken_runtime::RunActivation;
use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{RunQuery, RunRecord, StorageError};
use awaken_server_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_server_contract::now_ms;

use super::{
    Mailbox, MailboxError, MailboxLifecycleConfig, MailboxLifecycleHandle, MailboxLifecycleTasks,
    MailboxMaintenanceCallback, MailboxStartupRecoveryConfig, MailboxWorkerStatus,
    record_mailbox_operation_result, result_label,
};

impl Mailbox {
    // ── Lifecycle ────────────────────────────────────────────────────

    /// Page size for the recovery scan of pending-non-empty threads, so a large
    /// backend is not materialized into memory at once (ADR-0042 D7).
    pub(super) const PENDING_RECOVERY_PAGE_SIZE: usize = 256;

    /// Start framework-managed startup recovery plus sweep/GC maintenance.
    ///
    /// This method is idempotent: repeated calls return a handle to the
    /// already-running lifecycle instead of spawning duplicate recovery or
    /// maintenance loops. Dropping the returned handle does not stop the
    /// lifecycle; call `MailboxLifecycleHandle::shutdown().await` for
    /// quiescent shutdown or `MailboxLifecycleHandle::abort()` for
    /// fire-and-forget stop.
    ///
    /// If an async lifecycle transition is already in progress, this method
    /// returns an error instead of racing that transition. Use
    /// [`start_lifecycle_ready`](Self::start_lifecycle_ready) when the caller
    /// needs to wait for startup readiness.
    pub fn start_lifecycle(
        self: &Arc<Self>,
        config: MailboxLifecycleConfig,
    ) -> Result<MailboxLifecycleHandle, MailboxError> {
        let handle = MailboxLifecycleHandle {
            tasks: Arc::clone(&self.lifecycle_tasks),
            transition_lock: Arc::clone(&self.lifecycle_start_lock),
        };
        for _ in 0..16 {
            match self.lifecycle_start_lock.try_lock() {
                Ok(_transition_guard) => return self.start_lifecycle_internal(config, true),
                Err(_) if self.lifecycle_is_running()? => return Ok(handle),
                Err(_) => std::thread::yield_now(),
            }
        }
        Err(MailboxError::Internal(
            "mailbox lifecycle transition is already running".to_string(),
        ))
    }

    /// Run startup recovery to readiness, then start framework-managed
    /// maintenance.
    ///
    /// Unlike [`start_lifecycle`](Self::start_lifecycle), this method waits for
    /// startup recovery and returns an error when recovery exhausts its retry
    /// policy. Repeated calls remain idempotent: if lifecycle tasks are already
    /// running, the existing handle is returned.
    pub async fn start_lifecycle_ready(
        self: &Arc<Self>,
        mut config: MailboxLifecycleConfig,
    ) -> Result<MailboxLifecycleHandle, MailboxError> {
        let _start_guard = self.lifecycle_start_lock.lock().await;
        let handle = MailboxLifecycleHandle {
            tasks: Arc::clone(&self.lifecycle_tasks),
            transition_lock: Arc::clone(&self.lifecycle_start_lock),
        };
        if self.lifecycle_is_running()? {
            return Ok(handle);
        }

        if !config.startup_delay.is_zero() {
            tokio::time::sleep(config.startup_delay).await;
            config.startup_delay = Duration::ZERO;
        }

        self.run_startup_recovery_with_retry(config.startup_recovery.clone())
            .await?;
        self.start_lifecycle_internal(config, false)
    }

    pub(super) fn lifecycle_is_running(&self) -> Result<bool, MailboxError> {
        Ok(self
            .lifecycle_tasks
            .lock()
            .map_err(|_| MailboxError::Internal("mailbox lifecycle lock poisoned".to_string()))?
            .is_some())
    }

    fn start_lifecycle_internal(
        self: &Arc<Self>,
        config: MailboxLifecycleConfig,
        run_startup_recovery: bool,
    ) -> Result<MailboxLifecycleHandle, MailboxError> {
        let handle = MailboxLifecycleHandle {
            tasks: Arc::clone(&self.lifecycle_tasks),
            transition_lock: Arc::clone(&self.lifecycle_start_lock),
        };
        let mut lifecycle = self
            .lifecycle_tasks
            .lock()
            .map_err(|_| MailboxError::Internal("mailbox lifecycle lock poisoned".to_string()))?;

        if lifecycle.is_some() {
            return Ok(handle);
        }

        let startup_delay = config.startup_delay;
        let startup_recovery = config.startup_recovery.clone();
        let recover_mailbox = Arc::clone(self);
        let recover_task = run_startup_recovery.then(|| {
            tokio::spawn(async move {
                if !startup_delay.is_zero() {
                    tokio::time::sleep(startup_delay).await;
                }
                match recover_mailbox
                    .run_startup_recovery_with_retry(startup_recovery)
                    .await
                {
                    Ok(recovered) => {
                        tracing::info!(recovered, "mailbox startup recovery completed");
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "mailbox startup recovery failed");
                    }
                }
            })
        });

        let maintenance_mailbox = Arc::clone(self);
        let maintenance_callback = config.maintenance_callback;
        let maintenance_task = tokio::spawn(async move {
            if !startup_delay.is_zero() {
                tokio::time::sleep(startup_delay).await;
            }
            maintenance_mailbox
                .run_maintenance_loop(maintenance_callback)
                .await;
        });

        let dispatch_signal_task = self.store.supports_dispatch_signals().then(|| {
            let signal_mailbox = Arc::clone(self);
            tokio::spawn(async move {
                if !startup_delay.is_zero() {
                    tokio::time::sleep(startup_delay).await;
                }
                signal_mailbox.run_dispatch_signal_loop().await;
            })
        });

        *lifecycle = Some(MailboxLifecycleTasks {
            recover_task,
            dispatch_signal_task,
            maintenance_task,
        });
        Ok(handle)
    }

    async fn run_startup_recovery_with_retry(
        self: &Arc<Self>,
        config: MailboxStartupRecoveryConfig,
    ) -> Result<usize, MailboxError> {
        let max_attempts = config.max_attempts.max(1);
        for attempt in 1..=max_attempts {
            match self.recover().await {
                Ok(recovered) => return Ok(recovered),
                Err(error) if attempt < max_attempts => {
                    tracing::warn!(
                        attempt,
                        max_attempts,
                        retry_delay_ms = config.retry_delay.as_millis(),
                        error = %error,
                        "mailbox startup recovery failed; retrying"
                    );
                    if !config.retry_delay.is_zero() {
                        tokio::time::sleep(config.retry_delay).await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("max_attempts is normalized to at least one")
    }

    /// Recover on startup: reload queued dispatches and dispatch idle threads.
    #[tracing::instrument(skip(self))]
    pub async fn recover(self: &Arc<Self>) -> Result<usize, MailboxError> {
        let now = now_ms();
        let mut total = 0;

        // Reclaim expired leases from previous process crash.
        let reclaim_start = Instant::now();
        let reclaimed_result = self.store.reclaim_expired_leases(now, 100).await;
        record_mailbox_operation_result("reclaim", result_label(&reclaimed_result), reclaim_start);
        let reclaimed = reclaimed_result?;
        crate::metrics::inc_mailbox_operation_by("reclaim_dispatch", "ok", reclaimed.len() as u64);
        if !reclaimed.is_empty() {
            self.refresh_dispatch_depth_metrics().await;
        }
        for dispatch in &reclaimed {
            self.record_run_rescheduled_dispatch(dispatch, "expired_lease_reclaimed")
                .await;
            self.reconcile_terminal_dispatch(dispatch).await;
        }
        self.reconcile_terminal_dispatches().await;
        total += reclaimed.len();

        let repaired_checkpoint_events = self.repair_thread_message_checkpoint_events().await?;
        if repaired_checkpoint_events > 0 {
            tracing::info!(
                repaired_checkpoint_events,
                "repaired thread message checkpoint events"
            );
        }

        // Reload all queued mailbox IDs and try to dispatch.
        let thread_ids = self.store.queued_thread_ids().await?;
        for thread_id in &thread_ids {
            // Ensure worker exists for each thread with queued dispatches.
            self.get_or_create_worker(thread_id).await;
            self.try_dispatch_next(thread_id).await;
        }
        total += self
            .recover_prepared_runs_missing_dispatch_wal(&thread_ids)
            .await?;

        total += self
            .recover_orphaned_background_task_waits(&thread_ids)
            .await?;

        total += self.recover_orphaned_pending_threads(&thread_ids).await?;

        Ok(total)
    }

    /// Detect threads that hold pending messages but have no queued dispatch and
    /// no running run — a pending entry whose consume opportunity was lost
    /// (ADR-0042 D7). Threads already covered by a queued dispatch, a prepared
    /// run (reconstructed above), or an active run/lease (reclaimed above) are
    /// skipped. A genuine orphan is surfaced (warn + metric) for operator/monitor
    /// action: auto-starting a fresh consume run is intentionally not done here,
    /// as it has no run-activation context (agent/resolution) to start from.
    pub(super) async fn recover_orphaned_pending_threads(
        self: &Arc<Self>,
        queued_thread_ids: &[String],
    ) -> Result<usize, MailboxError> {
        let Some(store) = self.pending_thread_run_store.as_ref() else {
            return Ok(0);
        };
        // Re-query the live queued set rather than trusting the snapshot taken at
        // the start of `recover`: earlier recovery steps (prepared-run dispatch
        // reconstruction, background-wait recovery, try_dispatch) may have
        // enqueued dispatches for threads not queued when that snapshot was
        // taken. Using only the stale snapshot would false-positive those
        // freshly-queued threads as orphans (ADR-0042 D7 recovery noise).
        let mut queued_set: std::collections::HashSet<String> =
            queued_thread_ids.iter().cloned().collect();
        queued_set.extend(self.store.queued_thread_ids().await?);
        // Page through pending-non-empty threads with a cursor instead of loading
        // the whole set at once: on a large backend the unpaged scan would
        // materialize every pending thread id in memory at recovery time.
        let mut orphaned = 0usize;
        let mut after: Option<String> = None;
        loop {
            let page = store
                .list_threads_with_pending_messages(
                    Self::PENDING_RECOVERY_PAGE_SIZE,
                    after.as_deref(),
                )
                .await?;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            after = page.last().cloned();
            for thread_id in page {
                if queued_set.contains(thread_id.as_str()) {
                    continue;
                }
                // A running run owns its lane; its dispatch/lease (reclaimed
                // earlier) re-freezes the pending, so this is not an orphan.
                if let Some(run) = self.run_store.latest_run(&thread_id).await?
                    && run.status == RunStatus::Running
                {
                    continue;
                }
                orphaned += 1;
                crate::metrics::inc_mailbox_operation_by("orphaned_pending_thread", "detected", 1);
                tracing::warn!(
                    thread_id = %thread_id,
                    "recover: thread has pending messages but no consume opportunity \
                     (no queued dispatch, no running run); surfaced for re-delivery"
                );
            }
            if page_len < Self::PENDING_RECOVERY_PAGE_SIZE {
                break;
            }
        }
        Ok(orphaned)
    }

    async fn recover_orphaned_background_task_waits(
        self: &Arc<Self>,
        queued_thread_ids: &[String],
    ) -> Result<usize, MailboxError> {
        let queued_set: std::collections::HashSet<String> =
            queued_thread_ids.iter().cloned().collect();
        let runs = self.background_task_waiting_runs().await?;
        let mut total = 0usize;

        for run in runs {
            if queued_set.contains(&run.thread_id) {
                continue;
            }
            let request = RunActivation::new(
                run.thread_id.clone(),
                vec![Message::internal_user("<background-tasks-updated />")],
            )
            .with_agent_id(run.agent_id.clone())
            .with_continue_run_id(run.run_id.clone())
            .with_origin(awaken_server_contract::contract::storage::RunRequestOrigin::Internal)
            .with_run_mode(RunMode::InternalWake)
            .with_adapter(AdapterKind::Internal);
            self.submit_background(request).await?;
            total += 1;
            tracing::info!(
                thread_id = %run.thread_id,
                run_id = %run.run_id,
                "recover: enqueued wake dispatch for orphaned background-task thread"
            );
        }

        Ok(total)
    }

    pub(super) async fn recover_prepared_runs_missing_dispatch_wal(
        self: &Arc<Self>,
        queued_thread_ids: &[String],
    ) -> Result<usize, MailboxError> {
        let runs = self.prepared_runs_missing_dispatch_wal().await?;
        let mut total = 0usize;
        let queued_set: std::collections::HashSet<&str> =
            queued_thread_ids.iter().map(String::as_str).collect();
        let mut dispatch_threads = Vec::new();

        for run in runs {
            let Some(dispatch_id) = run.dispatch_id.clone() else {
                continue;
            };
            if self.store.load_dispatch(&dispatch_id).await?.is_some() {
                continue;
            }
            let now = now_ms();
            let dispatch = RunDispatch {
                dispatch_id: dispatch_id.clone(),
                thread_id: run.thread_id.clone(),
                run_id: run.run_id.clone(),
                priority: 128,
                dedupe_key: None,
                dispatch_epoch: 0,
                status: RunDispatchStatus::Queued,
                available_at: now,
                attempt_count: 0,
                max_attempts: self.config.default_max_attempts,
                last_error: None,
                claim_token: None,
                claimed_by: None,
                lease_until: None,
                dispatch_instance_id: None,
                run_status: None,
                termination: None,
                run_response: None,
                run_error: None,
                completed_at: None,
                created_at: now,
                updated_at: now,
            };
            if let Err(error) = self.store.enqueue(&dispatch).await {
                match error {
                    StorageError::AlreadyExists(id) if id == dispatch_id => {
                        tracing::info!(
                            thread_id = %run.thread_id,
                            run_id = %run.run_id,
                            dispatch_id = %dispatch_id,
                            "recover: another instance reconstructed prepared dispatch WAL"
                        );
                        continue;
                    }
                    other => return Err(MailboxError::Store(other)),
                }
            }
            self.record_mailbox_dispatch_event("RunQueued", &dispatch)
                .await;
            total += 1;
            tracing::warn!(
                thread_id = %run.thread_id,
                run_id = %run.run_id,
                dispatch_id = %dispatch_id,
                status = ?run.status,
                "recover: reconstructed dispatch WAL for prepared run"
            );
            if !queued_set.contains(run.thread_id.as_str()) {
                dispatch_threads.push(run.thread_id);
            }
        }

        dispatch_threads.sort();
        dispatch_threads.dedup();
        for thread_id in dispatch_threads {
            self.get_or_create_worker(&thread_id).await;
            self.try_dispatch_next(&thread_id).await;
        }

        if total > 0 {
            self.refresh_dispatch_depth_metrics().await;
        }
        Ok(total)
    }

    async fn background_task_waiting_runs(&self) -> Result<Vec<RunRecord>, MailboxError> {
        let mut runs = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = self
                .run_store
                .list_runs(&RunQuery {
                    status: Some(RunStatus::Waiting),
                    limit: 200,
                    offset,
                    ..Default::default()
                })
                .await?;
            let page_len = page.items.len();
            runs.extend(
                page.items
                    .into_iter()
                    .filter(RunRecord::is_background_task_waiting),
            );
            if !page.has_more || page_len == 0 {
                break;
            }
            offset += page_len;
        }
        Ok(runs)
    }

    async fn prepared_runs_missing_dispatch_wal(&self) -> Result<Vec<RunRecord>, MailboxError> {
        let mut prepared = Vec::new();
        for status in [RunStatus::Created, RunStatus::Waiting] {
            let mut offset = 0usize;
            loop {
                let page = self
                    .run_store
                    .list_runs(&RunQuery {
                        status: Some(status),
                        limit: 200,
                        offset,
                        ..Default::default()
                    })
                    .await?;
                let page_len = page.items.len();
                prepared.extend(page.items.into_iter().filter(|run| {
                    run.dispatch_id.is_some()
                        && (run.status == RunStatus::Created || run.is_resumable_waiting())
                }));
                if !page.has_more || page_len == 0 {
                    break;
                }
                offset += page_len;
            }
        }
        Ok(prepared)
    }

    /// Run sweep + GC loop forever. Call from `tokio::spawn`.
    ///
    /// When `maintenance_callback` is provided, it runs on each GC tick so
    /// applications can clean up resources they own.
    pub async fn run_maintenance_loop(
        self: Arc<Self>,
        maintenance_callback: Option<MailboxMaintenanceCallback>,
    ) {
        let mut sweep_interval = tokio::time::interval(self.config.sweep_interval);
        let mut gc_interval = tokio::time::interval(self.config.gc_interval);

        // Skip the initial immediate tick.
        sweep_interval.tick().await;
        gc_interval.tick().await;

        loop {
            tokio::select! {
                _ = sweep_interval.tick() => {
                    self.run_sweep().await;
                }
                _ = gc_interval.tick() => {
                    self.run_gc().await;
                    if let Some(cleanup) = &maintenance_callback {
                        cleanup();
                    }
                }
            }
        }
    }

    // ── Maintenance ──────────────────────────────────────────────────

    pub(super) async fn run_sweep(self: &Arc<Self>) {
        let now = now_ms();
        let reclaim_start = Instant::now();
        let reclaim_result = self.store.reclaim_expired_leases(now, 100).await;
        record_mailbox_operation_result("reclaim", result_label(&reclaim_result), reclaim_start);
        match reclaim_result {
            Ok(reclaimed) => {
                crate::metrics::inc_mailbox_operation_by(
                    "reclaim_dispatch",
                    "ok",
                    reclaimed.len() as u64,
                );
                if !reclaimed.is_empty() {
                    tracing::info!(count = reclaimed.len(), "sweep reclaimed expired leases");
                    self.refresh_dispatch_depth_metrics().await;
                    for dispatch in reclaimed {
                        self.record_run_rescheduled_dispatch(&dispatch, "expired_lease_reclaimed")
                            .await;
                        self.reconcile_terminal_dispatch(&dispatch).await;
                        if dispatch.status == RunDispatchStatus::Queued {
                            let thread_id = dispatch.thread_id.clone();
                            self.get_or_create_worker(&thread_id).await;
                            self.try_dispatch_next(&thread_id).await;
                        }
                    }
                }
                self.reconcile_terminal_dispatches().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "sweep failed");
            }
        }
        // Retry any checkpoint events that failed to publish after a freeze
        // commit, so projections recover without waiting for a process restart.
        self.drain_checkpoint_repair_queue().await;
    }

    pub(super) async fn run_gc(&self) {
        let now = now_ms();
        let gc_ttl_ms = self.config.gc_ttl.as_millis() as u64;
        let older_than = now.saturating_sub(gc_ttl_ms);
        let purge_start = Instant::now();
        let purge_result = self.store.purge_terminal(older_than).await;
        record_mailbox_operation_result("purge_terminal", result_label(&purge_result), purge_start);
        match purge_result {
            Ok(purged) => {
                crate::metrics::inc_mailbox_operation_by("purged", "ok", purged as u64);
                if purged > 0 {
                    tracing::info!(purged, "GC purged terminal dispatches");
                    self.refresh_dispatch_depth_metrics().await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "GC failed");
            }
        }

        // Clean up idle workers with no queued dispatches.
        self.gc_idle_workers().await;
    }

    /// Remove workers in `Idle` state that have no queued dispatches in the store.
    ///
    /// This prevents the `workers` HashMap from growing unbounded as new
    /// threads are created and their runs complete.
    pub(super) async fn gc_idle_workers(&self) {
        let idle_keys: Vec<String> = {
            let workers = self.workers.read().await;
            let mut keys = Vec::new();
            for (thread_id, worker) in workers.iter() {
                let w = worker.lock();
                if matches!(w.status, MailboxWorkerStatus::Idle) {
                    keys.push(thread_id.clone());
                }
            }
            keys
        };

        if idle_keys.is_empty() {
            return;
        }

        // Check the store without holding the workers write lock. Remote stores
        // may block on network or disk I/O; keeping the lock during those awaits
        // would stall submissions, reconnects, and dispatch transitions.
        let mut removable = Vec::new();
        for thread_id in &idle_keys {
            let has_queued = self
                .store
                .list_dispatches(
                    thread_id,
                    Some(&[RunDispatchStatus::Queued, RunDispatchStatus::Claimed]),
                    1,
                    0,
                )
                .await
                .map(|dispatches| !dispatches.is_empty())
                .unwrap_or(true); // Err → keep worker to be safe

            if !has_queued {
                removable.push(thread_id.clone());
            }
        }

        if removable.is_empty() {
            return;
        }

        let mut removed = 0usize;
        let mut workers = self.workers.write().await;
        for thread_id in removable {
            // Re-check under write lock: status might have changed while the
            // store query was in flight.
            let still_idle = if let Some(worker) = workers.get(&thread_id) {
                let w = worker.lock();
                matches!(w.status, MailboxWorkerStatus::Idle)
            } else {
                false
            };
            if still_idle {
                workers.remove(&thread_id);
                removed += 1;
            }
        }

        if removed > 0 {
            tracing::debug!(removed, "GC removed idle workers");
        }
    }
}
