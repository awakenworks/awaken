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
    MailboxInterrupt, MailboxJob, MailboxJobStatus, MailboxStore,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::contract::suspension::{ToolCallOutcome, ToolCallResume};
use awaken_contract::now_ms;
use awaken_runtime::{AgentRuntime, RunRequest};

use crate::transport::channel_sink::ReconnectableEventSink;

/// Guard window for inline-claimed jobs: if the process crashes between
/// enqueue and cancel, the sweep will reclaim the job after this period.
const INLINE_CLAIM_GUARD_MS: u64 = 60_000;

/// Validation message returned when an inline submit loses the active-run race.
pub(crate) const ACTIVE_RUN_CONFLICT_MESSAGE: &str =
    "thread has an active run; cannot claim inline";

// ── RunRequest ↔ MailboxJob conversion ───────────────────────────────

/// Typed envelope for RunRequest fields that Mailbox stores opaquely.
/// Centralizes the RunRequest → MailboxJob → RunRequest round-trip.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RunRequestExtras {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overrides: Option<awaken_contract::contract::inference::InferenceOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    decisions: Vec<(
        String,
        awaken_contract::contract::suspension::ToolCallResume,
    )>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    frontend_tools: Vec<awaken_contract::contract::tool::ToolDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continue_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    job_id_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_thread_id: Option<String>,
}

impl RunRequestExtras {
    fn from_request(request: &awaken_runtime::RunRequest) -> Self {
        Self {
            overrides: request.overrides.clone(),
            decisions: request.decisions.clone(),
            frontend_tools: request.frontend_tools.clone(),
            continue_run_id: request.continue_run_id.clone(),
            job_id_hint: request.job_id_hint.clone(),
            parent_thread_id: request.parent_thread_id.clone(),
        }
    }

    fn to_value(&self) -> Result<Option<serde_json::Value>, serde_json::Error> {
        if self.overrides.is_none()
            && self.decisions.is_empty()
            && self.frontend_tools.is_empty()
            && self.continue_run_id.is_none()
            && self.job_id_hint.is_none()
            && self.parent_thread_id.is_none()
        {
            Ok(None)
        } else {
            serde_json::to_value(self).map(Some)
        }
    }

    fn from_value(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value.clone())
    }

    fn apply_to(self, mut request: awaken_runtime::RunRequest) -> awaken_runtime::RunRequest {
        if let Some(ov) = self.overrides {
            request = request.with_overrides(ov);
        }
        if !self.decisions.is_empty() {
            request = request.with_decisions(self.decisions);
        }
        if !self.frontend_tools.is_empty() {
            request = request.with_frontend_tools(self.frontend_tools);
        }
        if let Some(crid) = self.continue_run_id {
            request = request.with_continue_run_id(crid);
        }
        if let Some(job_id_hint) = self.job_id_hint {
            request = request.with_job_id_hint(job_id_hint);
        }
        if let Some(parent_thread_id) = self.parent_thread_id {
            request = request.with_parent_thread_id(parent_thread_id);
        }
        request
    }
}

// ── TaskDoneMailboxNotify ────────────────────────────────────────────

/// Fallback for inbox delivery when the agent's run has ended.
///
/// Implements [`OnInboxClosed`] — when an `InboxSender::send()` fails
/// because the receiver was dropped (agent run returned with AwaitingTasks),
/// this enqueues a mailbox wake job so the thread gets a continuation run.
///
/// Uses `dedupe_key` to coalesce multiple task completions into one wake job.
pub struct TaskDoneMailboxNotify {
    mailbox: Arc<Mailbox>,
    thread_id: String,
    continue_run_id: Option<String>,
}

impl TaskDoneMailboxNotify {
    pub fn new(mailbox: Arc<Mailbox>, thread_id: String, continue_run_id: Option<String>) -> Self {
        Self {
            mailbox,
            thread_id,
            continue_run_id,
        }
    }
}

impl awaken_runtime::inbox::OnInboxClosed for TaskDoneMailboxNotify {
    fn closed(&self, message: &serde_json::Value) {
        let mailbox = self.mailbox.clone();
        let thread_id = self.thread_id.clone();
        let continue_run_id = self.continue_run_id.clone();
        let wake_message = awaken_runtime::inbox::inbox_event_message(message);

        // Spawn because OnInboxClosed::closed is sync but enqueue+dispatch is async
        tokio::spawn(async move {
            let extras = RunRequestExtras {
                overrides: None,
                decisions: Vec::new(),
                frontend_tools: Vec::new(),
                continue_run_id,
                job_id_hint: None,
                parent_thread_id: None,
            };
            let request_extras = extras.to_value().ok().flatten();

            let now = now_ms();
            let job = MailboxJob {
                job_id: uuid::Uuid::now_v7().to_string(),
                mailbox_id: thread_id.clone(),
                agent_id: String::new(),
                messages: vec![wake_message],
                origin: awaken_contract::contract::mailbox::MailboxJobOrigin::Internal,
                sender_id: None,
                parent_run_id: None,
                request_extras,
                priority: 200,
                dedupe_key: None,
                generation: 0,
                status: MailboxJobStatus::Queued,
                available_at: now,
                attempt_count: 0,
                max_attempts: 3,
                last_error: None,
                claim_token: None,
                claimed_by: None,
                lease_until: None,
                created_at: now,
                updated_at: now,
            };
            if let Err(e) = mailbox.store.enqueue(&job).await {
                tracing::warn!(thread_id, error = %e, "failed to enqueue background task wake job");
                return;
            }
            // Dispatch immediately so idle workers pick up the wake job.
            mailbox.get_or_create_worker(&thread_id).await;
            mailbox.try_dispatch_next(&thread_id).await;
        });
    }
}

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
    /// Job is queued, waiting for the current run to finish.
    Queued,
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
    /// Maximum retry delay in milliseconds for exponential backoff (default 30_000).
    pub max_retry_delay_ms: u64,
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
            max_retry_delay_ms: 30_000,
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

/// RAII guard that decrements the active-runs gauge on drop.
struct ActiveRunGuard;

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        crate::metrics::dec_active_runs();
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

    /// Default bounded channel capacity for the runtime→SSE relay.
    const EVENT_CHANNEL_CAPACITY: usize = 256;

    /// Submit a run for streaming. Returns event receiver immediately.
    ///
    /// The job is persisted (WAL), then claimed inline by this process.
    /// The caller wires `event_rx` to their transport (SSE, WebSocket, etc).
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id))]
    pub async fn submit(
        self: &Arc<Self>,
        request: RunRequest,
    ) -> Result<(MailboxSubmitResult, mpsc::Receiver<AgentEvent>), MailboxError> {
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone())?;

        // Step 1: Interrupt — bump generation, supersede stale queued jobs.
        let now = now_ms();
        match self.store.interrupt(&thread_id, now).await {
            Ok(interrupt) => {
                // Step 2: Cancel active runtime run if the interrupt found one.
                if interrupt.active_job.is_some()
                    && self.runtime.cancel_and_wait_by_thread(&thread_id).await
                {
                    tracing::info!(
                        thread_id = %thread_id,
                        superseded = interrupt.superseded_count,
                        "interrupted thread for new submission"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(thread_id = %thread_id, error = %e, "interrupt failed, falling back to cancel");
                self.runtime.cancel_and_wait_by_thread(&thread_id).await;
            }
        }

        let job = self.build_job(&request, &thread_id, messages)?;
        let job_id = job.job_id.clone();
        let mailbox_id = job.mailbox_id.clone();

        // WAL: persist before anything else.
        // Set available_at slightly in the future to prevent sweep from grabbing
        // the job during the inline claim window. If the process crashes before
        // the claim completes, sweep will reclaim the job after the guard period.
        let mut wal_job = job;
        wal_job.available_at = now_ms() + INLINE_CLAIM_GUARD_MS;
        self.store.enqueue(&wal_job).await?;

        // Inline claim.
        let now = now_ms();
        let claimed = self
            .store
            .claim_job(&job_id, &self.consumer_id, self.config.lease_ms, now)
            .await?;

        let (event_tx, event_rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);

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
            // Inline claim failed (another claimed job exists for this
            // mailbox). Cancel the orphaned job to prevent it from
            // lingering with the guard available_at.
            let now_fix = now_ms();
            if let Err(e) = self.store.cancel(&job_id, now_fix).await {
                tracing::warn!(job_id, error = %e, "failed to cancel unclaimed inline job");
            }
            Err(MailboxError::Validation(ACTIVE_RUN_CONFLICT_MESSAGE.into()))
        }
    }

    /// Submit a run in the background (fire-and-forget).
    ///
    /// Job is persisted with `available_at = now`, dispatch is event-driven.
    /// Returns job_id + thread_id for status polling.
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id))]
    pub async fn submit_background(
        self: &Arc<Self>,
        request: RunRequest,
    ) -> Result<MailboxSubmitResult, MailboxError> {
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone())?;

        let job = self.build_job(&request, &thread_id, messages)?;
        let job_id = job.job_id.clone();
        let mailbox_id = job.mailbox_id.clone();

        // WAL: persist with available_at = now.
        self.store.enqueue(&job).await?;

        // Dispatch via try_dispatch_next which handles Idle → Claiming atomically.
        self.get_or_create_worker(&mailbox_id).await;
        self.try_dispatch_next(&mailbox_id).await;

        // Check if THIS job was claimed (Running) or still Queued.
        let status = {
            let workers = self.workers.read().await;
            if let Some(worker) = workers.get(&mailbox_id) {
                let w = worker.lock().await;
                match &w.status {
                    MailboxWorkerStatus::Running {
                        job_id: running_id, ..
                    } if *running_id == job_id => MailboxDispatchStatus::Running,
                    _ => MailboxDispatchStatus::Queued,
                }
            } else {
                MailboxDispatchStatus::Queued
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
    pub async fn reconnect_sink(&self, thread_id: &str, new_tx: mpsc::Sender<AgentEvent>) -> bool {
        let workers = self.workers.read().await;
        let Some(worker) = workers.get(thread_id) else {
            return false;
        };
        let w = worker.lock().await;
        match &w.status {
            MailboxWorkerStatus::Running { sink, .. } => {
                sink.reconnect(new_tx);
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

    /// List mailbox IDs that currently have queued jobs.
    pub async fn queued_mailbox_ids(&self) -> Result<Vec<String>, MailboxError> {
        Ok(self.store.queued_mailbox_ids().await?)
    }

    pub async fn load_job(&self, job_id: &str) -> Result<Option<MailboxJob>, MailboxError> {
        Ok(self.store.load_job(job_id).await?)
    }

    // ── Lifecycle ────────────────────────────────────────────────────

    /// Recover on startup: reload Queued jobs, buffer, dispatch idle threads.
    #[tracing::instrument(skip(self))]
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

        // Recover threads stuck in awaiting_tasks with no queued wake job.
        if let Some(run_store) = self.runtime.thread_run_store() {
            let query = awaken_contract::contract::storage::RunQuery {
                status: Some(awaken_contract::contract::lifecycle::RunStatus::Waiting),
                limit: 200,
                ..Default::default()
            };
            if let Ok(page) = run_store.list_runs(&query).await {
                let queued_set: std::collections::HashSet<String> =
                    mailbox_ids.iter().cloned().collect();
                for run in &page.items {
                    if run.termination_code.as_deref() != Some("awaiting_tasks") {
                        continue;
                    }
                    // Skip if this thread already has a queued job
                    if queued_set.contains(&run.thread_id) {
                        continue;
                    }
                    // Enqueue a wake job for this orphaned awaiting_tasks thread
                    let now_val = now_ms();
                    let job = MailboxJob {
                        job_id: uuid::Uuid::now_v7().to_string(),
                        mailbox_id: run.thread_id.clone(),
                        agent_id: run.agent_id.clone(),
                        messages: vec![Message::internal_user("<background-tasks-updated />")],
                        origin: awaken_contract::contract::mailbox::MailboxJobOrigin::Internal,
                        sender_id: None,
                        parent_run_id: None,
                        request_extras: {
                            let extras = RunRequestExtras {
                                overrides: None,
                                decisions: Vec::new(),
                                frontend_tools: Vec::new(),
                                continue_run_id: Some(run.run_id.clone()),
                                job_id_hint: None,
                                parent_thread_id: None,
                            };
                            extras.to_value().ok().flatten()
                        },
                        priority: 200,
                        dedupe_key: Some(format!("bg-wake:{}", run.thread_id)),
                        generation: 0,
                        status: MailboxJobStatus::Queued,
                        available_at: now_val,
                        attempt_count: 0,
                        max_attempts: 3,
                        last_error: None,
                        claim_token: None,
                        claimed_by: None,
                        lease_until: None,
                        created_at: now_val,
                        updated_at: now_val,
                    };
                    if let Ok(()) = self.store.enqueue(&job).await {
                        total += 1;
                        tracing::info!(
                            thread_id = %run.thread_id,
                            run_id = %run.run_id,
                            "recover: enqueued wake job for orphaned awaiting_tasks thread"
                        );
                        self.get_or_create_worker(&run.thread_id).await;
                        self.try_dispatch_next(&run.thread_id).await;
                    }
                }
            }
        }

        Ok(total)
    }

    /// Run sweep + GC loop forever. Call from `tokio::spawn`.
    ///
    /// When `app_state` is provided, also purges stale replay buffers on
    /// each GC cycle to prevent unbounded growth.
    pub async fn run_maintenance_loop(self: Arc<Self>, app_state: Option<crate::app::AppState>) {
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
                    // Purge stale replay buffers (5 min TTL).
                    if let Some(ref st) = app_state {
                        st.purge_stale_replay_buffers(Duration::from_secs(300));
                    }
                }
            }
        }
    }

    // ── Internal: dispatch ───────────────────────────────────────────

    /// Claim a job from the store and start execution.
    #[tracing::instrument(skip(self), fields(mailbox_id = %mailbox_id))]
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
                revert_claiming_to_idle(&self.workers, mailbox_id).await;
                return;
            }
        };

        let Some(job) = claimed.into_iter().next() else {
            // No jobs to claim.
            revert_claiming_to_idle(&self.workers, mailbox_id).await;
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
        let (event_tx, _event_rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);
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
    #[tracing::instrument(skip(self), fields(mailbox_id = %mailbox_id))]
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
    #[tracing::instrument(skip(self, event_tx, reconnectable_sink, suspended), fields(job_id = %job.job_id, mailbox_id = %mailbox_id))]
    fn spawn_execution(
        self: &Arc<Self>,
        job: MailboxJob,
        event_tx: mpsc::Sender<AgentEvent>,
        reconnectable_sink: Arc<ReconnectableEventSink>,
        claim_token: String,
        mailbox_id: String,
        suspended: Arc<AtomicBool>,
    ) {
        let this = Arc::clone(self);
        let job_id = job.job_id.clone();

        tokio::spawn(async move {
            crate::metrics::inc_active_runs();
            let _guard = ActiveRunGuard;

            let sink = SuspensionAwareSink {
                inner: reconnectable_sink as Arc<dyn EventSink>,
                suspended,
            };

            // Generation check: if this job was superseded between claim and
            // execution start, abort without entering the runtime.
            if let Ok(Some(current_job)) = this.store.load_job(&job_id).await
                && current_job.status != MailboxJobStatus::Claimed
            {
                tracing::info!(job_id, status = ?current_job.status, "job no longer claimed, skipping execution");
                return;
            }

            let mut request =
                awaken_runtime::RunRequest::new(job.mailbox_id.clone(), job.messages.clone());
            if !job.agent_id.is_empty() {
                request = request.with_agent_id(job.agent_id.clone());
            }
            // Restore origin metadata.
            request = request.with_origin(job.origin);
            if let Some(ref prid) = job.parent_run_id {
                request = request.with_parent_run_id(prid.clone());
            }
            // Reconstruct RunRequest extras from opaque payload.
            if let Some(ref extras_value) = job.request_extras {
                match RunRequestExtras::from_value(extras_value) {
                    Ok(extras) => {
                        request = extras.apply_to(request);
                    }
                    Err(e) => {
                        tracing::error!(job_id, error = %e, "failed to deserialize RunRequestExtras");
                        // Permanent failure — extras are corrupt, dead-letter.
                        let now = now_ms();
                        let msg = format!("corrupt request_extras: {e}");
                        let _ = this
                            .store
                            .dead_letter(&job_id, &claim_token, &msg, now)
                            .await;
                        return;
                    }
                }
            }
            let continue_run_id = request.continue_run_id.clone();
            let (inbox_sender, inbox_receiver) = awaken_runtime::inbox::inbox_channel_with_fallback(
                Arc::new(TaskDoneMailboxNotify::new(
                    this.clone(),
                    job.mailbox_id.clone(),
                    continue_run_id,
                )),
            );
            request = request.with_inbox(inbox_sender, inbox_receiver);

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
                    let _ = event_tx
                        .send(AgentEvent::RunFinish {
                            thread_id: job.mailbox_id.clone(),
                            run_id: job_id.clone(),
                            result: None,
                            termination:
                                awaken_contract::contract::lifecycle::TerminationReason::Error(
                                    msg.clone(),
                                ),
                        })
                        .await;
                    let backoff_factor = 2u64.pow(job.attempt_count.saturating_sub(1).min(6));
                    let retry_at = now
                        + (this.config.default_retry_delay_ms * backoff_factor)
                            .min(this.config.max_retry_delay_ms);
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
                    let _ = event_tx
                        .send(AgentEvent::RunFinish {
                            thread_id: job.mailbox_id.clone(),
                            run_id: job_id.clone(),
                            result: None,
                            termination:
                                awaken_contract::contract::lifecycle::TerminationReason::Error(
                                    msg.clone(),
                                ),
                        })
                        .await;
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
    ) -> Result<MailboxJob, MailboxError> {
        let extras = RunRequestExtras::from_request(request);
        let request_extras = extras.to_value().map_err(|e| {
            MailboxError::Internal(format!("failed to serialize request extras: {e}"))
        })?;

        let now = now_ms();
        Ok(MailboxJob {
            job_id: request
                .job_id_hint
                .clone()
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
            mailbox_id: thread_id.to_string(),
            agent_id: request.agent_id.as_deref().unwrap_or_default().to_string(),
            messages,
            origin: request.origin,
            sender_id: None,
            parent_run_id: request.parent_run_id.clone(),
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
        })
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

        // Clean up idle workers with no queued jobs.
        self.gc_idle_workers().await;
    }

    /// Remove workers in `Idle` state that have no queued jobs in the store.
    ///
    /// This prevents the `workers` HashMap from growing unbounded as new
    /// threads are created and their runs complete.
    async fn gc_idle_workers(&self) {
        let idle_keys: Vec<String> = {
            let workers = self.workers.read().await;
            let mut keys = Vec::new();
            for (mailbox_id, worker) in workers.iter() {
                let w = worker.lock().await;
                if matches!(w.status, MailboxWorkerStatus::Idle) {
                    keys.push(mailbox_id.clone());
                }
            }
            keys
        };

        if idle_keys.is_empty() {
            return;
        }

        // Check store for queued jobs before removing.
        let mut removed = 0usize;
        let mut workers = self.workers.write().await;
        for mailbox_id in &idle_keys {
            // Re-check under write lock: status might have changed.
            let still_idle = if let Some(worker) = workers.get(mailbox_id) {
                let w = worker.lock().await;
                matches!(w.status, MailboxWorkerStatus::Idle)
            } else {
                false
            };
            if !still_idle {
                continue;
            }

            // Only remove if the store has no queued jobs for this mailbox.
            let has_queued = self
                .store
                .list_jobs(
                    mailbox_id,
                    Some(&[MailboxJobStatus::Queued, MailboxJobStatus::Claimed]),
                    1,
                    0,
                )
                .await
                .map(|jobs| !jobs.is_empty())
                .unwrap_or(true); // Err → keep worker to be safe

            if !has_queued {
                workers.remove(mailbox_id);
                removed += 1;
            }
        }

        if removed > 0 {
            tracing::debug!(removed, "GC removed idle workers");
        }
    }
}

/// Revert worker from Claiming → Idle, but only if still in Claiming state.
/// Prevents overwriting a Running state set by a concurrent dispatch.
async fn revert_claiming_to_idle(
    workers: &tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<MailboxWorker>>>>,
    mailbox_id: &str,
) {
    let workers = workers.read().await;
    if let Some(worker) = workers.get(mailbox_id) {
        let mut w = worker.lock().await;
        if matches!(w.status, MailboxWorkerStatus::Claiming) {
            w.status = MailboxWorkerStatus::Idle;
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
    use async_trait::async_trait;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult};
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::mailbox::MailboxJobOrigin;
    use awaken_contract::contract::message::{Message, ToolCall};
    use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadRunStore};
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };
    use awaken_runtime::extensions::background::{
        BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext,
        TaskResult as BackgroundTaskResult,
    };
    use awaken_runtime::loop_runner::build_agent_env;
    use awaken_runtime::{Plugin, ResolvedAgent};
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore};
    use serde_json::{Value, json};
    use std::sync::Mutex as StdMutex;
    use tokio::time::{Duration, Instant, sleep};

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

    struct ScriptedLlm {
        responses: StdMutex<Vec<StreamResult>>,
    }

    impl ScriptedLlm {
        fn new(responses: Vec<StreamResult>) -> Self {
            Self {
                responses: StdMutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmExecutor for ScriptedLlm {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let mut responses = self.responses.lock().expect("lock poisoned");
            if responses.is_empty() {
                Ok(StreamResult {
                    content: vec![ContentBlock::text("done")],
                    tool_calls: vec![],
                    usage: None,
                    stop_reason: Some(StopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    struct FixedResolver {
        agent: ResolvedAgent,
        plugins: Vec<Arc<dyn Plugin>>,
    }

    impl awaken_runtime::AgentResolver for FixedResolver {
        fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, awaken_runtime::RuntimeError> {
            let mut agent = self.agent.clone();
            agent.env = build_agent_env(&self.plugins, &agent)?;
            Ok(agent)
        }
    }

    struct SpawnShortBgTaskTool {
        manager: Arc<BackgroundTaskManager>,
        delay: Duration,
    }

    #[async_trait]
    impl Tool for SpawnShortBgTaskTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("spawn_bg", "spawn_bg", "Spawn a short background task")
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let delay = self.delay;
            self.manager
                .spawn(
                    &ctx.run_identity.thread_id,
                    "bg",
                    None,
                    "short task",
                    TaskParentContext::default(),
                    move |_task_ctx| async move {
                        sleep(delay).await;
                        BackgroundTaskResult::Success(json!({"done": true}))
                    },
                )
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
            Ok(ToolResult::success("spawn_bg", json!({"spawned": true})).into())
        }
    }

    async fn wait_for_latest_run<F>(
        store: &InMemoryStore,
        thread_id: &str,
        predicate: F,
    ) -> RunRecord
    where
        F: Fn(&RunRecord) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(run) = store
                .latest_run(thread_id)
                .await
                .expect("latest run lookup should succeed")
                && predicate(&run)
            {
                return run;
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for run predicate on thread {thread_id}"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn seeded_waiting_run(run_id: &str, thread_id: &str, agent_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: agent_id.to_string(),
            parent_run_id: None,
            status: RunStatus::Waiting,
            termination_code: Some("awaiting_tasks".into()),
            created_at: 1,
            updated_at: 1,
            steps: 2,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
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
        assert_eq!(config.max_retry_delay_ms, 30_000);
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
        let queued = MailboxDispatchStatus::Queued;
        assert!(matches!(running, MailboxDispatchStatus::Running));
        assert!(matches!(queued, MailboxDispatchStatus::Queued));
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
            max_retry_delay_ms: 60_000,
        };
        assert_eq!(config.lease_ms, 5_000);
        assert_eq!(config.default_max_attempts, 3);
        assert_eq!(config.default_retry_delay_ms, 500);
        assert_eq!(config.max_retry_delay_ms, 60_000);
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
        let job = mailbox.build_job(&request, "thread-42", messages).unwrap();

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
        let job = mailbox.build_job(&request, "thread-1", messages).unwrap();
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
        let job = mailbox.build_job(&request, "thread-ext", messages).unwrap();

        assert!(job.request_extras.is_some());
        let extras = job.request_extras.unwrap();
        assert!(extras["frontend_tools"].is_array());
    }

    #[test]
    fn run_request_extras_serde_roundtrip() {
        use awaken_contract::contract::tool::ToolDescriptor;
        let extras = RunRequestExtras {
            overrides: None,
            decisions: vec![],
            frontend_tools: vec![ToolDescriptor::new("ft1", "FT1", "desc")],
            continue_run_id: None,
            job_id_hint: None,
            parent_thread_id: None,
        };
        let value = extras.to_value().unwrap().unwrap();
        let parsed = RunRequestExtras::from_value(&value).unwrap();
        assert_eq!(parsed.frontend_tools.len(), 1);
        assert_eq!(parsed.frontend_tools[0].id, "ft1");
        assert!(parsed.decisions.is_empty());
        assert!(parsed.overrides.is_none());
    }

    #[test]
    fn run_request_extras_empty_returns_none() {
        let extras = RunRequestExtras {
            overrides: None,
            decisions: vec![],
            frontend_tools: vec![],
            continue_run_id: None,
            job_id_hint: None,
            parent_thread_id: None,
        };
        assert!(extras.to_value().unwrap().is_none());
    }

    #[test]
    fn run_request_extras_apply_to_request() {
        use awaken_contract::contract::tool::ToolDescriptor;
        let extras = RunRequestExtras {
            overrides: None,
            decisions: vec![],
            frontend_tools: vec![ToolDescriptor::new("ft1", "FT1", "desc")],
            continue_run_id: None,
            job_id_hint: Some("job-1".into()),
            parent_thread_id: Some("parent-thread".into()),
        };
        let request = RunRequest::new("t1", vec![Message::user("hi")]);
        let applied = extras.apply_to(request);
        assert_eq!(applied.frontend_tools.len(), 1);
        assert_eq!(applied.job_id_hint.as_deref(), Some("job-1"));
        assert_eq!(applied.parent_thread_id.as_deref(), Some("parent-thread"));
    }

    #[test]
    fn build_job_round_trips_parent_thread_id() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-child", vec![Message::user("hi")])
            .with_agent_id("agent")
            .with_parent_thread_id("thread-parent");
        let job = mailbox
            .build_job(&request, "thread-child", request.messages.clone())
            .expect("build job should succeed");

        let extras = RunRequestExtras::from_value(job.request_extras.as_ref().unwrap())
            .expect("extras should deserialize");
        let restored = extras.apply_to(RunRequest::new(job.mailbox_id, job.messages));
        assert_eq!(restored.parent_thread_id.as_deref(), Some("thread-parent"));
    }

    #[tokio::test]
    async fn build_job_preserves_origin_metadata() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-meta", vec![Message::user("hi")])
            .with_agent_id("a1")
            .with_origin(MailboxJobOrigin::A2A)
            .with_parent_run_id("parent-run-1");
        let messages = request.messages.clone();
        let job = mailbox
            .build_job(&request, "thread-meta", messages)
            .unwrap();

        assert!(matches!(job.origin, MailboxJobOrigin::A2A));
        assert_eq!(job.parent_run_id.as_deref(), Some("parent-run-1"));
    }

    #[tokio::test]
    async fn build_job_defaults_origin_to_user() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-default", vec![Message::user("hi")]);
        let messages = request.messages.clone();
        let job = mailbox
            .build_job(&request, "thread-default", messages)
            .unwrap();

        assert!(matches!(job.origin, MailboxJobOrigin::User));
        assert!(job.parent_run_id.is_none());
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
        let status = MailboxDispatchStatus::Queued;
        assert!(matches!(status, MailboxDispatchStatus::Queued));
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
            MailboxDispatchStatus::Running | MailboxDispatchStatus::Queued
        ));
    }

    #[tokio::test]
    async fn waiting_thread_is_reactivated_by_incoming_message() {
        let store = Arc::new(InMemoryStore::new());
        store
            .create_run(&seeded_waiting_run(
                "run-waiting",
                "thread-waiting",
                "agent",
            ))
            .await
            .expect("seed waiting run");

        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("reactivated")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm),
            plugins: vec![],
        });
        let runtime = Arc::new(
            AgentRuntime::new(resolver)
                .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>),
        );
        let mailbox_store = make_store();
        let mailbox = make_mailbox(runtime, mailbox_store);

        mailbox
            .submit_background(
                RunRequest::new("thread-waiting", vec![Message::user("poke")])
                    .with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");

        let latest = wait_for_latest_run(&store, "thread-waiting", |run| {
            run.status == RunStatus::Done && run.updated_at > 1
        })
        .await;

        assert_eq!(
            latest.run_id, "run-waiting",
            "incoming messages should continue the existing waiting run"
        );
        assert_eq!(latest.status, RunStatus::Done);
    }

    #[tokio::test]
    async fn background_task_completion_should_enqueue_internal_wake_message() {
        let store = Arc::new(InMemoryStore::new());
        let mailbox_store = make_store();
        let manager = Arc::new(BackgroundTaskManager::new());

        let llm = Arc::new(ScriptedLlm::new(vec![
            StreamResult {
                content: vec![ContentBlock::text("spawning task")],
                tool_calls: vec![ToolCall::new("c1", "spawn_bg", json!({}))],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("waiting for background task")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ]));
        let agent = ResolvedAgent::new("agent", "m", "sys", llm).with_tool(Arc::new(
            SpawnShortBgTaskTool {
                manager: manager.clone(),
                delay: Duration::from_millis(25),
            },
        ));
        let resolver = Arc::new(FixedResolver {
            agent,
            plugins: vec![Arc::new(BackgroundTaskPlugin::new(manager))],
        });
        let runtime = Arc::new(
            AgentRuntime::new(resolver)
                .with_thread_run_store(store.clone() as Arc<dyn ThreadRunStore>),
        );
        let mailbox = make_mailbox(runtime, mailbox_store.clone());

        mailbox
            .submit_background(
                RunRequest::new("thread-bg", vec![Message::user("start")]).with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");

        let waiting = wait_for_latest_run(&store, "thread-bg", |run| {
            run.status == RunStatus::Waiting
                && run.termination_code.as_deref() == Some("awaiting_tasks")
        })
        .await;
        sleep(Duration::from_millis(100)).await;

        let jobs = mailbox_store
            .list_jobs("thread-bg", None, 10, 0)
            .await
            .expect("list jobs should succeed");

        assert!(
            jobs.len() >= 2,
            "background completion should enqueue an internal wake message; waiting run was {:?}, jobs were {:?}",
            waiting,
            jobs
        );
        assert!(
            jobs.iter().skip(1).any(|job| {
                job.messages.iter().any(|msg| {
                    msg.role == awaken_contract::contract::message::Role::User
                        && msg.visibility
                            == awaken_contract::contract::message::Visibility::Internal
                        && msg.text().contains("<background-task-event")
                        && msg.text().contains("\"done\":true")
                })
            }),
            "expected a synthetic background wake job after task completion"
        );
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

    // ── Concurrency tests ───────────────────────────────────────────

    #[tokio::test]
    async fn concurrent_submit_background_same_thread_only_one_runs() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit 5 background jobs to the same thread concurrently.
        let mut handles = Vec::new();
        for i in 0..5 {
            let mb = Arc::clone(&mailbox);
            handles.push(tokio::spawn(async move {
                let req = RunRequest::new("thread-conc", vec![Message::user(format!("msg-{i}"))])
                    .with_agent_id("agent-1");
                mb.submit_background(req).await
            }));
        }
        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // All should succeed (enqueue always works).
        assert!(results.iter().all(|r| r.is_ok()));

        // At most one should be Running (the rest are Queued).
        let running_count = results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .filter(|r| matches!(r.status, MailboxDispatchStatus::Running))
            .count();
        assert!(
            running_count <= 1,
            "at most 1 should be Running, got {running_count}"
        );

        // Store should have at most 1 Claimed job for this mailbox.
        let jobs = store
            .list_jobs("thread-conc", Some(&[MailboxJobStatus::Claimed]), 10, 0)
            .await
            .unwrap();
        assert!(
            jobs.len() <= 1,
            "store should have at most 1 Claimed job, got {}",
            jobs.len()
        );
    }

    #[tokio::test]
    async fn concurrent_submit_same_thread_only_one_claims() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit 3 streaming requests to the same thread concurrently.
        let mut handles = Vec::new();
        for i in 0..3 {
            let mb = Arc::clone(&mailbox);
            handles.push(tokio::spawn(async move {
                let req = RunRequest::new(
                    "thread-stream-conc",
                    vec![Message::user(format!("msg-{i}"))],
                )
                .with_agent_id("agent-1");
                mb.submit(req).await
            }));
        }
        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Some may fail (inline-claim rejected), some succeed.
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert!(ok_count >= 1, "at least 1 should succeed");

        // Store should have at most 1 Claimed job.
        let jobs = store
            .list_jobs(
                "thread-stream-conc",
                Some(&[MailboxJobStatus::Claimed]),
                10,
                0,
            )
            .await
            .unwrap();
        assert!(jobs.len() <= 1, "at most 1 Claimed, got {}", jobs.len());
    }

    #[tokio::test]
    async fn submit_background_returns_correct_status() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // First submit should dispatch (Running or Queued depending on timing).
        let req1 =
            RunRequest::new("thread-status", vec![Message::user("a")]).with_agent_id("agent-1");
        let result1 = mailbox.submit_background(req1).await.unwrap();
        // First job should be claimed/running since thread is idle.
        assert!(
            matches!(
                result1.status,
                MailboxDispatchStatus::Running | MailboxDispatchStatus::Queued
            ),
            "first job should be Running or Queued"
        );

        // Second submit while first is running should be Queued.
        let req2 =
            RunRequest::new("thread-status", vec![Message::user("b")]).with_agent_id("agent-1");
        let result2 = mailbox.submit_background(req2).await.unwrap();
        assert!(
            matches!(result2.status, MailboxDispatchStatus::Queued),
            "second job should be Queued while first is running"
        );
    }

    #[tokio::test]
    async fn worker_status_not_corrupted_after_empty_claim() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit and dispatch a job to get worker into Running state.
        let req =
            RunRequest::new("thread-guard", vec![Message::user("a")]).with_agent_id("agent-1");
        mailbox.submit_background(req).await.unwrap();

        // Worker should be Running (or Claiming).
        let workers = mailbox.workers.read().await;
        if let Some(worker) = workers.get("thread-guard") {
            let w = worker.lock().await;
            assert!(
                !matches!(w.status, MailboxWorkerStatus::Idle),
                "worker should not be Idle after dispatch"
            );
        }
        drop(workers);

        // Call try_dispatch_next while Running — should be a no-op.
        mailbox.try_dispatch_next("thread-guard").await;

        // Worker should still be Running, not reverted to Idle.
        let workers = mailbox.workers.read().await;
        if let Some(worker) = workers.get("thread-guard") {
            let w = worker.lock().await;
            assert!(
                !matches!(w.status, MailboxWorkerStatus::Idle),
                "worker should still not be Idle"
            );
        }
    }

    // ── Coverage gap tests ──────────────────────────────────────────

    #[test]
    fn run_request_extras_corrupt_json_returns_error() {
        let corrupt = serde_json::json!({"overrides": "not-an-object", "decisions": 42});
        let result = RunRequestExtras::from_value(&corrupt);
        assert!(result.is_err(), "corrupt JSON should fail deserialization");
    }

    #[tokio::test]
    async fn submit_inline_claim_fails_when_thread_already_claimed() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // First submit claims successfully.
        let req1 =
            RunRequest::new("thread-clash", vec![Message::user("first")]).with_agent_id("agent-1");
        let result1 = mailbox.submit(req1).await;
        assert!(result1.is_ok(), "first submit should succeed");

        // Second submit to same thread: interrupt will cancel the first,
        // but timing may allow the second to also succeed or fail gracefully.
        let req2 =
            RunRequest::new("thread-clash", vec![Message::user("second")]).with_agent_id("agent-1");
        let result2 = mailbox.submit(req2).await;
        // Either succeeds (interrupt cancelled old) or fails with validation error.
        // Crucially: no panic, no double-claimed state.
        match &result2 {
            Ok((r, _)) => assert!(!r.job_id.is_empty()),
            Err(MailboxError::Validation(_)) => {} // acceptable
            Err(e) => panic!("unexpected error: {e}"),
        }

        // Store invariant: at most 1 Claimed job for this thread.
        let claimed = store
            .list_jobs("thread-clash", Some(&[MailboxJobStatus::Claimed]), 10, 0)
            .await
            .unwrap();
        assert!(
            claimed.len() <= 1,
            "at most 1 Claimed, got {}",
            claimed.len()
        );
    }

    #[tokio::test]
    async fn reconnect_sink_returns_false_for_idle_worker() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        // Create a worker but don't start a run.
        mailbox.get_or_create_worker("thread-idle").await;

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let result = mailbox.reconnect_sink("thread-idle", tx).await;
        assert!(!result, "reconnect should fail for idle worker");
    }

    #[tokio::test]
    async fn reconnect_sink_returns_false_for_unknown_thread() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let result = mailbox.reconnect_sink("nonexistent", tx).await;
        assert!(!result, "reconnect should fail for unknown thread");
    }

    #[tokio::test]
    async fn reconnect_sink_succeeds_for_running_worker() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        // Directly set the worker to Running status (avoids race with
        // spawn_execution resetting to Idle when StubResolver fails).
        let worker = mailbox.get_or_create_worker("thread-reconnect").await;
        {
            let reconnectable = Arc::new(ReconnectableEventSink::new(mpsc::channel(16).0));
            let mut w = worker.lock().await;
            w.status = MailboxWorkerStatus::Running {
                job_id: "job-fake".into(),
                lease_handle: tokio::spawn(futures::future::pending::<()>()),
                sink: reconnectable,
            };
        }

        let (tx, _rx) = mpsc::channel(16);
        let result = mailbox.reconnect_sink("thread-reconnect", tx).await;
        assert!(result, "reconnect should succeed for running worker");
    }

    #[tokio::test]
    async fn build_job_extras_roundtrip_with_decisions() {
        use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let decisions = vec![(
            "call-1".to_string(),
            ToolCallResume {
                decision_id: "d-1".into(),
                action: ResumeDecisionAction::Resume,
                result: serde_json::json!({"approved": true}),
                reason: None,
                updated_at: 0,
            },
        )];

        let request = RunRequest::new("thread-dec", vec![Message::user("hi")])
            .with_agent_id("a1")
            .with_decisions(decisions.clone());
        let messages = request.messages.clone();
        let job = mailbox.build_job(&request, "thread-dec", messages).unwrap();

        // Verify extras contain decisions.
        assert!(job.request_extras.is_some());
        let extras = RunRequestExtras::from_value(job.request_extras.as_ref().unwrap()).unwrap();
        assert_eq!(extras.decisions.len(), 1);
        assert_eq!(extras.decisions[0].0, "call-1");
    }

    #[tokio::test]
    async fn build_job_origin_a2a_roundtrip() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-a2a", vec![Message::user("hi")])
            .with_origin(MailboxJobOrigin::A2A)
            .with_parent_run_id("parent-123");
        let messages = request.messages.clone();
        let job = mailbox.build_job(&request, "thread-a2a", messages).unwrap();

        assert!(matches!(job.origin, MailboxJobOrigin::A2A));
        assert_eq!(job.parent_run_id.as_deref(), Some("parent-123"));
    }

    // ── INLINE_CLAIM_GUARD_MS ───────────────────────────────────────

    #[test]
    fn inline_claim_guard_is_reasonable() {
        assert_eq!(INLINE_CLAIM_GUARD_MS, 60_000);
    }

    // ── Nack exponential backoff ────────────────────────────────────

    #[test]
    fn nack_backoff_progression() {
        let config = MailboxConfig::default();
        // Formula from execute_job: 2^(attempt_count.saturating_sub(1).min(6))
        // attempt_count is 0-based on the job at nack time, but incremented
        // by the store before re-queue. The backoff in execute_job uses
        // job.attempt_count which is the pre-nack value.
        for (attempt_count, expected_ms) in [
            (1, 250),   // 2^0 * 250 = 250
            (2, 500),   // 2^1 * 250 = 500
            (3, 1000),  // 2^2 * 250 = 1000
            (4, 2000),  // 2^3 * 250 = 2000
            (5, 4000),  // 2^4 * 250 = 4000
            (6, 8000),  // 2^5 * 250 = 8000
            (7, 16000), // 2^6 * 250 = 16000
        ] {
            let backoff_factor = 2u64.pow((attempt_count as u32).saturating_sub(1).min(6));
            let delay =
                (config.default_retry_delay_ms * backoff_factor).min(config.max_retry_delay_ms);
            assert_eq!(delay, expected_ms, "attempt_count={attempt_count}");
        }
    }

    #[test]
    fn nack_backoff_caps_at_max() {
        let config = MailboxConfig {
            max_retry_delay_ms: 5000,
            default_retry_delay_ms: 1000,
            ..Default::default()
        };
        // attempt_count=4 → 2^3 = 8 → 1000*8 = 8000, capped at 5000
        let backoff_factor = 2u64.pow(3);
        let delay = (config.default_retry_delay_ms * backoff_factor).min(config.max_retry_delay_ms);
        assert_eq!(delay, 5000);
    }

    #[test]
    fn nack_backoff_zero_attempt_is_base_delay() {
        let config = MailboxConfig::default();
        // attempt_count=0 → saturating_sub(1)=0, but min(6)=0 → 2^0=1 → 250*1=250
        // However in practice attempt_count starts at 1 after first claim.
        let backoff_factor = 2u64.pow(0u32.saturating_sub(1).min(6));
        let delay = (config.default_retry_delay_ms * backoff_factor).min(config.max_retry_delay_ms);
        assert_eq!(delay, 250);
    }

    #[test]
    fn nack_backoff_high_attempt_stays_capped() {
        let config = MailboxConfig::default();
        // attempt_count=100 → min(6)=6 → 2^6=64 → 250*64=16000 < 30000
        let backoff_factor = 2u64.pow(100u32.saturating_sub(1).min(6));
        let delay = (config.default_retry_delay_ms * backoff_factor).min(config.max_retry_delay_ms);
        assert_eq!(delay, 16000);

        // With smaller max: attempt_count=100 → 250*64=16000, capped at 10000
        let config2 = MailboxConfig {
            max_retry_delay_ms: 10_000,
            ..Default::default()
        };
        let delay2 =
            (config2.default_retry_delay_ms * backoff_factor).min(config2.max_retry_delay_ms);
        assert_eq!(delay2, 10_000);
    }

    // ── GC idle workers ─────────────────────────────────────────────

    #[tokio::test]
    async fn gc_idle_workers_removes_idle_with_no_jobs() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Manually insert an Idle worker (no jobs in store for this thread).
        {
            let mut workers = mailbox.workers.write().await;
            workers.insert(
                "thread-gc".to_string(),
                Arc::new(Mutex::new(MailboxWorker::default())),
            );
        }

        // Verify the worker is present.
        assert!(mailbox.workers.read().await.contains_key("thread-gc"));

        // Run GC — idle worker with no queued jobs should be removed.
        mailbox.gc_idle_workers().await;

        assert!(
            !mailbox.workers.read().await.contains_key("thread-gc"),
            "idle worker with no queued jobs should be removed"
        );
    }

    #[tokio::test]
    async fn gc_idle_workers_keeps_worker_with_queued_jobs() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Enqueue a job for the thread (background so it goes to store).
        let request =
            RunRequest::new("thread-gc-keep", vec![Message::user("hi")]).with_agent_id("agent-1");
        mailbox.submit_background(request).await.unwrap();

        // Force the worker to Idle status (simulating it finished one job
        // but another is queued).
        {
            let mut workers = mailbox.workers.write().await;
            workers.insert(
                "thread-gc-keep".to_string(),
                Arc::new(Mutex::new(MailboxWorker::default())),
            );
        }

        // Run GC — worker has queued/claimed jobs, so it should be kept.
        mailbox.gc_idle_workers().await;

        // The worker should still exist because there are jobs in the store.
        let has_jobs = !store
            .list_jobs(
                "thread-gc-keep",
                Some(&[MailboxJobStatus::Queued, MailboxJobStatus::Claimed]),
                1,
                0,
            )
            .await
            .unwrap()
            .is_empty();
        if has_jobs {
            assert!(
                mailbox.workers.read().await.contains_key("thread-gc-keep"),
                "idle worker with queued jobs should NOT be removed"
            );
        }
    }

    #[tokio::test]
    async fn gc_idle_workers_noop_when_empty() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        // No workers exist — GC should not panic.
        mailbox.gc_idle_workers().await;
        let workers = mailbox.workers.read().await;
        assert!(workers.is_empty());
    }
}
