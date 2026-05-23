//! Framework-managed lifecycle for `Mailbox`: startup recovery plus
//! sweep / GC maintenance loops.

use std::sync::Arc;
use std::time::{Duration, Instant};

use awaken_contract::contract::mailbox::RunDispatchStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_contract::now_ms;
use awaken_runtime::RunActivation;

use super::{
    Mailbox, MailboxError, MailboxLifecycleConfig, MailboxLifecycleHandle, MailboxLifecycleTasks,
    MailboxMaintenanceCallback, MailboxStartupRecoveryConfig, MailboxWorkerStatus,
    record_mailbox_operation_result, result_label,
};

impl Mailbox {
    // ── Lifecycle ────────────────────────────────────────────────────

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

        // Reload all queued mailbox IDs and try to dispatch.
        let thread_ids = self.store.queued_thread_ids().await?;
        for thread_id in &thread_ids {
            // Ensure worker exists for each thread with queued dispatches.
            self.get_or_create_worker(thread_id).await;
            self.try_dispatch_next(thread_id).await;
        }

        // Recover orphaned background-task waits with no queued wake dispatch.
        {
            let query = awaken_contract::contract::storage::RunQuery {
                status: Some(awaken_contract::contract::lifecycle::RunStatus::Waiting),
                limit: 200,
                ..Default::default()
            };
            if let Ok(page) = self.run_store.list_runs(&query).await {
                let queued_set: std::collections::HashSet<String> =
                    thread_ids.iter().cloned().collect();
                for run in &page.items {
                    if !run.is_background_task_waiting() {
                        continue;
                    }
                    // Skip if this thread already has a queued dispatch.
                    if queued_set.contains(&run.thread_id) {
                        continue;
                    }
                    let request = RunActivation::new(
                        run.thread_id.clone(),
                        vec![Message::internal_user("<background-tasks-updated />")],
                    )
                    .with_agent_id(run.agent_id.clone())
                    .with_continue_run_id(run.run_id.clone())
                    .with_origin(awaken_contract::contract::storage::RunRequestOrigin::Internal)
                    .with_run_mode(RunMode::InternalWake)
                    .with_adapter(AdapterKind::Internal);
                    if self.submit_background(request).await.is_ok() {
                        total += 1;
                        tracing::info!(
                            thread_id = %run.thread_id,
                            run_id = %run.run_id,
                            "recover: enqueued wake dispatch for orphaned background-task thread"
                        );
                    }
                }
            }
        }

        Ok(total)
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
    }

    async fn run_gc(&self) {
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
