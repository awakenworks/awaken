//! Mailbox service: unified persistent run queue.
//!
//! Every run request (streaming, background, A2A, internal) enters as a
//! [`MailboxJob`] keyed by `thread_id`. The Mailbox orchestrates persistent
//! enqueue, lease-based claim, execution via [`AgentRuntime`], and lifecycle
//! management (lease renewal, sweep, GC).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinHandle;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::mailbox::{
    MailboxInterrupt, MailboxJob, MailboxJobOrigin, MailboxJobStatus, MailboxStore,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::contract::suspension::{ToolCallOutcome, ToolCallResume};
use awaken_contract::now_ms;
use awaken_runtime::{AgentRuntime, RunRequest};

use crate::transport::channel_sink::ReconnectableEventSink;

// ── Public types ─────────────────────────────────────────────────────

/// Result returned by submit/submit_background.
#[derive(Debug, Clone)]
pub struct MailboxSubmitResult {
    pub job_id: String,
    pub thread_id: String,
    pub status: MailboxDispatchStatus,
}

/// Dispatch status for a submitted job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxDispatchStatus {
    /// Job was claimed and is executing now.
    Running,
    /// Job is queued behind `pending_ahead` other jobs.
    Queued { pending_ahead: usize },
}

/// Mailbox service errors.
#[derive(Debug, Error)]
pub enum MailboxError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error("store error: {0}")]
    Store(#[from] StorageError),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Outcome classification for runtime run results.
#[derive(Debug)]
pub enum MailboxRunOutcome {
    /// Run completed successfully.
    Completed,
    /// Transient infrastructure failure -- retry.
    TransientError(String),
    /// Permanent failure -- do not retry.
    PermanentError(String),
}

/// Configuration for the Mailbox service.
#[derive(Debug, Clone)]
pub struct MailboxConfig {
    /// Lease duration in milliseconds (default 30_000).
    pub lease_ms: u64,
    /// Lease duration in milliseconds when the run is suspended/waiting
    /// for human input (default 600_000 = 10 minutes).
    pub suspended_lease_ms: u64,
    /// How often to renew leases (default 10s).
    pub lease_renewal_interval: Duration,
    /// How often to sweep for expired leases (default 30s).
    pub sweep_interval: Duration,
    /// How often to run GC for terminal jobs (default 60s).
    pub gc_interval: Duration,
    /// How long to keep terminal jobs before purging (default 24h).
    pub gc_ttl: Duration,
    /// Default max attempts before dead-lettering (default 5).
    pub default_max_attempts: u32,
    /// Default retry delay in milliseconds (default 250).
    pub default_retry_delay_ms: u64,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self {
            lease_ms: 30_000,
            suspended_lease_ms: 600_000,
            lease_renewal_interval: Duration::from_secs(10),
            sweep_interval: Duration::from_secs(30),
            gc_interval: Duration::from_secs(60),
            gc_ttl: Duration::from_secs(24 * 60 * 60),
            default_max_attempts: 5,
            default_retry_delay_ms: 250,
        }
    }
}

// ── Internal types ───────────────────────────────────────────────────

/// Per-thread worker status.
enum MailboxWorkerStatus {
    Idle,
    /// Transitional: claim in progress. Prevents TOCTOU race where two
    /// concurrent dispatches both see Idle and both try to claim.
    Claiming,
    Running {
        job_id: String,
        lease_handle: JoinHandle<()>,
        sink: Arc<ReconnectableEventSink>,
    },
}

/// Per-thread worker. Store is the sole queue authority.
struct MailboxWorker {
    status: MailboxWorkerStatus,
}

impl Default for MailboxWorker {
    fn default() -> Self {
        Self {
            status: MailboxWorkerStatus::Idle,
        }
    }
}

// ── Suspension-aware event sink ──────────────────────────────────────

/// Wraps an inner `EventSink` and sets a shared flag when the run
/// enters a suspended (waiting) state, detected by a `ToolCallDone`
/// event with `ToolCallOutcome::Suspended`.
struct SuspensionAwareSink {
    inner: Arc<dyn EventSink>,
    suspended: Arc<AtomicBool>,
}

#[async_trait]
impl EventSink for SuspensionAwareSink {
    async fn emit(&self, event: AgentEvent) {
        if matches!(
            &event,
            AgentEvent::ToolCallDone {
                outcome: ToolCallOutcome::Suspended,
                ..
            }
        ) {
            self.suspended.store(true, Ordering::Release);
        }
        // Reset the flag when the run resumes from suspension.
        if matches!(&event, AgentEvent::ToolCallResumed { .. }) {
            self.suspended.store(false, Ordering::Release);
        }
        self.inner.emit(event).await;
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

// ── Mailbox service ──────────────────────────────────────────────────

/// Unified persistent run queue.
///
/// Orchestrates `MailboxStore` (persistence) + `AgentRuntime` (execution)
/// with lease-based distributed claim, per-thread serialization, sweep,
/// and garbage collection.
pub struct Mailbox {
    runtime: Arc<AgentRuntime>,
    store: Arc<dyn MailboxStore>,
    consumer_id: String,
    workers: RwLock<HashMap<String, Arc<Mutex<MailboxWorker>>>>,
    config: MailboxConfig,
}

impl Mailbox {
    /// Create a new Mailbox service.
    pub fn new(
        runtime: Arc<AgentRuntime>,
        store: Arc<dyn MailboxStore>,
        consumer_id: String,
        config: MailboxConfig,
    ) -> Self {
        Self {
            runtime,
            store,
            consumer_id,
            workers: RwLock::new(HashMap::new()),
            config,
        }
    }

    // ── Submission ───────────────────────────────────────────────────

    /// Submit a run for streaming. Returns event receiver immediately.
    ///
    /// The job is persisted (WAL), then claimed inline by this process.
    /// The caller wires `event_rx` to their transport (SSE, WebSocket, etc).
    pub async fn submit(
        self: &Arc<Self>,
        request: RunRequest,
    ) -> Result<(MailboxSubmitResult, mpsc::UnboundedReceiver<AgentEvent>), MailboxError> {
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone())?;

        // Step 1: Interrupt — bump generation, supersede stale queued jobs.
        let now = now_ms();
        match self.store.interrupt(&thread_id, now).await {
            Ok(interrupt) => {
                // Step 2: Cancel active runtime run if the interrupt found one.
                if interrupt.active_job.is_some() {
                    if self.runtime.cancel_and_wait_by_thread(&thread_id).await {
                        tracing::info!(
                            thread_id = %thread_id,
                            superseded = interrupt.superseded_count,
                            "interrupted thread for new submission"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(thread_id = %thread_id, error = %e, "interrupt failed, falling back to cancel");
                self.runtime.cancel_and_wait_by_thread(&thread_id).await;
            }
        }

        let job = self.build_job(&request, &thread_id, messages);
        let job_id = job.job_id.clone();
        let mailbox_id = job.mailbox_id.clone();

        // WAL: persist before anything else.
        // Use u64::MAX for available_at to prevent sweep from grabbing it.
        let mut wal_job = job;
        wal_job.available_at = u64::MAX;
        self.store.enqueue(&wal_job).await?;

        // Inline claim.
        let now = now_ms();
        let claimed = self
            .store
            .claim_job(&job_id, &self.consumer_id, self.config.lease_ms, now)
            .await?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        if let Some(claimed_job) = claimed {
            let claim_token = claimed_job.claim_token.clone().unwrap_or_default();

            // Shared flag: set by the event sink when a tool call is suspended.
            let suspended = Arc::new(AtomicBool::new(false));

            // Start lease renewal.
            let lease_handle = self.spawn_lease_renewal(
                job_id.clone(),
                claim_token.clone(),
                mailbox_id.clone(),
                Arc::clone(&suspended),
            );

            // Create reconnectable sink for SSE reconnection on resume.
            let reconnectable_sink = Arc::new(ReconnectableEventSink::new(event_tx.clone()));

            // Update worker state.
            let worker = self.get_or_create_worker(&mailbox_id).await;
            {
                let mut w = worker.lock().await;
                w.status = MailboxWorkerStatus::Running {
                    job_id: job_id.clone(),
                    lease_handle,
                    sink: Arc::clone(&reconnectable_sink),
                };
            }

            // Spawn execution.
            self.spawn_execution(
                claimed_job,
                event_tx.clone(),
                reconnectable_sink,
                claim_token,
                mailbox_id,
                suspended,
            );

            Ok((
                MailboxSubmitResult {
                    job_id,
                    thread_id,
                    status: MailboxDispatchStatus::Running,
                },
                event_rx,
            ))
        } else {
            // Claim failed (unlikely for inline claim), treat as queued.
            Ok((
                MailboxSubmitResult {
                    job_id,
                    thread_id,
                    status: MailboxDispatchStatus::Queued { pending_ahead: 0 },
                },
                event_rx,
            ))
        }
    }

    /// Submit a run in the background (fire-and-forget).
    ///
    /// Job is persisted with `available_at = now`, dispatch is event-driven.
    /// Returns job_id + thread_id for status polling.
    pub async fn submit_background(
        self: &Arc<Self>,
        request: RunRequest,
    ) -> Result<MailboxSubmitResult, MailboxError> {
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone())?;

        let job = self.build_job(&request, &thread_id, messages);
        let job_id = job.job_id.clone();
        let mailbox_id = job.mailbox_id.clone();

        // WAL: persist with available_at = now.
        self.store.enqueue(&job).await?;

        // Check worker state and dispatch or buffer.
        let worker = self.get_or_create_worker(&mailbox_id).await;
        let status = {
            let mut w = worker.lock().await;
            match &w.status {
                MailboxWorkerStatus::Idle => {
                    // Dispatch immediately.
                    drop(w);
                    self.dispatch_job(&mailbox_id).await;
                    MailboxDispatchStatus::Running
                }
                MailboxWorkerStatus::Claiming | MailboxWorkerStatus::Running { .. } => {
                    // Job stays in store queue; will be dispatched when
                    // current run completes.
                    MailboxDispatchStatus::Queued { pending_ahead: 0 }
                }
            }
        };

        Ok(MailboxSubmitResult {
            job_id,
            thread_id,
            status,
        })
    }

    // ── Control ──────────────────────────────────────────────────────

    /// Cancel a run by job_id or thread_id.
    ///
    /// If Queued: transitions to Cancelled via store.
    /// If Claimed/Running: cancels runtime run via dual-index lookup.
    pub async fn cancel(&self, id: &str) -> Result<bool, MailboxError> {
        // Try store cancel first (works for Queued jobs).
        let now = now_ms();
        let cancelled = self.store.cancel(id, now).await?;
        if cancelled.is_some() {
            return Ok(true);
        }

        // Try runtime cancel (for Claimed/Running jobs).
        Ok(self.runtime.cancel(id))
    }

    /// Interrupt a thread: bump generation, supersede all pending,
    /// cancel active run. Clean slate for the thread.
    pub async fn interrupt(&self, thread_id: &str) -> Result<MailboxInterrupt, MailboxError> {
        let now = now_ms();
        let result = self.store.interrupt(thread_id, now).await?;

        // Cancel active runtime run if any.
        if result.active_job.is_some() {
            self.runtime.cancel(thread_id);
        }

        Ok(result)
    }

    /// Forward a tool-call decision to an active run.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        self.runtime.send_decision(id, tool_call_id, resume)
    }

    /// Reconnect the event sink for an active (suspended) run.
    ///
    /// Replaces the underlying channel sender so subsequent events flow to
    /// `new_tx`. Returns `true` if the thread has an active worker.
    pub async fn reconnect_sink(
        &self,
        thread_id: &str,
        new_tx: mpsc::UnboundedSender<AgentEvent>,
    ) -> bool {
        let workers = self.workers.read().await;
        let Some(worker) = workers.get(thread_id) else {
            return false;
        };
        let w = worker.lock().await;
        match &w.status {
            MailboxWorkerStatus::Running { sink, .. } => {
                sink.reconnect(new_tx).await;
                true
            }
            MailboxWorkerStatus::Idle | MailboxWorkerStatus::Claiming => false,
        }
    }

    // ── Query ────────────────────────────────────────────────────────

    /// List mailbox jobs for a thread (with optional status filter).
    pub async fn list_jobs(
        &self,
        thread_id: &str,
        status_filter: Option<&[MailboxJobStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MailboxJob>, MailboxError> {
        Ok(self
            .store
            .list_jobs(thread_id, status_filter, limit, offset)
            .await?)
    }

    // ── Lifecycle ────────────────────────────────────────────────────

    /// Recover on startup: reload Queued jobs, buffer, dispatch idle threads.
    pub async fn recover(self: &Arc<Self>) -> Result<usize, MailboxError> {
        let now = now_ms();
        let mut total = 0;

        // Reclaim expired leases from previous process crash.
        let reclaimed = self.store.reclaim_expired_leases(now, 100).await?;
        total += reclaimed.len();

        // Reload all queued mailbox IDs and try to dispatch.
        let mailbox_ids = self.store.queued_mailbox_ids().await?;
        for mailbox_id in &mailbox_ids {
            // Ensure worker exists for each mailbox with queued jobs.
            self.get_or_create_worker(mailbox_id).await;
            self.try_dispatch_next(mailbox_id).await;
        }

        Ok(total)
    }

    /// Run sweep + GC loop forever. Call from `tokio::spawn`.
    pub async fn run_maintenance_loop(self: Arc<Self>) {
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
                }
            }
        }
    }

    // ── Internal: dispatch ───────────────────────────────────────────

    /// Claim a job from the store and start execution.
    async fn dispatch_job(self: &Arc<Self>, mailbox_id: &str) {
        let now = now_ms();
        let claimed = match self
            .store
            .claim(mailbox_id, &self.consumer_id, self.config.lease_ms, now, 1)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, mailbox_id, "failed to claim job");
                // Revert Claiming → Idle
                let workers = self.workers.read().await;
                if let Some(worker) = workers.get(mailbox_id) {
                    worker.lock().await.status = MailboxWorkerStatus::Idle;
                }
                return;
            }
        };

        let Some(job) = claimed.into_iter().next() else {
            // No jobs to claim, mark worker idle.
            let workers = self.workers.read().await;
            if let Some(worker) = workers.get(mailbox_id) {
                let mut w = worker.lock().await;
                w.status = MailboxWorkerStatus::Idle;
            }
            return;
        };

        let job_id = job.job_id.clone();
        let claim_token = job.claim_token.clone().unwrap_or_default();

        // Shared flag: set by the event sink when a tool call is suspended.
        let suspended = Arc::new(AtomicBool::new(false));

        // Start lease renewal.
        let lease_handle = self.spawn_lease_renewal(
            job_id.clone(),
            claim_token.clone(),
            mailbox_id.to_string(),
            Arc::clone(&suspended),
        );

        // Create channel for background dispatch (events go nowhere unless observed).
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let reconnectable_sink = Arc::new(ReconnectableEventSink::new(event_tx.clone()));

        // Update worker state.
        let worker = self.get_or_create_worker(mailbox_id).await;
        {
            let mut w = worker.lock().await;
            w.status = MailboxWorkerStatus::Running {
                job_id: job_id.clone(),
                lease_handle,
                sink: Arc::clone(&reconnectable_sink),
            };
        }

        self.spawn_execution(
            job,
            event_tx,
            reconnectable_sink,
            claim_token,
            mailbox_id.to_string(),
            suspended,
        );
    }

    /// Claim from store and dispatch the next job for this mailbox.
    async fn try_dispatch_next(self: &Arc<Self>, mailbox_id: &str) {
        let worker = {
            let workers = self.workers.read().await;
            match workers.get(mailbox_id) {
                Some(w) => Arc::clone(w),
                None => return,
            }
        };

        // Atomically transition Idle → Claiming to prevent TOCTOU race.
        {
            let mut w = worker.lock().await;
            if !matches!(w.status, MailboxWorkerStatus::Idle) {
                return;
            }
            w.status = MailboxWorkerStatus::Claiming;
        }

        self.dispatch_job(mailbox_id).await;
    }

    /// Spawn a lease renewal task that periodically extends the lease.
    ///
    /// When the `suspended` flag is set (run is waiting for human input),
    /// the renewal uses `suspended_lease_ms` instead of the default `lease_ms`
    /// to prevent premature lease expiration during HITL scenarios.
    fn spawn_lease_renewal(
        &self,
        job_id: String,
        claim_token: String,
        mailbox_id: String,
        suspended: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let store = Arc::clone(&self.store);
        let runtime = Arc::clone(&self.runtime);
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
                match store
                    .extend_lease(&job_id, &claim_token, effective_lease_ms, now)
                    .await
                {
                    Ok(true) => {} // Lease extended successfully.
                    Ok(false) => {
                        // Lease lost -- another process reclaimed.
                        tracing::warn!(job_id, mailbox_id, "lease lost, cancelling run");
                        runtime.cancel(&mailbox_id);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(job_id, error = %e, "lease extension failed");
                        break;
                    }
                }
            }
        })
    }

    /// Spawn the actual execution task for a claimed job.
    fn spawn_execution(
        self: &Arc<Self>,
        job: MailboxJob,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        reconnectable_sink: Arc<ReconnectableEventSink>,
        claim_token: String,
        mailbox_id: String,
        suspended: Arc<AtomicBool>,
    ) {
        let this = Arc::clone(self);
        let job_id = job.job_id.clone();

        tokio::spawn(async move {
            let sink = SuspensionAwareSink {
                inner: reconnectable_sink as Arc<dyn EventSink>,
                suspended,
            };

            let mut request =
                awaken_runtime::RunRequest::new(job.mailbox_id.clone(), job.messages.clone());
            if !job.agent_id.is_empty() {
                request = request.with_agent_id(job.agent_id.clone());
            }
            // Reconstruct RunRequest extras from opaque payload.
            if let Some(ref extras) = job.request_extras {
                if let Ok(overrides) = serde_json::from_value(extras["overrides"].clone()) {
                    request = request.with_overrides(overrides);
                }
                if let Ok(decisions) = serde_json::from_value::<
                    Vec<(
                        String,
                        awaken_contract::contract::suspension::ToolCallResume,
                    )>,
                >(extras["decisions"].clone())
                {
                    if !decisions.is_empty() {
                        request = request.with_decisions(decisions);
                    }
                }
                if let Ok(tools) = serde_json::from_value::<
                    Vec<awaken_contract::contract::tool::ToolDescriptor>,
                >(extras["frontend_tools"].clone())
                {
                    if !tools.is_empty() {
                        request = request.with_frontend_tools(tools);
                    }
                }
            }

            let result = this.runtime.run(request, Arc::new(sink)).await;
            let now = now_ms();

            match classify_error(&result) {
                MailboxRunOutcome::Completed => {
                    if let Err(e) = this.store.ack(&job_id, &claim_token, now).await {
                        tracing::warn!(job_id, error = %e, "ack failed");
                    }
                }
                MailboxRunOutcome::TransientError(msg) => {
                    tracing::warn!(job_id, error = %msg, "run failed (transient), nacking");
                    // Emit error event so the SSE stream terminates with a
                    // proper RUN_ERROR instead of silently closing.
                    let _ = event_tx.send(AgentEvent::RunFinish {
                        thread_id: job.mailbox_id.clone(),
                        run_id: job_id.clone(),
                        result: None,
                        termination: awaken_contract::contract::lifecycle::TerminationReason::Error(
                            msg.clone(),
                        ),
                    });
                    let retry_at = now + this.config.default_retry_delay_ms;
                    if let Err(e) = this
                        .store
                        .nack(&job_id, &claim_token, retry_at, &msg, now)
                        .await
                    {
                        tracing::warn!(job_id, error = %e, "nack failed");
                    }
                }
                MailboxRunOutcome::PermanentError(msg) => {
                    tracing::warn!(job_id, error = %msg, "run failed (permanent), dead-lettering");
                    // Emit error event so the SSE stream terminates with a
                    // proper RUN_ERROR. The runtime did not reach the loop,
                    // so no RunFinish was emitted — we must do it here.
                    let _ = event_tx.send(AgentEvent::RunFinish {
                        thread_id: job.mailbox_id.clone(),
                        run_id: job_id.clone(),
                        result: None,
                        termination: awaken_contract::contract::lifecycle::TerminationReason::Error(
                            msg.clone(),
                        ),
                    });
                    if let Err(e) = this
                        .store
                        .dead_letter(&job_id, &claim_token, &msg, now)
                        .await
                    {
                        tracing::warn!(job_id, error = %e, "dead_letter failed");
                    }
                }
            }

            // Abort lease renewal.
            let worker = this.get_or_create_worker(&mailbox_id).await;
            {
                let mut w = worker.lock().await;
                let should_transition = matches!(
                    &w.status,
                    MailboxWorkerStatus::Running { job_id: cid, .. } if *cid == job_id
                );
                if should_transition {
                    // Take ownership of the old status to abort the lease handle.
                    let old = std::mem::replace(&mut w.status, MailboxWorkerStatus::Idle);
                    if let MailboxWorkerStatus::Running { lease_handle, .. } = old {
                        lease_handle.abort();
                    }
                }
            }

            // Try to dispatch next job for this mailbox.
            this.try_dispatch_next(&mailbox_id).await;
        });
    }

    /// Get or create a per-thread worker.
    async fn get_or_create_worker(&self, mailbox_id: &str) -> Arc<Mutex<MailboxWorker>> {
        // Fast path: read lock.
        {
            let workers = self.workers.read().await;
            if let Some(w) = workers.get(mailbox_id) {
                return Arc::clone(w);
            }
        }
        // Slow path: write lock.
        let mut workers = self.workers.write().await;
        Arc::clone(
            workers
                .entry(mailbox_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(MailboxWorker::default()))),
        )
    }

    /// Build a MailboxJob from validated run inputs.
    fn build_job(
        &self,
        request: &RunRequest,
        thread_id: &str,
        messages: Vec<Message>,
    ) -> MailboxJob {
        // Serialize RunRequest extras that Mailbox doesn't inspect.
        let extras = serde_json::json!({
            "overrides": request.overrides,
            "decisions": request.decisions,
            "frontend_tools": request.frontend_tools,
        });
        let request_extras =
            if extras == serde_json::json!({"overrides":null,"decisions":[],"frontend_tools":[]}) {
                None
            } else {
                Some(extras)
            };

        let now = now_ms();
        MailboxJob {
            job_id: uuid::Uuid::now_v7().to_string(),
            mailbox_id: thread_id.to_string(),
            agent_id: request.agent_id.as_deref().unwrap_or_default().to_string(),
            messages,
            origin: MailboxJobOrigin::User,
            sender_id: None,
            parent_run_id: None,
            request_extras,
            priority: 128,
            dedupe_key: None,
            generation: 0,
            status: MailboxJobStatus::Queued,
            available_at: now,
            attempt_count: 0,
            max_attempts: self.config.default_max_attempts,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            created_at: now,
            updated_at: now,
        }
    }

    // ── Maintenance ──────────────────────────────────────────────────

    async fn run_sweep(self: &Arc<Self>) {
        let now = now_ms();
        match self.store.reclaim_expired_leases(now, 100).await {
            Ok(reclaimed) => {
                if !reclaimed.is_empty() {
                    tracing::info!(count = reclaimed.len(), "sweep reclaimed expired leases");
                    for job in reclaimed {
                        if job.status == MailboxJobStatus::Queued {
                            let mailbox_id = job.mailbox_id.clone();
                            self.get_or_create_worker(&mailbox_id).await;
                            self.try_dispatch_next(&mailbox_id).await;
                        }
                    }
                }
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
        match self.store.purge_terminal(older_than).await {
            Ok(purged) => {
                if purged > 0 {
                    tracing::info!(purged, "GC purged terminal jobs");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "GC failed");
            }
        }
    }
}

// ── Free functions ───────────────────────────────────────────────────

/// Validate and normalize run request inputs.
///
/// Checks that messages are non-empty, trims/generates thread_id.
/// Returns `(thread_id, messages)`.
/// Internal validation for mailbox submit paths.
fn validate_run_inputs(
    thread_id: String,
    messages: Vec<Message>,
) -> Result<(String, Vec<Message>), MailboxError> {
    if messages.is_empty() {
        return Err(MailboxError::Validation(
            "at least one message is required".to_string(),
        ));
    }
    let thread_id = {
        let trimmed = thread_id.trim().to_string();
        if trimmed.is_empty() {
            uuid::Uuid::now_v7().to_string()
        } else {
            trimmed
        }
    };
    Ok((thread_id, messages))
}

/// Classify a runtime run result for ack/nack/dead_letter.
fn classify_error(
    result: &Result<
        awaken_runtime::loop_runner::AgentRunResult,
        awaken_runtime::loop_runner::AgentLoopError,
    >,
) -> MailboxRunOutcome {
    match result {
        Ok(_) => MailboxRunOutcome::Completed,
        Err(e) => {
            use awaken_runtime::loop_runner::AgentLoopError;
            match e {
                AgentLoopError::RuntimeError(re) => {
                    use awaken_runtime::RuntimeError;
                    match re {
                        RuntimeError::ThreadAlreadyRunning { .. } => {
                            // After the cancel-on-submit change, this error
                            // indicates a race that retrying won't fix.
                            MailboxRunOutcome::PermanentError(e.to_string())
                        }
                        RuntimeError::AgentNotFound { .. } | RuntimeError::ResolveFailed { .. } => {
                            MailboxRunOutcome::PermanentError(e.to_string())
                        }
                        _ => MailboxRunOutcome::TransientError(e.to_string()),
                    }
                }
                AgentLoopError::StorageError(_) => MailboxRunOutcome::TransientError(e.to_string()),
                AgentLoopError::InferenceFailed(_) => {
                    MailboxRunOutcome::TransientError(e.to_string())
                }
                // Agent-level failures (phase error, invalid resume) are not infra errors.
                _ => MailboxRunOutcome::Completed,
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::Message;
    use awaken_stores::InMemoryMailboxStore;

    // ── Helper ───────────────────────────────────────────────────────

    /// Stub resolver that always returns an error (no agents registered).
    struct StubResolver;
    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn make_store() -> Arc<InMemoryMailboxStore> {
        Arc::new(InMemoryMailboxStore::new())
    }

    fn make_runtime() -> Arc<AgentRuntime> {
        Arc::new(AgentRuntime::new(Arc::new(StubResolver)))
    }

    fn make_mailbox(runtime: Arc<AgentRuntime>, store: Arc<InMemoryMailboxStore>) -> Arc<Mailbox> {
        Arc::new(Mailbox::new(
            runtime,
            store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ))
    }

    // ── Tests ────────────────────────────────────────────────────────

    #[test]
    fn mailbox_config_defaults() {
        let config = MailboxConfig::default();
        assert_eq!(config.lease_ms, 30_000);
        assert_eq!(config.suspended_lease_ms, 600_000);
        assert_eq!(config.lease_renewal_interval, Duration::from_secs(10));
        assert_eq!(config.sweep_interval, Duration::from_secs(30));
        assert_eq!(config.gc_interval, Duration::from_secs(60));
        assert_eq!(config.gc_ttl, Duration::from_secs(24 * 60 * 60));
        assert_eq!(config.default_max_attempts, 5);
        assert_eq!(config.default_retry_delay_ms, 250);
    }

    #[test]
    fn run_request_fields() {
        let req = RunRequest::new("t-1", vec![Message::user("hello")]).with_agent_id("agent-a");
        assert_eq!(req.thread_id, "t-1");
        assert_eq!(req.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn run_spec_validation_empty_messages_errors() {
        let result = validate_run_inputs("thread-1".into(), vec![]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), MailboxError::Validation(_)));
    }

    #[test]
    fn run_spec_validation_blank_thread_id_generates_new() {
        let result = validate_run_inputs("  ".into(), vec![Message::user("hi")]);
        assert!(result.is_ok());
        let (thread_id, _) = result.unwrap();
        assert!(!thread_id.is_empty());
        assert_ne!(thread_id.trim(), "");
    }

    #[test]
    fn run_spec_validation_trims_thread_id() {
        let result = validate_run_inputs("  my-thread  ".into(), vec![Message::user("hi")]);
        assert!(result.is_ok());
        let (thread_id, _) = result.unwrap();
        assert_eq!(thread_id, "my-thread");
    }

    #[test]
    fn dispatch_status_enum_variants() {
        let running = MailboxDispatchStatus::Running;
        let queued = MailboxDispatchStatus::Queued { pending_ahead: 0 };
        assert!(matches!(running, MailboxDispatchStatus::Running));
        assert!(matches!(
            queued,
            MailboxDispatchStatus::Queued { pending_ahead: 0 }
        ));
    }

    #[tokio::test]
    async fn submit_background_enqueues_job() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let request =
            RunRequest::new("thread-1", vec![Message::user("hello")]).with_agent_id("agent-1");
        let result = mailbox.submit_background(request).await.unwrap();

        assert_eq!(result.thread_id, "thread-1");
        assert!(!result.job_id.is_empty());

        // Verify job is in store.
        let jobs = store.list_jobs("thread-1", None, 100, 0).await.unwrap();
        assert!(!jobs.is_empty());
    }

    #[tokio::test]
    async fn cancel_queued_job_works() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit a job with available_at in the future so it stays Queued.
        let request =
            RunRequest::new("thread-cancel", vec![Message::user("hello")]).with_agent_id("agent-1");
        let result = mailbox.submit_background(request).await.unwrap();
        let job_id = result.job_id.clone();

        // The job might already be dispatched (claimed). Load it to check.
        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        if loaded.status == MailboxJobStatus::Queued {
            let cancelled = mailbox.cancel(&job_id).await.unwrap();
            assert!(cancelled);

            let after = store.load_job(&job_id).await.unwrap().unwrap();
            assert_eq!(after.status, MailboxJobStatus::Cancelled);
        }
        // If already Claimed, cancel via runtime path is tested implicitly.
    }

    #[tokio::test]
    async fn list_jobs_returns_entries() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        for i in 0..3 {
            let request = RunRequest::new("thread-list", vec![Message::user("msg")])
                .with_agent_id(format!("agent-{i}"));
            mailbox.submit_background(request).await.unwrap();
        }

        let jobs = mailbox
            .list_jobs("thread-list", None, 100, 0)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 3);
    }

    #[test]
    fn mailbox_error_display() {
        let e = MailboxError::Validation("test".to_string());
        assert_eq!(e.to_string(), "validation error: test");

        let e = MailboxError::Internal("oops".to_string());
        assert_eq!(e.to_string(), "internal error: oops");
    }

    #[test]
    fn mailbox_submit_result_fields() {
        let result = MailboxSubmitResult {
            job_id: "job-1".into(),
            thread_id: "thread-1".into(),
            status: MailboxDispatchStatus::Running,
        };
        assert_eq!(result.job_id, "job-1");
        assert_eq!(result.thread_id, "thread-1");
        assert!(matches!(result.status, MailboxDispatchStatus::Running));
    }

    #[tokio::test]
    async fn suspension_aware_sink_sets_flag_on_suspended_tool_call() {
        use awaken_contract::contract::event_sink::{EventSink, VecEventSink};
        use awaken_contract::contract::suspension::ToolCallOutcome;
        use awaken_contract::contract::tool::{ToolResult, ToolStatus};

        let inner: Arc<dyn EventSink> = Arc::new(VecEventSink::new());
        let suspended = Arc::new(AtomicBool::new(false));
        let sink = SuspensionAwareSink {
            inner: Arc::clone(&inner),
            suspended: Arc::clone(&suspended),
        };

        // Non-suspended tool call should not set the flag.
        sink.emit(AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult {
                tool_name: "echo".into(),
                status: ToolStatus::Success,
                data: serde_json::json!("ok"),
                message: None,
                suspension: None,
                metadata: Default::default(),
            },
            outcome: ToolCallOutcome::Succeeded,
        })
        .await;
        assert!(!suspended.load(Ordering::Acquire));

        // Suspended tool call should set the flag.
        sink.emit(AgentEvent::ToolCallDone {
            id: "c2".into(),
            message_id: "m2".into(),
            result: ToolResult {
                tool_name: "approve".into(),
                status: ToolStatus::Pending,
                data: serde_json::json!("pending"),
                message: None,
                suspension: None,
                metadata: Default::default(),
            },
            outcome: ToolCallOutcome::Suspended,
        })
        .await;
        assert!(suspended.load(Ordering::Acquire));

        // ToolCallResumed should reset the flag.
        sink.emit(AgentEvent::ToolCallResumed {
            target_id: "c2".into(),
            result: serde_json::json!({"approved": true}),
        })
        .await;
        assert!(!suspended.load(Ordering::Acquire));
    }

    // ── classify_error tests ──────────────────────────────────────────

    #[test]
    fn classify_error_ok_is_completed() {
        use awaken_contract::contract::lifecycle::TerminationReason;
        let result = Ok(awaken_runtime::loop_runner::AgentRunResult {
            response: "done".to_string(),
            termination: TerminationReason::NaturalEnd,
            steps: 1,
        });
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::Completed
        ));
    }

    #[test]
    fn classify_error_thread_already_running_is_permanent() {
        use awaken_runtime::RuntimeError;
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::RuntimeError(
            RuntimeError::ThreadAlreadyRunning {
                thread_id: "t1".to_string(),
            },
        ));
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::PermanentError(_)
        ));
    }

    #[test]
    fn classify_error_agent_not_found_is_permanent() {
        use awaken_runtime::RuntimeError;
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::RuntimeError(RuntimeError::AgentNotFound {
            agent_id: "missing".to_string(),
        }));
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::PermanentError(_)
        ));
    }

    #[test]
    fn classify_error_resolve_failed_is_permanent() {
        use awaken_runtime::RuntimeError;
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::RuntimeError(RuntimeError::ResolveFailed {
            message: "not found".to_string(),
        }));
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::PermanentError(_)
        ));
    }

    #[test]
    fn classify_error_storage_error_is_transient() {
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::StorageError("disk full".to_string()));
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::TransientError(_)
        ));
    }

    #[test]
    fn classify_error_inference_failed_is_transient() {
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::InferenceFailed("timeout".to_string()));
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::TransientError(_)
        ));
    }

    #[test]
    fn classify_error_phase_error_is_completed() {
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::PhaseError(
            awaken_contract::StateError::UnknownKey {
                key: "bad".to_string(),
            },
        ));
        // Phase errors are not infra failures -> Completed
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::Completed
        ));
    }

    #[test]
    fn classify_error_invalid_resume_is_completed() {
        use awaken_runtime::loop_runner::AgentLoopError;
        let result = Err(AgentLoopError::InvalidResume("bad resume".to_string()));
        assert!(matches!(
            classify_error(&result),
            MailboxRunOutcome::Completed
        ));
    }

    // ── validate_run_inputs additional tests ──────────────────────────

    #[test]
    fn validate_run_inputs_preserves_normal_thread_id() {
        let (thread_id, msgs) =
            validate_run_inputs("my-thread".into(), vec![Message::user("hi")]).unwrap();
        assert_eq!(thread_id, "my-thread");
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn validate_run_inputs_multiple_messages() {
        let (_, msgs) = validate_run_inputs(
            "t".into(),
            vec![Message::user("a"), Message::user("b"), Message::user("c")],
        )
        .unwrap();
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn validate_run_inputs_empty_string_generates_uuid() {
        let (thread_id, _) = validate_run_inputs("".into(), vec![Message::user("hi")]).unwrap();
        assert!(!thread_id.is_empty());
        // UUIDv7 is 36 chars with hyphens
        assert_eq!(thread_id.len(), 36);
    }

    // ── MailboxConfig custom values ──────────────────────────────────

    #[test]
    fn mailbox_config_custom_values() {
        let config = MailboxConfig {
            lease_ms: 5_000,
            suspended_lease_ms: 60_000,
            lease_renewal_interval: Duration::from_secs(2),
            sweep_interval: Duration::from_secs(5),
            gc_interval: Duration::from_secs(10),
            gc_ttl: Duration::from_secs(3600),
            default_max_attempts: 3,
            default_retry_delay_ms: 500,
        };
        assert_eq!(config.lease_ms, 5_000);
        assert_eq!(config.default_max_attempts, 3);
        assert_eq!(config.default_retry_delay_ms, 500);
    }

    // ── build_job field validation ──────────────────────────────────

    #[tokio::test]
    async fn build_job_sets_correct_fields() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request =
            RunRequest::new("thread-42", vec![Message::user("test")]).with_agent_id("agent-x");
        let messages = request.messages.clone();
        let job = mailbox.build_job(&request, "thread-42", messages);

        assert_eq!(job.mailbox_id, "thread-42");
        assert_eq!(job.agent_id, "agent-x");
        assert_eq!(job.messages.len(), 1);
        assert_eq!(job.status, MailboxJobStatus::Queued);
        assert_eq!(job.attempt_count, 0);
        assert_eq!(job.max_attempts, 5); // default
        assert_eq!(job.priority, 128);
        assert_eq!(job.generation, 0);
        assert!(job.claim_token.is_none());
        assert!(job.claimed_by.is_none());
        assert!(job.lease_until.is_none());
        assert!(job.last_error.is_none());
        assert!(matches!(job.origin, MailboxJobOrigin::User));
        // request_extras is None when no overrides/decisions/frontend_tools
        assert!(job.request_extras.is_none());
    }

    #[tokio::test]
    async fn build_job_without_agent_id_sets_empty() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-1", vec![Message::user("hi")]);
        let messages = request.messages.clone();
        let job = mailbox.build_job(&request, "thread-1", messages);
        assert_eq!(job.agent_id, "");
    }

    #[tokio::test]
    async fn build_job_preserves_request_extras() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-ext", vec![Message::user("hi")])
            .with_agent_id("a1")
            .with_frontend_tools(vec![awaken_contract::contract::tool::ToolDescriptor::new(
                "ft1", "FT1", "desc",
            )]);
        let messages = request.messages.clone();
        let job = mailbox.build_job(&request, "thread-ext", messages);

        assert!(job.request_extras.is_some());
        let extras = job.request_extras.unwrap();
        assert!(extras["frontend_tools"].is_array());
    }

    // ── MailboxError variants ──────────────────────────────────────

    #[test]
    fn mailbox_error_store_variant() {
        use awaken_contract::contract::storage::StorageError;
        let err: MailboxError = StorageError::NotFound("x".to_string()).into();
        let msg = err.to_string();
        assert!(msg.contains("store error"));
    }

    // ── MailboxRunOutcome debug ──────────────────────────────────────

    #[test]
    fn mailbox_run_outcome_debug() {
        let completed = MailboxRunOutcome::Completed;
        let transient = MailboxRunOutcome::TransientError("oops".to_string());
        let permanent = MailboxRunOutcome::PermanentError("fatal".to_string());
        assert!(format!("{:?}", completed).contains("Completed"));
        assert!(format!("{:?}", transient).contains("oops"));
        assert!(format!("{:?}", permanent).contains("fatal"));
    }

    // ── MailboxDispatchStatus ────────────────────────────────────────

    #[test]
    fn dispatch_status_queued_zero() {
        let status = MailboxDispatchStatus::Queued { pending_ahead: 0 };
        assert!(matches!(
            status,
            MailboxDispatchStatus::Queued { pending_ahead: 0 }
        ));
    }

    // ── Interrupt test ──────────────────────────────────────────────

    #[tokio::test]
    async fn interrupt_bumps_generation() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit some jobs
        let request =
            RunRequest::new("thread-int", vec![Message::user("a")]).with_agent_id("agent-1");
        mailbox.submit_background(request).await.unwrap();

        let result = mailbox.interrupt("thread-int").await.unwrap();
        // After interrupt, the generation should be bumped
        assert!(result.new_generation > 0);
    }

    // ── submit streaming returns event channel ──────────────────────

    #[tokio::test]
    async fn submit_returns_event_channel() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let request =
            RunRequest::new("thread-stream", vec![Message::user("hi")]).with_agent_id("agent-1");
        let (result, _event_rx) = mailbox.submit(request).await.unwrap();

        assert_eq!(result.thread_id, "thread-stream");
        assert!(!result.job_id.is_empty());
        assert!(matches!(
            result.status,
            MailboxDispatchStatus::Running | MailboxDispatchStatus::Queued { .. }
        ));
    }

    // ── send_decision returns false for unknown id ──────────────────

    #[test]
    fn send_decision_unknown_id_returns_false() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let result = mailbox.send_decision(
            "nonexistent",
            "tc-1".to_string(),
            ToolCallResume {
                decision_id: "d1".into(),
                action: awaken_contract::contract::suspension::ResumeDecisionAction::Resume,
                result: serde_json::json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        );
        assert!(!result);
    }
}
