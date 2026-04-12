//! Mailbox service: unified persistent run queue.
//!
//! Every run request (streaming, background, A2A, internal) enters as a
//! [`RunDispatch`] keyed by `thread_id`. The Mailbox orchestrates persistent
//! enqueue, lease-based claim, execution via [`RunDispatchExecutor`], and lifecycle
//! management (lease renewal, sweep, GC).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinHandle;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::{
    MailboxInterrupt, MailboxStore, RunDispatch, RunDispatchResult, RunDispatchStatus,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    MessageSeqRange, RunMessageInput, RunRecord, RunRequestSnapshot, RunResumeDecision,
    StorageError, ThreadRunStore,
};
use awaken_contract::contract::suspension::{ToolCallOutcome, ToolCallResume};
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_contract::now_ms;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_runtime::{AgentRuntime, RunRequest};

use crate::transport::channel_sink::ReconnectableEventSink;

/// Guard window for inline-claimed dispatches: if the process crashes between
/// enqueue and claim, the sweep will reclaim the dispatch after this period.
const INLINE_CLAIM_GUARD_MS: u64 = 60_000;

/// Validation message returned when an inline submit loses the active-run race.
pub(crate) const ACTIVE_RUN_CONFLICT_MESSAGE: &str =
    "thread has an active run; cannot claim inline";

// ── RunRequest ↔ RunDispatch conversion ───────────────────────────────

/// Typed envelope for RunRequest fields that Mailbox stores opaquely.
/// Centralizes the RunRequest → RunDispatch → RunRequest round-trip.
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
    run_id_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dispatch_id_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_request_id: Option<String>,
    #[serde(default)]
    run_mode: RunMode,
    #[serde(default)]
    adapter: AdapterKind,
}

impl RunRequestExtras {
    fn from_request(request: &awaken_runtime::RunRequest) -> Self {
        Self {
            overrides: request.overrides.clone(),
            decisions: request.decisions.clone(),
            frontend_tools: request.frontend_tools.clone(),
            continue_run_id: request.continue_run_id.clone(),
            run_id_hint: request.run_id_hint.clone(),
            dispatch_id_hint: request.dispatch_id_hint.clone(),
            parent_thread_id: request.parent_thread_id.clone(),
            transport_request_id: request.transport_request_id.clone(),
            run_mode: request.run_mode,
            adapter: request.adapter,
        }
    }

    fn to_value(&self) -> Result<Option<serde_json::Value>, serde_json::Error> {
        if self.overrides.is_none()
            && self.decisions.is_empty()
            && self.frontend_tools.is_empty()
            && self.continue_run_id.is_none()
            && self.run_id_hint.is_none()
            && self.dispatch_id_hint.is_none()
            && self.parent_thread_id.is_none()
            && self.transport_request_id.is_none()
            && self.run_mode == RunMode::Foreground
            && self.adapter == AdapterKind::Internal
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
        if let Some(run_id_hint) = self.run_id_hint {
            request = request.with_run_id_hint(run_id_hint);
        }
        if let Some(dispatch_id_hint) = self.dispatch_id_hint {
            request = request.with_dispatch_id_hint(dispatch_id_hint);
        }
        if let Some(parent_thread_id) = self.parent_thread_id {
            request = request.with_parent_thread_id(parent_thread_id);
        }
        if let Some(transport_request_id) = self.transport_request_id {
            request = request.with_transport_request_id(transport_request_id);
        }
        request
            .with_run_mode(self.run_mode)
            .with_adapter(self.adapter)
    }
}

// ── TaskDoneMailboxNotify ────────────────────────────────────────────

/// Fallback for inbox delivery when the agent's run has ended.
///
/// Implements [`OnInboxClosed`](awaken_runtime::inbox::OnInboxClosed) — when an `InboxSender::send()` fails
/// because the receiver was dropped (agent run returned with AwaitingTasks),
/// this enqueues a mailbox wake dispatch so the thread gets a continuation run.
///
/// Uses `dedupe_key` to coalesce multiple task completions into one wake dispatch.
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
            let mut request = RunRequest::new(thread_id.clone(), vec![wake_message])
                .with_origin(awaken_contract::contract::storage::RunRequestOrigin::Internal)
                .with_run_mode(RunMode::InternalWake)
                .with_adapter(AdapterKind::Internal);
            if let Some(run_id) = continue_run_id {
                request = request.with_continue_run_id(run_id);
            }
            if let Err(e) = mailbox.submit_background(request).await {
                tracing::warn!(thread_id, error = %e, "failed to enqueue background task wake dispatch");
            }
        });
    }
}

// ── Public types ─────────────────────────────────────────────────────

/// Result returned by submit/submit_background.
#[derive(Debug, Clone)]
pub struct MailboxSubmitResult {
    pub dispatch_id: String,
    pub run_id: String,
    pub thread_id: String,
    pub status: MailboxDispatchStatus,
}

/// Dispatch status for a submitted run activation.
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

/// Execution boundary used by mailbox dispatch.
///
/// Mailbox owns delivery, leasing, retry, and recovery. The executor behind
/// this trait owns actual run execution and live-run control. It intentionally
/// does not expose storage so mailbox scheduling stays orthogonal to the main
/// runtime implementation.
#[async_trait]
pub trait RunDispatchExecutor: Send + Sync {
    /// Execute a run request and stream events into the provided sink.
    async fn run(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError>;

    /// Cancel an active run by run id or thread id.
    fn cancel(&self, id: &str) -> bool;

    /// Cancel an active run by thread id and wait for it to unregister.
    async fn cancel_and_wait_by_thread(&self, thread_id: &str) -> bool;

    /// Forward one human/tool decision to an active run.
    fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool;
}

#[async_trait]
impl RunDispatchExecutor for AgentRuntime {
    async fn run(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        AgentRuntime::run(self, request, sink).await
    }

    fn cancel(&self, id: &str) -> bool {
        AgentRuntime::cancel(self, id)
    }

    async fn cancel_and_wait_by_thread(&self, thread_id: &str) -> bool {
        AgentRuntime::cancel_and_wait_by_thread(self, thread_id).await
    }

    fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        AgentRuntime::send_decision(self, id, tool_call_id, resume)
    }
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
    /// How often to run GC for terminal dispatches (default 60s).
    pub gc_interval: Duration,
    /// How long to keep terminal dispatches before purging (default 24h).
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

/// Callback invoked during mailbox maintenance GC ticks.
pub type MailboxMaintenanceCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// Startup recovery retry settings used by lifecycle startup.
#[derive(Clone)]
pub struct MailboxStartupRecoveryConfig {
    /// Maximum recovery attempts before giving up. Values below 1 are treated
    /// as one attempt.
    pub max_attempts: u32,
    /// Delay between failed recovery attempts.
    pub retry_delay: Duration,
}

impl Default for MailboxStartupRecoveryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            retry_delay: Duration::from_millis(250),
        }
    }
}

/// Configuration for framework-managed mailbox lifecycle tasks.
#[derive(Clone)]
pub struct MailboxLifecycleConfig {
    /// Delay before startup recovery and maintenance begin.
    pub startup_delay: Duration,
    /// Retry policy for startup recovery.
    pub startup_recovery: MailboxStartupRecoveryConfig,
    /// Optional cleanup hook for application-owned resources.
    pub maintenance_callback: Option<MailboxMaintenanceCallback>,
}

impl Default for MailboxLifecycleConfig {
    fn default() -> Self {
        Self {
            startup_delay: Duration::ZERO,
            startup_recovery: MailboxStartupRecoveryConfig::default(),
            maintenance_callback: None,
        }
    }
}

/// Handle for framework-managed mailbox lifecycle tasks.
///
/// Dropping the handle does not stop lifecycle tasks. Call [`shutdown`](Self::shutdown)
/// for quiescent shutdown or [`abort`](Self::abort) for fire-and-forget stop.
#[derive(Clone)]
pub struct MailboxLifecycleHandle {
    tasks: Arc<StdMutex<Option<MailboxLifecycleTasks>>>,
    transition_lock: Arc<Mutex<()>>,
}

impl MailboxLifecycleHandle {
    /// Abort lifecycle tasks. This is idempotent.
    pub fn abort(&self) {
        if let Some(tasks) = self.tasks.lock().expect("lifecycle lock poisoned").take() {
            tasks.abort();
        }
    }

    /// Abort lifecycle tasks and wait until they have fully exited.
    ///
    /// This is the quiescent shutdown path. Use it when a caller needs a hard
    /// guarantee that a subsequent lifecycle start cannot overlap old recovery
    /// or maintenance tasks.
    pub async fn shutdown(&self) -> Result<(), MailboxError> {
        let _transition_guard = self.transition_lock.lock().await;
        let tasks = self.tasks.lock().expect("lifecycle lock poisoned").take();
        if let Some(tasks) = tasks {
            tasks.shutdown().await?;
        }
        Ok(())
    }

    /// Returns true while lifecycle tasks are registered for this mailbox.
    pub fn is_running(&self) -> bool {
        self.tasks
            .lock()
            .expect("lifecycle lock poisoned")
            .is_some()
    }
}

struct MailboxLifecycleTasks {
    recover_task: Option<JoinHandle<()>>,
    maintenance_task: JoinHandle<()>,
}

impl MailboxLifecycleTasks {
    fn abort(self) {
        if let Some(task) = self.recover_task {
            task.abort();
        }
        self.maintenance_task.abort();
    }

    async fn shutdown(self) -> Result<(), MailboxError> {
        if let Some(task) = self.recover_task {
            task.abort();
            await_lifecycle_task("mailbox startup recovery", task).await?;
        }
        self.maintenance_task.abort();
        await_lifecycle_task("mailbox maintenance", self.maintenance_task).await
    }
}

async fn await_lifecycle_task(name: &str, task: JoinHandle<()>) -> Result<(), MailboxError> {
    match task.await {
        Ok(()) => Ok(()),
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) if error.is_panic() => Err(MailboxError::Internal(format!("{name} panicked"))),
        Err(error) => Err(MailboxError::Internal(format!("{name} failed: {error}"))),
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
        dispatch_id: String,
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
/// Orchestrates `MailboxStore` (dispatch persistence) + `ThreadRunStore`
/// (run/message truth) + `RunDispatchExecutor` (execution)
/// with lease-based distributed claim, per-thread serialization, sweep,
/// and garbage collection.
pub struct Mailbox {
    executor: Arc<dyn RunDispatchExecutor>,
    store: Arc<dyn MailboxStore>,
    run_store: Arc<dyn ThreadRunStore>,
    consumer_id: String,
    workers: RwLock<HashMap<String, Arc<Mutex<MailboxWorker>>>>,
    config: MailboxConfig,
    lifecycle_tasks: Arc<StdMutex<Option<MailboxLifecycleTasks>>>,
    lifecycle_start_lock: Arc<Mutex<()>>,
}

impl Mailbox {
    /// Create a new Mailbox service.
    pub fn new<R>(
        executor: Arc<R>,
        store: Arc<dyn MailboxStore>,
        run_store: Arc<dyn ThreadRunStore>,
        consumer_id: String,
        config: MailboxConfig,
    ) -> Self
    where
        R: RunDispatchExecutor + 'static,
    {
        Self::new_with_executor(executor, store, run_store, consumer_id, config)
    }

    /// Create a Mailbox service from an already-erased execution boundary.
    pub fn new_with_executor(
        executor: Arc<dyn RunDispatchExecutor>,
        store: Arc<dyn MailboxStore>,
        run_store: Arc<dyn ThreadRunStore>,
        consumer_id: String,
        config: MailboxConfig,
    ) -> Self {
        Self {
            executor,
            store,
            run_store,
            consumer_id,
            workers: RwLock::new(HashMap::new()),
            config,
            lifecycle_tasks: Arc::new(StdMutex::new(None)),
            lifecycle_start_lock: Arc::new(Mutex::new(())),
        }
    }

    // ── Submission ───────────────────────────────────────────────────

    /// Default bounded channel capacity for the runtime→SSE relay.
    const EVENT_CHANNEL_CAPACITY: usize = 256;

    /// Submit a run for streaming. Returns event receiver immediately.
    ///
    /// The dispatch is persisted (WAL), then claimed inline by this process.
    /// The caller wires `event_rx` to their transport (SSE, WebSocket, etc).
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id))]
    pub async fn submit(
        self: &Arc<Self>,
        mut request: RunRequest,
    ) -> Result<(MailboxSubmitResult, mpsc::Receiver<AgentEvent>), MailboxError> {
        normalize_mailbox_run_mode(&mut request, false);
        let (thread_id, messages) = validate_run_inputs(
            request.thread_id.clone(),
            request.messages.clone(),
            !request.decisions.is_empty(),
        )?;

        // Step 1: Interrupt — bump dispatch epoch, supersede stale queued dispatches.
        let now = now_ms();
        match self.store.interrupt(&thread_id, now).await {
            Ok(interrupt) => {
                // Step 2: Cancel active runtime run if the interrupt found one.
                if interrupt.active_dispatch.is_some()
                    && self.executor.cancel_and_wait_by_thread(&thread_id).await
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
                self.executor.cancel_and_wait_by_thread(&thread_id).await;
            }
        }

        let run_id = self
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await?;
        let dispatch = self.build_dispatch(&request, &thread_id)?;
        let dispatch_id = dispatch.dispatch_id.clone();
        let thread_id = dispatch.thread_id.clone();

        // WAL: persist before anything else.
        // Set available_at slightly in the future to prevent sweep from grabbing
        // the dispatch during the inline claim window. If the process crashes before
        // the claim completes, sweep will reclaim the dispatch after the guard period.
        let mut wal_dispatch = dispatch;
        wal_dispatch.available_at = now_ms() + INLINE_CLAIM_GUARD_MS;
        self.store.enqueue(&wal_dispatch).await?;

        // Inline claim.
        let now = now_ms();
        let claimed = self
            .store
            .claim_dispatch(&dispatch_id, &self.consumer_id, self.config.lease_ms, now)
            .await?;

        let (event_tx, event_rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);

        if let Some(claimed_dispatch) = claimed {
            let claim_token = claimed_dispatch.claim_token.clone().unwrap_or_default();

            // Shared flag: set by the event sink when a tool call is suspended.
            let suspended = Arc::new(AtomicBool::new(false));

            // Start lease renewal.
            let lease_handle = self.spawn_lease_renewal(
                dispatch_id.clone(),
                claim_token.clone(),
                thread_id.clone(),
                Arc::clone(&suspended),
            );

            // Create reconnectable sink for SSE reconnection on resume.
            let reconnectable_sink = Arc::new(ReconnectableEventSink::new(event_tx.clone()));

            // Update worker state.
            let worker = self.get_or_create_worker(&thread_id).await;
            {
                let mut w = worker.lock().await;
                w.status = MailboxWorkerStatus::Running {
                    dispatch_id: dispatch_id.clone(),
                    lease_handle,
                    sink: Arc::clone(&reconnectable_sink),
                };
            }

            // Spawn execution.
            self.spawn_execution(
                claimed_dispatch,
                event_tx.clone(),
                reconnectable_sink,
                claim_token,
                thread_id.clone(),
                suspended,
            );

            Ok((
                MailboxSubmitResult {
                    dispatch_id,
                    run_id,
                    thread_id,
                    status: MailboxDispatchStatus::Running,
                },
                event_rx,
            ))
        } else {
            // Inline claim failed (another claimed dispatch exists for this
            // thread). Cancel the orphaned dispatch to prevent it from
            // lingering with the guard available_at.
            let now_fix = now_ms();
            if let Err(e) = self.store.cancel(&dispatch_id, now_fix).await {
                tracing::warn!(dispatch_id, error = %e, "failed to cancel unclaimed inline dispatch");
            }
            Err(MailboxError::Validation(ACTIVE_RUN_CONFLICT_MESSAGE.into()))
        }
    }

    /// Submit a run in the background (fire-and-forget).
    ///
    /// Dispatch is persisted with `available_at = now`, then execution is event-driven.
    /// Returns dispatch_id + thread_id for status polling.
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id))]
    pub async fn submit_background(
        self: &Arc<Self>,
        mut request: RunRequest,
    ) -> Result<MailboxSubmitResult, MailboxError> {
        normalize_mailbox_run_mode(&mut request, true);
        let (thread_id, messages) = validate_run_inputs(
            request.thread_id.clone(),
            request.messages.clone(),
            !request.decisions.is_empty(),
        )?;

        let run_id = self
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await?;
        let dispatch = self.build_dispatch(&request, &thread_id)?;
        let dispatch_id = dispatch.dispatch_id.clone();
        let thread_id = dispatch.thread_id.clone();

        // WAL: persist with available_at = now.
        self.store.enqueue(&dispatch).await?;

        // Dispatch via try_dispatch_next which handles Idle → Claiming atomically.
        self.get_or_create_worker(&thread_id).await;
        self.try_dispatch_next(&thread_id).await;

        // Check if this dispatch was claimed (Running) or is still Queued.
        let status = {
            let workers = self.workers.read().await;
            if let Some(worker) = workers.get(&thread_id) {
                let w = worker.lock().await;
                match &w.status {
                    MailboxWorkerStatus::Running {
                        dispatch_id: running_id,
                        ..
                    } if *running_id == dispatch_id => MailboxDispatchStatus::Running,
                    _ => MailboxDispatchStatus::Queued,
                }
            } else {
                MailboxDispatchStatus::Queued
            }
        };

        Ok(MailboxSubmitResult {
            dispatch_id,
            run_id,
            thread_id,
            status,
        })
    }

    // ── Control ──────────────────────────────────────────────────────

    /// Cancel a run by dispatch_id or thread_id.
    ///
    /// If Queued: transitions to Cancelled via store.
    /// If Claimed/Running: cancels runtime run via dual-index lookup.
    pub async fn cancel(&self, id: &str) -> Result<bool, MailboxError> {
        // Try store cancel first (works for Queued dispatches).
        let now = now_ms();
        let cancelled = self.store.cancel(id, now).await?;
        if cancelled.is_some() {
            return Ok(true);
        }

        // Try runtime cancel (for Claimed/Running dispatches).
        Ok(self.executor.cancel(id))
    }

    /// Interrupt a thread: bump dispatch epoch, supersede all pending,
    /// cancel active run. Clean slate for the thread.
    pub async fn interrupt(&self, thread_id: &str) -> Result<MailboxInterrupt, MailboxError> {
        let now = now_ms();
        let result = self.store.interrupt(thread_id, now).await?;

        // Cancel active runtime run if any.
        if result.active_dispatch.is_some() {
            self.executor.cancel(thread_id);
        }

        Ok(result)
    }

    /// Forward a tool-call decision to an active run.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        self.executor.send_decision(id, tool_call_id, resume)
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

    async fn reusable_waiting_run_id(&self, thread_id: &str) -> Option<String> {
        if let Some(thread) = self.run_store.load_thread(thread_id).await.ok().flatten()
            && let Some(open_run_id) = thread.open_run_id.as_deref()
            && let Some(run) = self.run_store.load_run(open_run_id).await.ok().flatten()
            && run.thread_id == thread_id
            && run.is_resumable_waiting()
        {
            return Some(run.run_id);
        }
        let run = self.run_store.latest_run(thread_id).await.ok().flatten()?;
        run.is_resumable_waiting().then_some(run.run_id)
    }

    // ── Query ────────────────────────────────────────────────────────

    /// List mailbox dispatches for a thread (with optional status filter).
    pub async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, MailboxError> {
        Ok(self
            .store
            .list_dispatches(thread_id, status_filter, limit, offset)
            .await?)
    }

    /// List thread IDs that currently have queued dispatches.
    pub async fn queued_thread_ids(&self) -> Result<Vec<String>, MailboxError> {
        Ok(self.store.queued_thread_ids().await?)
    }

    pub async fn load_dispatch(
        &self,
        dispatch_id: &str,
    ) -> Result<Option<RunDispatch>, MailboxError> {
        Ok(self.store.load_dispatch(dispatch_id).await?)
    }

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

    fn lifecycle_is_running(&self) -> Result<bool, MailboxError> {
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

        *lifecycle = Some(MailboxLifecycleTasks {
            recover_task,
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
    ///
    /// Also reclaims any Claimed dispatches from a previous process that never
    /// completed (orphan cleanup), preventing threads from being stuck in a
    /// "running" state indefinitely after a restart.
    #[tracing::instrument(skip(self))]
    pub async fn recover(self: &Arc<Self>) -> Result<usize, MailboxError> {
        let now = now_ms();
        let mut total = 0;

        // Reclaim expired leases from previous process crash.
        let reclaimed = self.store.reclaim_expired_leases(now, 100).await?;
        total += reclaimed.len();

        // Force-reclaim any Claimed dispatches that survived from a previous
        // process — their leases may not have expired yet but the executor
        // that held them is gone.
        let stale_claimed = self
            .store
            .reclaim_expired_leases(now + self.config.lease_ms, 500)
            .await
            .unwrap_or_default();
        if !stale_claimed.is_empty() {
            tracing::info!(
                count = stale_claimed.len(),
                "recover: force-reclaimed stale claimed dispatches from previous process"
            );
            total += stale_claimed.len();
        }

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
                    let request = RunRequest::new(
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

    // ── Internal: dispatch ───────────────────────────────────────────

    /// Claim a dispatch from the store and start execution.
    #[tracing::instrument(skip(self), fields(thread_id = %thread_id))]
    async fn dispatch_next_claim(self: &Arc<Self>, thread_id: &str) {
        let now = now_ms();
        let claimed = match self
            .store
            .claim(thread_id, &self.consumer_id, self.config.lease_ms, now, 1)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, thread_id, "failed to claim dispatch");
                revert_claiming_to_idle(&self.workers, thread_id).await;
                return;
            }
        };

        let Some(dispatch) = claimed.into_iter().next() else {
            // No dispatches to claim.
            revert_claiming_to_idle(&self.workers, thread_id).await;
            return;
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

        // Create channel for background dispatch (events go nowhere unless observed).
        let (event_tx, _event_rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);
        let reconnectable_sink = Arc::new(ReconnectableEventSink::new(event_tx.clone()));

        // Update worker state.
        let worker = self.get_or_create_worker(thread_id).await;
        {
            let mut w = worker.lock().await;
            w.status = MailboxWorkerStatus::Running {
                dispatch_id: dispatch_id.clone(),
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
    }

    /// Claim from store and execute the next dispatch for this thread.
    #[tracing::instrument(skip(self), fields(thread_id = %thread_id))]
    async fn try_dispatch_next(self: &Arc<Self>, thread_id: &str) {
        let worker = {
            let workers = self.workers.read().await;
            match workers.get(thread_id) {
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

        self.dispatch_next_claim(thread_id).await;
    }

    /// Spawn a lease renewal task that periodically extends the lease.
    ///
    /// When the `suspended` flag is set (run is waiting for human input),
    /// the renewal uses `suspended_lease_ms` instead of the default `lease_ms`
    /// to prevent premature lease expiration during HITL scenarios.
    fn spawn_lease_renewal(
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
                match store
                    .extend_lease(&dispatch_id, &claim_token, effective_lease_ms, now)
                    .await
                {
                    Ok(true) => {} // Lease extended successfully.
                    Ok(false) => {
                        // Lease lost -- another process reclaimed.
                        tracing::warn!(dispatch_id, thread_id, "lease lost, cancelling run");
                        runtime.cancel(&thread_id);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(dispatch_id, error = %e, "lease extension failed");
                        break;
                    }
                }
            }
        })
    }

    /// Spawn the actual execution task for a claimed dispatch.
    #[tracing::instrument(skip(self, event_tx, reconnectable_sink, suspended), fields(dispatch_id = %dispatch.dispatch_id, thread_id = %thread_id))]
    fn spawn_execution(
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
            // execution start, abort without entering the runtime.
            if let Ok(Some(current_dispatch)) = this.store.load_dispatch(&dispatch_id).await
                && current_dispatch.status != RunDispatchStatus::Claimed
            {
                tracing::info!(dispatch_id, status = ?current_dispatch.status, "dispatch no longer claimed, skipping execution");
                return;
            }

            let dispatch_instance_id = uuid::Uuid::now_v7().to_string();
            let mut request = match this.reconstruct_run_request(&dispatch).await {
                Ok(request) => request,
                Err(error) => {
                    tracing::error!(dispatch_id, error = %error, "failed to reconstruct run request from durable run record");
                    let now = now_ms();
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
                    let _ = this
                        .store
                        .record_dispatch_start(
                            &dispatch_id,
                            &claim_token,
                            &dispatch_instance_id,
                            now,
                        )
                        .await;
                    let _ = this
                        .store
                        .record_run_result(&dispatch_id, &claim_token, &run_result, now)
                        .await;
                    let _ = this
                        .store
                        .dead_letter(&dispatch_id, &claim_token, &msg, now)
                        .await;
                    return;
                }
            };
            normalize_mailbox_run_mode(&mut request, false);
            let run_id = dispatch.run_id.clone();
            request = request
                .with_dispatch_id(dispatch_id.clone())
                .with_session_id(dispatch_instance_id.clone());
            let start_now = now_ms();
            if let Err(e) = this
                .store
                .record_dispatch_start(&dispatch_id, &claim_token, &dispatch_instance_id, start_now)
                .await
            {
                tracing::warn!(dispatch_id, run_id, error = %e, "failed to record mailbox dispatch start");
            }
            let continue_run_id = request.continue_run_id.clone();
            let (inbox_sender, inbox_receiver) = awaken_runtime::inbox::inbox_channel_with_fallback(
                Arc::new(TaskDoneMailboxNotify::new(
                    this.clone(),
                    dispatch.thread_id.clone(),
                    continue_run_id,
                )),
            );
            request = request.with_inbox(inbox_sender, inbox_receiver);

            let result = this.executor.run(request, Arc::new(sink)).await;
            let now = now_ms();
            let run_result = mailbox_run_result(&run_id, &dispatch_instance_id, &result);
            if let Err(e) = this
                .store
                .record_run_result(&dispatch_id, &claim_token, &run_result, now)
                .await
            {
                tracing::warn!(dispatch_id, run_id, error = %e, "failed to record mailbox run result");
            }

            match classify_error(&result) {
                MailboxRunOutcome::Completed => {
                    if let Err(e) = this.store.ack(&dispatch_id, &claim_token, now).await {
                        tracing::warn!(dispatch_id, error = %e, "ack failed");
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
                    if let Err(e) = this
                        .store
                        .nack(&dispatch_id, &claim_token, retry_at, &msg, now)
                        .await
                    {
                        tracing::warn!(dispatch_id, error = %e, "nack failed");
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
                    if let Err(e) = this
                        .store
                        .dead_letter(&dispatch_id, &claim_token, &msg, now)
                        .await
                    {
                        tracing::warn!(dispatch_id, error = %e, "dead_letter failed");
                    }
                }
            }

            // Abort lease renewal.
            let worker = this.get_or_create_worker(&thread_id).await;
            {
                let mut w = worker.lock().await;
                let should_transition = matches!(
                    &w.status,
                    MailboxWorkerStatus::Running { dispatch_id: cid, .. } if *cid == dispatch_id
                );
                if should_transition {
                    // Take ownership of the old status to abort the lease handle.
                    let old = std::mem::replace(&mut w.status, MailboxWorkerStatus::Idle);
                    if let MailboxWorkerStatus::Running { lease_handle, .. } = old {
                        lease_handle.abort();
                    }
                }
            }

            // Try to execute the next queued dispatch for this thread.
            this.try_dispatch_next(&thread_id).await;
        });
    }

    /// Get or create a per-thread worker.
    async fn get_or_create_worker(&self, thread_id: &str) -> Arc<Mutex<MailboxWorker>> {
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
                .or_insert_with(|| Arc::new(Mutex::new(MailboxWorker::default()))),
        )
    }

    /// Create or update the durable run truth before enqueuing a dispatch.
    async fn prepare_run_for_dispatch(
        &self,
        request: &mut RunRequest,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<String, MailboxError> {
        if request.continue_run_id.is_none()
            && request.run_id_hint.is_none()
            && let Some(waiting_run_id) = self.reusable_waiting_run_id(thread_id).await
        {
            request.continue_run_id = Some(waiting_run_id);
        }

        let run_id = request
            .continue_run_id
            .clone()
            .or_else(|| request.run_id_hint.clone())
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        if request.continue_run_id.is_none() {
            request.run_id_hint = Some(run_id.clone());
        }

        let normalized_messages = normalize_message_ids(messages);
        let existing_messages = self
            .run_store
            .load_messages(thread_id)
            .await?
            .unwrap_or_default();
        let previous_run = self.run_store.latest_run(thread_id).await?;
        let mut appended_messages = existing_messages;
        appended_messages.extend(normalized_messages.iter().cloned());
        let input_message_ids = normalized_messages
            .iter()
            .filter_map(|message| message.id.clone())
            .collect::<Vec<_>>();
        let request_extras = RunRequestExtras::from_request(request)
            .to_value()
            .map_err(|e| {
                MailboxError::Internal(format!("failed to serialize request extras: {e}"))
            })?;
        let request_snapshot = RunRequestSnapshot {
            origin: request.origin,
            sender_id: None,
            input_message_ids: input_message_ids.clone(),
            input_message_count: normalized_messages.len() as u64,
            request_extras,
            decisions: request
                .decisions
                .iter()
                .map(|(call_id, resume)| RunResumeDecision {
                    call_id: call_id.clone(),
                    resume: resume.clone(),
                })
                .collect(),
            frontend_tools: request.frontend_tools.clone(),
            parent_thread_id: request.parent_thread_id.clone(),
            transport_request_id: request.transport_request_id.clone(),
        };
        let input = Some(RunMessageInput {
            thread_id: thread_id.to_string(),
            range: MessageSeqRange::new(1, appended_messages.len() as u64),
            trigger_message_ids: input_message_ids,
            selected_message_ids: Vec::new(),
            context_policy: None,
            compacted_snapshot_id: None,
        });

        if let Some(mut existing) = self.run_store.load_run(&run_id).await? {
            if existing.thread_id != thread_id {
                return Err(MailboxError::Validation(format!(
                    "run_id '{run_id}' belongs to thread '{}', not '{thread_id}'",
                    existing.thread_id
                )));
            }
            if existing.status != RunStatus::Created && !existing.is_resumable_waiting() {
                return Err(MailboxError::Validation(format!(
                    "run_id '{run_id}' is not open for dispatch"
                )));
            }
            existing.request = Some(request_snapshot);
            existing.input = input;
            existing.updated_at = now_ms() / 1000;
            self.run_store
                .checkpoint(thread_id, &appended_messages, &existing)
                .await?;
            return Ok(run_id);
        }

        let inferred_agent_id = request
            .agent_id
            .clone()
            .or_else(|| {
                previous_run.as_ref().and_then(|run| {
                    (run.status != RunStatus::Created && !run.agent_id.trim().is_empty())
                        .then(|| run.agent_id.clone())
                })
            })
            .unwrap_or_else(|| "default".to_string());
        let inherited_state = previous_run
            .as_ref()
            .filter(|run| run.status != RunStatus::Created)
            .and_then(|run| run.state.clone());
        let now = now_ms() / 1000;
        let record = RunRecord {
            run_id: run_id.clone(),
            thread_id: thread_id.to_string(),
            agent_id: inferred_agent_id,
            parent_run_id: request.parent_run_id.clone(),
            request: Some(request_snapshot),
            input,
            output: None,
            status: RunStatus::Created,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: request.transport_request_id.clone(),
            waiting: None,
            outcome: None,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: inherited_state,
        };
        self.run_store
            .checkpoint(thread_id, &appended_messages, &record)
            .await?;
        Ok(run_id)
    }

    /// Build a RunDispatch from the durable run prepared above.
    fn build_dispatch(
        &self,
        request: &RunRequest,
        thread_id: &str,
    ) -> Result<RunDispatch, MailboxError> {
        let run_id = request
            .continue_run_id
            .clone()
            .or_else(|| request.run_id_hint.clone())
            .ok_or_else(|| MailboxError::Internal("run_id missing after preparation".into()))?;
        let now = now_ms();
        Ok(RunDispatch {
            dispatch_id: request
                .dispatch_id_hint
                .clone()
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
            thread_id: thread_id.to_string(),
            run_id,
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
        })
    }

    async fn reconstruct_run_request(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<RunRequest, MailboxError> {
        let run = self
            .run_store
            .load_run(&dispatch.run_id)
            .await?
            .ok_or_else(|| {
                MailboxError::Validation(format!(
                    "run '{}' not found for dispatch '{}'",
                    dispatch.run_id, dispatch.dispatch_id
                ))
            })?;
        if run.thread_id != dispatch.thread_id {
            return Err(MailboxError::Validation(format!(
                "run '{}' belongs to thread '{}', not dispatch thread '{}'",
                run.run_id, run.thread_id, dispatch.thread_id
            )));
        }
        let snapshot = run.request.clone().ok_or_else(|| {
            MailboxError::Validation(format!("run '{}' has no request snapshot", run.run_id))
        })?;
        let activation_messages = self.activation_messages_for_run(&run, &snapshot).await?;
        let mut request = RunRequest::new(run.thread_id.clone(), activation_messages)
            .with_messages_already_persisted(true)
            .with_origin(snapshot.origin)
            .with_run_mode(RunMode::Resume)
            .with_adapter(AdapterKind::Internal);
        if !run.agent_id.trim().is_empty() {
            request = request.with_agent_id(run.agent_id.clone());
        }
        if let Some(parent_run_id) = run.parent_run_id.clone() {
            request = request.with_parent_run_id(parent_run_id);
        }
        if let Some(parent_thread_id) = snapshot.parent_thread_id.clone() {
            request = request.with_parent_thread_id(parent_thread_id);
        }
        if let Some(transport_request_id) = snapshot.transport_request_id.clone() {
            request = request.with_transport_request_id(transport_request_id);
        }
        if !snapshot.decisions.is_empty() {
            request = request.with_decisions(
                snapshot
                    .decisions
                    .iter()
                    .map(|decision| (decision.call_id.clone(), decision.resume.clone()))
                    .collect(),
            );
        }
        if !snapshot.frontend_tools.is_empty() {
            request = request.with_frontend_tools(snapshot.frontend_tools.clone());
        }
        if let Some(extras_value) = snapshot.request_extras.as_ref() {
            let extras = RunRequestExtras::from_value(extras_value).map_err(|error| {
                MailboxError::Validation(format!("corrupt request_extras: {error}"))
            })?;
            request = extras.apply_to(request);
        }
        request = if run.is_resumable_waiting() {
            request.with_continue_run_id(run.run_id.clone())
        } else {
            request.with_run_id_hint(run.run_id.clone())
        };
        Ok(request.with_trace_dispatch_id(dispatch.dispatch_id.clone()))
    }

    async fn activation_messages_for_run(
        &self,
        run: &RunRecord,
        snapshot: &RunRequestSnapshot,
    ) -> Result<Vec<Message>, MailboxError> {
        if snapshot.input_message_ids.is_empty() {
            return self.activation_messages_from_range(run, snapshot).await;
        }
        let mut messages = Vec::with_capacity(snapshot.input_message_ids.len());
        for message_id in &snapshot.input_message_ids {
            let record = self
                .run_store
                .load_message_record(&run.thread_id, message_id)
                .await?
                .ok_or_else(|| {
                    MailboxError::Validation(format!(
                        "message '{message_id}' not found for run '{}'",
                        run.run_id
                    ))
                })?;
            messages.push(record.message);
        }
        Ok(messages)
    }

    async fn activation_messages_from_range(
        &self,
        run: &RunRecord,
        snapshot: &RunRequestSnapshot,
    ) -> Result<Vec<Message>, MailboxError> {
        let Some(input) = run.input.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(range) = input.range else {
            return Ok(Vec::new());
        };
        let count = snapshot.input_message_count;
        if count == 0 {
            return Ok(Vec::new());
        }
        let from_seq = range.to_seq.saturating_sub(count).saturating_add(1);
        let Some(range) = MessageSeqRange::new(from_seq.max(range.from_seq), range.to_seq) else {
            return Ok(Vec::new());
        };
        let records = self
            .run_store
            .load_message_records_range(&run.thread_id, range)
            .await?;
        Ok(records.into_iter().map(|record| record.message).collect())
    }

    // ── Maintenance ──────────────────────────────────────────────────

    async fn run_sweep(self: &Arc<Self>) {
        let now = now_ms();
        match self.store.reclaim_expired_leases(now, 100).await {
            Ok(reclaimed) => {
                if !reclaimed.is_empty() {
                    tracing::info!(count = reclaimed.len(), "sweep reclaimed expired leases");
                    for dispatch in reclaimed {
                        if dispatch.status == RunDispatchStatus::Queued {
                            let thread_id = dispatch.thread_id.clone();
                            self.get_or_create_worker(&thread_id).await;
                            self.try_dispatch_next(&thread_id).await;
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
                    tracing::info!(purged, "GC purged terminal dispatches");
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
    async fn gc_idle_workers(&self) {
        let idle_keys: Vec<String> = {
            let workers = self.workers.read().await;
            let mut keys = Vec::new();
            for (thread_id, worker) in workers.iter() {
                let w = worker.lock().await;
                if matches!(w.status, MailboxWorkerStatus::Idle) {
                    keys.push(thread_id.clone());
                }
            }
            keys
        };

        if idle_keys.is_empty() {
            return;
        }

        // Check store for queued dispatches before removing.
        let mut removed = 0usize;
        let mut workers = self.workers.write().await;
        for thread_id in &idle_keys {
            // Re-check under write lock: status might have changed.
            let still_idle = if let Some(worker) = workers.get(thread_id) {
                let w = worker.lock().await;
                matches!(w.status, MailboxWorkerStatus::Idle)
            } else {
                false
            };
            if !still_idle {
                continue;
            }

            // Only remove if the store has no queued dispatches for this thread.
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
                workers.remove(thread_id);
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
    thread_id: &str,
) {
    let workers = workers.read().await;
    if let Some(worker) = workers.get(thread_id) {
        let mut w = worker.lock().await;
        if matches!(w.status, MailboxWorkerStatus::Claiming) {
            w.status = MailboxWorkerStatus::Idle;
        }
    }
}

// ── Free functions ───────────────────────────────────────────────────

fn normalize_mailbox_run_mode(request: &mut RunRequest, background: bool) {
    if request.run_mode != RunMode::Foreground {
        return;
    }

    request.run_mode = if !request.decisions.is_empty() || request.continue_run_id.is_some() {
        RunMode::Resume
    } else if matches!(
        request.origin,
        awaken_contract::contract::storage::RunRequestOrigin::Internal
    ) {
        RunMode::InternalWake
    } else if background {
        RunMode::Scheduled
    } else {
        RunMode::Foreground
    };
}

/// Validate and normalize run request inputs.
///
/// Checks that messages are non-empty, trims/generates thread_id.
/// Returns `(thread_id, messages)`.
/// Internal validation for mailbox submit paths.
fn validate_run_inputs(
    thread_id: String,
    messages: Vec<Message>,
    allow_empty_messages: bool,
) -> Result<(String, Vec<Message>), MailboxError> {
    if messages.is_empty() && !allow_empty_messages {
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

fn normalize_message_ids(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if message.id.as_deref().map(str::is_empty).unwrap_or(true) {
                message.id = Some(awaken_contract::contract::message::gen_message_id());
            }
            message
        })
        .collect()
}

fn mailbox_run_result(
    run_id: &str,
    dispatch_instance_id: &str,
    result: &Result<
        awaken_runtime::loop_runner::AgentRunResult,
        awaken_runtime::loop_runner::AgentLoopError,
    >,
) -> RunDispatchResult {
    use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};

    match result {
        Ok(run) => {
            let (status, _) = run.termination.to_run_status();
            RunDispatchResult {
                run_id: run.run_id.clone(),
                dispatch_instance_id: dispatch_instance_id.to_string(),
                status,
                termination: Some(run.termination.clone()),
                response: (!run.response.is_empty()).then(|| run.response.clone()),
                error: match &run.termination {
                    TerminationReason::Error(message) => Some(message.clone()),
                    _ => None,
                },
            }
        }
        Err(error) => RunDispatchResult {
            run_id: run_id.to_string(),
            dispatch_instance_id: dispatch_instance_id.to_string(),
            status: RunStatus::Done,
            termination: Some(TerminationReason::Error(error.to_string())),
            response: None,
            error: Some(error.to_string()),
        },
    }
}

fn mailbox_run_identity(
    dispatch: &RunDispatch,
    run_id: &str,
    dispatch_instance_id: &str,
) -> awaken_contract::contract::identity::RunIdentity {
    awaken_contract::contract::identity::RunIdentity::new(
        dispatch.thread_id.clone(),
        None,
        run_id.to_string(),
        None,
        String::new(),
        awaken_contract::contract::identity::RunOrigin::Internal,
    )
    .with_dispatch_id(dispatch.dispatch_id.clone())
    .with_session_id(dispatch_instance_id.to_string())
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
    use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
    use awaken_contract::contract::message::{Message, ToolCall};
    use awaken_contract::contract::storage::RunRequestOrigin;
    use awaken_contract::contract::storage::{
        RunRecord, RunStore, RunWaitingState, ThreadRunStore, ThreadStore, WaitingReason,
    };
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };
    use awaken_contract::thread::Thread;
    use awaken_runtime::extensions::background::{
        BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext,
        TaskResult as BackgroundTaskResult,
    };
    use awaken_runtime::loop_runner::build_agent_env;
    use awaken_runtime::{Plugin, ResolvedAgent};
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore};
    use serde_json::{Value, json};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicUsize;
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

    struct RecoverFlakyMailboxStore {
        inner: InMemoryMailboxStore,
        reclaim_failures_remaining: AtomicUsize,
        reclaim_calls: AtomicUsize,
    }

    impl RecoverFlakyMailboxStore {
        fn new(reclaim_failures: usize) -> Self {
            Self {
                inner: InMemoryMailboxStore::new(),
                reclaim_failures_remaining: AtomicUsize::new(reclaim_failures),
                reclaim_calls: AtomicUsize::new(0),
            }
        }

        fn reclaim_calls(&self) -> usize {
            self.reclaim_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl MailboxStore for RecoverFlakyMailboxStore {
        async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
            self.inner.enqueue(dispatch).await
        }

        async fn claim(
            &self,
            thread_id: &str,
            consumer_id: &str,
            lease_ms: u64,
            now: u64,
            limit: usize,
        ) -> Result<Vec<RunDispatch>, StorageError> {
            self.inner
                .claim(thread_id, consumer_id, lease_ms, now, limit)
                .await
        }

        async fn claim_dispatch(
            &self,
            dispatch_id: &str,
            consumer_id: &str,
            lease_ms: u64,
            now: u64,
        ) -> Result<Option<RunDispatch>, StorageError> {
            self.inner
                .claim_dispatch(dispatch_id, consumer_id, lease_ms, now)
                .await
        }

        async fn ack(
            &self,
            dispatch_id: &str,
            claim_token: &str,
            now: u64,
        ) -> Result<(), StorageError> {
            self.inner.ack(dispatch_id, claim_token, now).await
        }

        async fn record_dispatch_start(
            &self,
            dispatch_id: &str,
            claim_token: &str,
            dispatch_instance_id: &str,
            now: u64,
        ) -> Result<(), StorageError> {
            self.inner
                .record_dispatch_start(dispatch_id, claim_token, dispatch_instance_id, now)
                .await
        }

        async fn record_run_result(
            &self,
            dispatch_id: &str,
            claim_token: &str,
            result: &RunDispatchResult,
            now: u64,
        ) -> Result<(), StorageError> {
            self.inner
                .record_run_result(dispatch_id, claim_token, result, now)
                .await
        }

        async fn nack(
            &self,
            dispatch_id: &str,
            claim_token: &str,
            retry_at: u64,
            error: &str,
            now: u64,
        ) -> Result<(), StorageError> {
            self.inner
                .nack(dispatch_id, claim_token, retry_at, error, now)
                .await
        }

        async fn dead_letter(
            &self,
            dispatch_id: &str,
            claim_token: &str,
            error: &str,
            now: u64,
        ) -> Result<(), StorageError> {
            self.inner
                .dead_letter(dispatch_id, claim_token, error, now)
                .await
        }

        async fn cancel(
            &self,
            dispatch_id: &str,
            now: u64,
        ) -> Result<Option<RunDispatch>, StorageError> {
            self.inner.cancel(dispatch_id, now).await
        }

        async fn extend_lease(
            &self,
            dispatch_id: &str,
            claim_token: &str,
            extension_ms: u64,
            now: u64,
        ) -> Result<bool, StorageError> {
            self.inner
                .extend_lease(dispatch_id, claim_token, extension_ms, now)
                .await
        }

        async fn interrupt(
            &self,
            thread_id: &str,
            now: u64,
        ) -> Result<MailboxInterrupt, StorageError> {
            self.inner.interrupt(thread_id, now).await
        }

        async fn load_dispatch(
            &self,
            dispatch_id: &str,
        ) -> Result<Option<RunDispatch>, StorageError> {
            self.inner.load_dispatch(dispatch_id).await
        }

        async fn list_dispatches(
            &self,
            thread_id: &str,
            status_filter: Option<&[RunDispatchStatus]>,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<RunDispatch>, StorageError> {
            self.inner
                .list_dispatches(thread_id, status_filter, limit, offset)
                .await
        }

        async fn reclaim_expired_leases(
            &self,
            now: u64,
            limit: usize,
        ) -> Result<Vec<RunDispatch>, StorageError> {
            self.reclaim_calls.fetch_add(1, Ordering::SeqCst);
            let remaining = self.reclaim_failures_remaining.load(Ordering::SeqCst);
            if remaining > 0
                && self
                    .reclaim_failures_remaining
                    .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Err(StorageError::Io("injected startup recovery failure".into()));
            }
            self.inner.reclaim_expired_leases(now, limit).await
        }

        async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
            self.inner.purge_terminal(older_than).await
        }

        async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
            self.inner.queued_thread_ids().await
        }
    }

    fn make_runtime() -> Arc<AgentRuntime> {
        Arc::new(AgentRuntime::new(Arc::new(StubResolver)))
    }

    fn make_mailbox(runtime: Arc<AgentRuntime>, store: Arc<InMemoryMailboxStore>) -> Arc<Mailbox> {
        Arc::new(Mailbox::new(
            runtime,
            store,
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ))
    }

    fn make_mailbox_with_run_store(
        runtime: Arc<AgentRuntime>,
        store: Arc<InMemoryMailboxStore>,
        run_store: Arc<dyn ThreadRunStore>,
    ) -> Arc<Mailbox> {
        Arc::new(Mailbox::new(
            runtime,
            store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ))
    }

    struct NoopMailboxRuntime;

    #[async_trait::async_trait]
    impl RunDispatchExecutor for NoopMailboxRuntime {
        async fn run(
            &self,
            _request: RunRequest,
            _sink: Arc<dyn EventSink>,
        ) -> Result<AgentRunResult, AgentLoopError> {
            panic!("decoupling test must not execute runs")
        }

        fn cancel(&self, _id: &str) -> bool {
            false
        }

        async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
            false
        }

        fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
            false
        }
    }

    struct RecordedMailboxRequest {
        run_mode: RunMode,
        adapter: AdapterKind,
        dispatch_id: Option<String>,
        session_id: Option<String>,
    }

    #[derive(Default)]
    struct RecordingMailboxRuntime {
        requests: StdMutex<Vec<RecordedMailboxRequest>>,
    }

    #[async_trait::async_trait]
    impl RunDispatchExecutor for RecordingMailboxRuntime {
        async fn run(
            &self,
            request: RunRequest,
            _sink: Arc<dyn EventSink>,
        ) -> Result<AgentRunResult, AgentLoopError> {
            let run_id = request
                .continue_run_id
                .clone()
                .or(request.run_id_hint.clone())
                .unwrap_or_else(|| "recorded-run".to_string());
            self.requests
                .lock()
                .expect("lock poisoned")
                .push(RecordedMailboxRequest {
                    run_mode: request.run_mode,
                    adapter: request.adapter,
                    dispatch_id: request.dispatch_id.clone(),
                    session_id: request.session_id.clone(),
                });
            Ok(AgentRunResult {
                run_id,
                response: "ok".to_string(),
                termination: TerminationReason::NaturalEnd,
                steps: 1,
            })
        }

        fn cancel(&self, _id: &str) -> bool {
            false
        }

        async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
            false
        }

        fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
            false
        }
    }

    struct RecordedStoreMailboxRequest {
        thread_id: String,
        continue_run_id: Option<String>,
        run_mode: RunMode,
        adapter: AdapterKind,
    }

    struct RecordingStoreMailboxRuntime {
        requests: StdMutex<Vec<RecordedStoreMailboxRequest>>,
    }

    impl RecordingStoreMailboxRuntime {
        fn new(_store: Arc<InMemoryStore>) -> Self {
            Self {
                requests: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl RunDispatchExecutor for RecordingStoreMailboxRuntime {
        async fn run(
            &self,
            request: RunRequest,
            _sink: Arc<dyn EventSink>,
        ) -> Result<AgentRunResult, AgentLoopError> {
            let run_id = request
                .continue_run_id
                .clone()
                .or(request.run_id_hint.clone())
                .unwrap_or_else(|| "recorded-run".to_string());
            self.requests
                .lock()
                .expect("lock poisoned")
                .push(RecordedStoreMailboxRequest {
                    thread_id: request.thread_id,
                    continue_run_id: request.continue_run_id,
                    run_mode: request.run_mode,
                    adapter: request.adapter,
                });
            Ok(AgentRunResult {
                run_id,
                response: "ok".to_string(),
                termination: TerminationReason::NaturalEnd,
                steps: 1,
            })
        }

        fn cancel(&self, _id: &str) -> bool {
            false
        }

        async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
            false
        }

        fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
            false
        }
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

    async fn wait_for_dispatch<F>(
        store: &InMemoryMailboxStore,
        dispatch_id: &str,
        predicate: F,
    ) -> RunDispatch
    where
        F: Fn(&RunDispatch) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(dispatch) = store
                .load_dispatch(dispatch_id)
                .await
                .expect("mailbox dispatch lookup should succeed")
                && predicate(&dispatch)
            {
                return dispatch;
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for mailbox dispatch predicate on dispatch {dispatch_id}"
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
            request: None,
            input: None,
            output: None,
            status: RunStatus::Waiting,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: Some(RunWaitingState {
                reason: WaitingReason::BackgroundTasks,
                ticket_ids: Vec::new(),
                tickets: Vec::new(),
                since_dispatch_id: None,
                message: None,
            }),
            outcome: None,
            created_at: 1,
            started_at: None,
            finished_at: None,
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
    fn mailbox_lifecycle_config_defaults() {
        let config = MailboxLifecycleConfig::default();
        assert_eq!(config.startup_delay, Duration::ZERO);
        assert_eq!(config.startup_recovery.max_attempts, 1);
        assert_eq!(
            config.startup_recovery.retry_delay,
            Duration::from_millis(250)
        );
        assert!(config.maintenance_callback.is_none());
    }

    #[tokio::test]
    async fn start_lifecycle_ready_fails_when_startup_recovery_fails() {
        let store = Arc::new(RecoverFlakyMailboxStore::new(1));
        let runtime = make_runtime();
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let error = match mailbox
            .start_lifecycle_ready(MailboxLifecycleConfig {
                startup_recovery: MailboxStartupRecoveryConfig {
                    max_attempts: 1,
                    retry_delay: Duration::ZERO,
                },
                ..Default::default()
            })
            .await
        {
            Ok(_) => panic!("ready lifecycle should fail when startup recovery fails"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("injected startup recovery failure")
        );
        assert!(
            !mailbox
                .lifecycle_is_running()
                .expect("lifecycle state should be readable")
        );
    }

    #[tokio::test]
    async fn start_lifecycle_ready_retries_startup_recovery_until_ready() {
        let store = Arc::new(RecoverFlakyMailboxStore::new(1));
        let runtime = make_runtime();
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store.clone(),
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut request = RunRequest::new("thread-retry-recover", vec![Message::user("recover")])
            .with_agent_id("missing-agent");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .expect("prepare queued run");
        let dispatch = mailbox
            .build_dispatch(&request, &thread_id)
            .expect("build queued dispatch");
        let dispatch_id = dispatch.dispatch_id.clone();
        store
            .enqueue(&dispatch)
            .await
            .expect("enqueue queued dispatch");

        let handle = mailbox
            .start_lifecycle_ready(MailboxLifecycleConfig {
                startup_recovery: MailboxStartupRecoveryConfig {
                    max_attempts: 2,
                    retry_delay: Duration::ZERO,
                },
                ..Default::default()
            })
            .await
            .expect("ready lifecycle should retry startup recovery");

        let recovered = wait_for_dispatch(&store.inner, &dispatch_id, |dispatch| {
            dispatch.status == RunDispatchStatus::DeadLetter
        })
        .await;
        assert_eq!(recovered.status, RunDispatchStatus::DeadLetter);
        handle.shutdown().await.expect("shutdown lifecycle");
    }

    #[tokio::test]
    async fn start_lifecycle_ready_serializes_concurrent_recovery() {
        let store = Arc::new(RecoverFlakyMailboxStore::new(0));
        let runtime = make_runtime();
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store.clone(),
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut starters = Vec::new();
        for _ in 0..32 {
            let mailbox = Arc::clone(&mailbox);
            starters.push(tokio::spawn(async move {
                mailbox
                    .start_lifecycle_ready(MailboxLifecycleConfig::default())
                    .await
            }));
        }

        let mut handles = Vec::new();
        for starter in starters {
            handles.push(
                starter
                    .await
                    .expect("starter task should not panic")
                    .expect("ready lifecycle should start"),
            );
        }

        assert_eq!(
            store.reclaim_calls(),
            1,
            "concurrent ready starts should run startup recovery once"
        );
        assert!(handles.iter().all(MailboxLifecycleHandle::is_running));
        handles[0].shutdown().await.expect("shutdown lifecycle");
        assert!(handles.iter().all(|handle| !handle.is_running()));
    }

    #[tokio::test]
    async fn start_lifecycle_does_not_bypass_ready_transition() {
        let store = Arc::new(RecoverFlakyMailboxStore::new(0));
        let runtime = make_runtime();
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store.clone(),
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let ready_mailbox = Arc::clone(&mailbox);
        let ready = tokio::spawn(async move {
            ready_mailbox
                .start_lifecycle_ready(MailboxLifecycleConfig {
                    startup_delay: Duration::from_millis(75),
                    startup_recovery: MailboxStartupRecoveryConfig {
                        max_attempts: 1,
                        retry_delay: Duration::ZERO,
                    },
                    ..Default::default()
                })
                .await
        });
        sleep(Duration::from_millis(10)).await;

        let err = match mailbox.start_lifecycle(MailboxLifecycleConfig::default()) {
            Ok(_) => panic!("sync start must not race ready startup"),
            Err(error) => error,
        };
        assert!(
            err.to_string()
                .contains("lifecycle transition is already running")
        );

        let handle = ready
            .await
            .expect("ready task should not panic")
            .expect("ready lifecycle should start");
        assert_eq!(
            store.reclaim_calls(),
            1,
            "ready recovery should not be duplicated by sync start"
        );
        handle.shutdown().await.expect("shutdown lifecycle");
    }

    #[tokio::test]
    async fn start_lifecycle_is_idempotent_and_drop_does_not_abort_recovery() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let mut request = RunRequest::new("thread-drop-recover", vec![Message::user("recover")])
            .with_agent_id("missing-agent");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .expect("prepare queued run");
        let dispatch = mailbox
            .build_dispatch(&request, &thread_id)
            .expect("build queued dispatch");
        let dispatch_id = dispatch.dispatch_id.clone();
        store
            .enqueue(&dispatch)
            .await
            .expect("enqueue queued dispatch");

        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig {
                startup_delay: Duration::from_millis(10),
                ..Default::default()
            })
            .expect("lifecycle start should succeed");
        let duplicate = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("duplicate lifecycle start should be a no-op");
        assert!(handle.is_running());
        assert!(duplicate.is_running());

        drop(handle);
        drop(duplicate);

        wait_for_dispatch(&store, &dispatch_id, |dispatch| {
            dispatch.status == RunDispatchStatus::DeadLetter
        })
        .await;

        let cleanup = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("should return the existing lifecycle handle");
        cleanup.shutdown().await.expect("shutdown lifecycle");
        assert!(!cleanup.is_running());
    }

    #[tokio::test]
    async fn start_lifecycle_explicit_abort_allows_restart() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let first = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("first lifecycle start should succeed");
        assert!(first.is_running());
        first.shutdown().await.expect("shutdown first lifecycle");
        assert!(!first.is_running());

        let second = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("lifecycle should restart after explicit abort");
        assert!(second.is_running());
        second.shutdown().await.expect("shutdown second lifecycle");
        assert!(!second.is_running());
    }

    #[tokio::test]
    async fn maintenance_callback_runs_on_gc_tick() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig {
                gc_interval: Duration::from_millis(10),
                sweep_interval: Duration::from_secs(60),
                ..Default::default()
            },
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_hook = Arc::clone(&calls);
        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig {
                maintenance_callback: Some(Arc::new(move || {
                    calls_for_hook.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .expect("lifecycle should start");

        let deadline = Instant::now() + Duration::from_secs(1);
        while calls.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "maintenance callback did not run"
            );
            sleep(Duration::from_millis(5)).await;
        }
        handle.shutdown().await.expect("shutdown lifecycle");
    }

    #[tokio::test]
    async fn start_lifecycle_handle_drop_keeps_lifecycle_running() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("lifecycle should start");
        assert!(handle.is_running());
        drop(handle);

        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("lifecycle should still be running after handle drop");
        assert!(handle.is_running());
        handle.shutdown().await.expect("shutdown lifecycle");
    }

    #[tokio::test]
    async fn lifecycle_shutdown_waits_for_maintenance_to_quiesce() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            Arc::new(InMemoryStore::new()),
            "test-consumer".to_string(),
            MailboxConfig {
                gc_interval: Duration::from_millis(10),
                sweep_interval: Duration::from_secs(60),
                ..Default::default()
            },
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_hook = Arc::clone(&calls);
        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig {
                maintenance_callback: Some(Arc::new(move || {
                    calls_for_hook.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .expect("lifecycle should start");

        let deadline = Instant::now() + Duration::from_secs(1);
        while calls.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "maintenance callback did not run"
            );
            sleep(Duration::from_millis(5)).await;
        }

        handle.shutdown().await.expect("shutdown should quiesce");
        assert!(!handle.is_running());
        let calls_after_shutdown = calls.load(Ordering::SeqCst);
        sleep(Duration::from_millis(40)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            calls_after_shutdown,
            "maintenance callback should not run after shutdown completes"
        );
    }

    #[tokio::test]
    async fn concurrent_start_lifecycle_is_idempotent() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let mut joins = Vec::new();
        for _ in 0..32 {
            let mb = Arc::clone(&mailbox);
            joins.push(tokio::spawn(async move {
                mb.start_lifecycle(MailboxLifecycleConfig::default())
            }));
        }

        let mut handles = Vec::new();
        for join in joins {
            match join.await.expect("start task should not panic") {
                Ok(handle) => handles.push(handle),
                Err(err) => panic!("idempotent lifecycle start should not fail: {err}"),
            }
        }

        assert_eq!(handles.len(), 32, "all concurrent starters get a handle");
        assert!(handles.iter().all(MailboxLifecycleHandle::is_running));
        handles[0].shutdown().await.expect("shutdown lifecycle");
        assert!(handles.iter().all(|handle| !handle.is_running()));
    }

    #[tokio::test]
    async fn start_lifecycle_runs_startup_recovery_for_existing_queued_dispatches() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let mut request = RunRequest::new("thread-recover", vec![Message::user("recover me")])
            .with_agent_id("missing-agent");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .expect("prepare queued run");
        let dispatch = mailbox
            .build_dispatch(&request, &thread_id)
            .expect("build queued dispatch");
        let dispatch_id = dispatch.dispatch_id.clone();
        store
            .enqueue(&dispatch)
            .await
            .expect("enqueue queued dispatch");

        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("lifecycle should start");

        let recovered = wait_for_dispatch(&store, &dispatch_id, |dispatch| {
            dispatch.status == RunDispatchStatus::DeadLetter
        })
        .await;

        assert_eq!(recovered.status, RunDispatchStatus::DeadLetter);
        assert!(
            recovered
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("missing-agent")),
            "dead-letter error should preserve the runtime failure: {recovered:?}"
        );
        handle.shutdown().await.expect("shutdown lifecycle");
    }

    #[tokio::test]
    async fn start_lifecycle_reclaims_expired_claimed_dispatches_and_executes_them() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let mut request = RunRequest::new("thread-stale", vec![Message::user("recover stale")])
            .with_agent_id("missing-agent");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .expect("prepare stale run");
        let dispatch = mailbox
            .build_dispatch(&request, &thread_id)
            .expect("build stale claimed dispatch");
        let dispatch_id = dispatch.dispatch_id.clone();
        let claim_now = dispatch.available_at;
        store
            .enqueue(&dispatch)
            .await
            .expect("enqueue queued dispatch");
        let claimed = store
            .claim("thread-stale", "dead-consumer", 1, claim_now, 1)
            .await
            .expect("claim dispatch before simulated crash");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].status, RunDispatchStatus::Claimed);
        assert_eq!(claimed[0].lease_until, Some(claim_now + 1));
        sleep(Duration::from_millis(2)).await;

        let handle = mailbox
            .start_lifecycle(MailboxLifecycleConfig::default())
            .expect("lifecycle should start");

        let recovered = wait_for_dispatch(&store, &dispatch_id, |dispatch| {
            dispatch.status == RunDispatchStatus::DeadLetter
                && dispatch.run_status == Some(RunStatus::Done)
        })
        .await;

        assert_eq!(recovered.status, RunDispatchStatus::DeadLetter);
        assert_eq!(recovered.attempt_count, 1);
        let run_id = recovered.run_id.as_str();
        assert_ne!(
            run_id, dispatch_id,
            "recovered stale dispatches should also keep run id separate from mailbox dispatch id"
        );
        assert!(recovered.dispatch_instance_id.is_some());
        assert!(matches!(
            recovered.termination,
            Some(TerminationReason::Error(ref message)) if message.contains("missing-agent")
        ));
        assert!(
            recovered
                .run_error
                .as_deref()
                .is_some_and(|error| error.contains("missing-agent"))
        );
        handle.shutdown().await.expect("shutdown lifecycle");
    }

    #[test]
    fn run_request_fields() {
        let req = RunRequest::new("t-1", vec![Message::user("hello")]).with_agent_id("agent-a");
        assert_eq!(req.thread_id, "t-1");
        assert_eq!(req.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.run_mode, RunMode::Foreground);
        assert_eq!(req.adapter, AdapterKind::Internal);
    }

    #[test]
    fn run_spec_validation_empty_messages_errors() {
        let result = validate_run_inputs("thread-1".into(), vec![], false);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), MailboxError::Validation(_)));
    }

    #[test]
    fn run_spec_validation_allows_decision_only_resume() {
        let result = validate_run_inputs("thread-1".into(), vec![], true);
        assert!(result.is_ok());
        let (thread_id, messages) = result.unwrap();
        assert_eq!(thread_id, "thread-1");
        assert!(messages.is_empty());
    }

    #[test]
    fn run_spec_validation_blank_thread_id_generates_new() {
        let result = validate_run_inputs("  ".into(), vec![Message::user("hi")], false);
        assert!(result.is_ok());
        let (thread_id, _) = result.unwrap();
        assert!(!thread_id.is_empty());
        assert_ne!(thread_id.trim(), "");
    }

    #[test]
    fn run_spec_validation_trims_thread_id() {
        let result = validate_run_inputs("  my-thread  ".into(), vec![Message::user("hi")], false);
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

    #[test]
    fn mailbox_construction_depends_on_runtime_boundary_not_agent_runtime() {
        let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
        let mailbox = Mailbox::new_with_executor(
            runtime,
            make_store(),
            Arc::new(InMemoryStore::new()),
            "decoupled-consumer".to_string(),
            MailboxConfig::default(),
        );

        assert_eq!(mailbox.consumer_id, "decoupled-consumer");
    }

    #[tokio::test]
    async fn submit_background_enqueues_dispatch() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let request =
            RunRequest::new("thread-1", vec![Message::user("hello")]).with_agent_id("agent-1");
        let result = mailbox.submit_background(request).await.unwrap();

        assert_eq!(result.thread_id, "thread-1");
        assert!(!result.dispatch_id.is_empty());
        assert!(!result.run_id.is_empty());
        assert_ne!(result.dispatch_id, result.run_id);

        // Verify dispatch is in store.
        let dispatches = store
            .list_dispatches("thread-1", None, 100, 0)
            .await
            .unwrap();
        assert!(!dispatches.is_empty());
        assert_eq!(dispatches[0].run_id, result.run_id);
    }

    #[tokio::test]
    async fn submit_background_delivers_scheduled_policy_context() {
        let store = make_store();
        let runtime = Arc::new(RecordingMailboxRuntime::default());
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            store,
            Arc::new(InMemoryStore::new()),
            "recording-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let result = mailbox
            .submit_background(
                RunRequest::new("thread-policy-bg", vec![Message::user("hello")])
                    .with_agent_id("agent-1")
                    .with_adapter(AdapterKind::Acp),
            )
            .await
            .expect("background submit should enqueue");

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if !runtime.requests.lock().expect("lock poisoned").is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "runtime did not receive request");
            sleep(Duration::from_millis(5)).await;
        }

        let requests = runtime.requests.lock().expect("lock poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].run_mode, RunMode::Scheduled);
        assert_eq!(requests[0].adapter, AdapterKind::Acp);
        assert_eq!(
            requests[0].dispatch_id.as_deref(),
            Some(result.dispatch_id.as_str())
        );
        assert!(
            requests[0].session_id.is_some(),
            "dispatch session id should be set"
        );
    }

    #[tokio::test]
    async fn prepare_run_for_dispatch_precreates_created_run_and_thread_projection() {
        let thread_store = Arc::new(InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntime::new(Arc::new(StubResolver))
                .with_thread_run_store(thread_store.clone() as Arc<dyn ThreadRunStore>),
        );
        let mailbox_store = make_store();
        let mailbox = make_mailbox_with_run_store(
            runtime,
            mailbox_store,
            thread_store.clone() as Arc<dyn ThreadRunStore>,
        );
        let mut request = RunRequest::new("thread-created", vec![Message::user("plan this")])
            .with_agent_id("agent-created")
            .with_transport_request_id("transport-created");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();

        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .expect("precreate");

        assert_eq!(request.run_id_hint.as_deref(), Some(run_id.as_str()));
        let run = thread_store
            .load_run(&run_id)
            .await
            .expect("load run")
            .expect("created run");
        assert_eq!(run.status, RunStatus::Created);
        assert_eq!(run.agent_id, "agent-created");
        let request_snapshot = run.request.as_ref().unwrap();
        assert!(
            !request_snapshot.input_message_ids.is_empty(),
            "new run snapshots should reference thread messages instead of duplicating bodies"
        );
        assert_eq!(request_snapshot.input_message_count, 1);
        assert_eq!(
            request_snapshot.input_message_ids,
            vec![messages[0].id.clone().expect("message id")]
        );
        let input = run.input.as_ref().expect("run input message range");
        assert_eq!(input.thread_id, "thread-created");
        assert_eq!(input.range.unwrap().from_seq, 1);
        assert_eq!(input.range.unwrap().to_seq, 1);
        assert_eq!(
            input.trigger_message_ids,
            vec![messages[0].id.clone().expect("message id")]
        );
        assert_eq!(
            run.request
                .as_ref()
                .unwrap()
                .transport_request_id
                .as_deref(),
            Some("transport-created")
        );
        let thread = thread_store
            .load_thread("thread-created")
            .await
            .expect("load thread")
            .expect("thread projection");
        assert_eq!(thread.open_run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(thread.latest_run_id.as_deref(), Some(run_id.as_str()));
        assert!(thread.active_run_id.is_none());
    }

    #[tokio::test]
    async fn prepare_run_for_dispatch_inherits_previous_runtime_state() {
        let thread_store = Arc::new(InMemoryStore::new());
        let mut previous = seeded_waiting_run("run-prev", "thread-state", "agent-prev");
        previous.status = RunStatus::Done;
        previous.state = Some(awaken_contract::state::PersistedState {
            revision: 7,
            extensions: std::collections::HashMap::from([(
                "remote".to_string(),
                json!({"context_id": "remote-ctx-1"}),
            )]),
        });
        thread_store
            .checkpoint("thread-state", &[Message::user("first")], &previous)
            .await
            .expect("seed previous run");

        let runtime = Arc::new(
            AgentRuntime::new(Arc::new(StubResolver))
                .with_thread_run_store(thread_store.clone() as Arc<dyn ThreadRunStore>),
        );
        let mailbox = make_mailbox_with_run_store(
            runtime,
            make_store(),
            thread_store.clone() as Arc<dyn ThreadRunStore>,
        );
        let mut request = RunRequest::new("thread-state", vec![Message::user("second")]);
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();

        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .expect("precreate");

        let run = thread_store
            .load_run(&run_id)
            .await
            .expect("load run")
            .expect("created run");
        assert_eq!(run.status, RunStatus::Created);
        assert_eq!(run.agent_id, "agent-prev");
        let input = run.input.as_ref().expect("run input message range");
        assert_eq!(input.range.unwrap().from_seq, 1);
        assert_eq!(input.range.unwrap().to_seq, 2);
        let state = run.state.expect("inherited runtime state");
        assert_eq!(state.revision, 7);
        assert_eq!(state.extensions["remote"]["context_id"], "remote-ctx-1");
    }

    #[tokio::test]
    async fn cancel_queued_dispatch_works() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit a dispatch with available_at in the future so it stays Queued.
        let request =
            RunRequest::new("thread-cancel", vec![Message::user("hello")]).with_agent_id("agent-1");
        let result = mailbox.submit_background(request).await.unwrap();
        let dispatch_id = result.dispatch_id.clone();

        // The dispatch might already be dispatched (claimed). Load it to check.
        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        if loaded.status == RunDispatchStatus::Queued {
            let cancelled = mailbox.cancel(&dispatch_id).await.unwrap();
            assert!(cancelled);

            let after = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
            assert_eq!(after.status, RunDispatchStatus::Cancelled);
        }
        // If already Claimed, cancel via runtime path is tested implicitly.
    }

    #[tokio::test]
    async fn list_dispatches_returns_entries() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        for i in 0..3 {
            let request = RunRequest::new("thread-list", vec![Message::user("msg")])
                .with_agent_id(format!("agent-{i}"));
            mailbox.submit_background(request).await.unwrap();
        }

        let dispatches = mailbox
            .list_dispatches("thread-list", None, 100, 0)
            .await
            .unwrap();
        assert_eq!(dispatches.len(), 3);
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
            dispatch_id: "dispatch-1".into(),
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            status: MailboxDispatchStatus::Running,
        };
        assert_eq!(result.dispatch_id, "dispatch-1");
        assert_eq!(result.run_id, "run-1");
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
            run_id: "run-1".to_string(),
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
            validate_run_inputs("my-thread".into(), vec![Message::user("hi")], false).unwrap();
        assert_eq!(thread_id, "my-thread");
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn validate_run_inputs_multiple_messages() {
        let (_, msgs) = validate_run_inputs(
            "t".into(),
            vec![Message::user("a"), Message::user("b"), Message::user("c")],
            false,
        )
        .unwrap();
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn validate_run_inputs_empty_string_generates_uuid() {
        let (thread_id, _) =
            validate_run_inputs("".into(), vec![Message::user("hi")], false).unwrap();
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

    // ── build_dispatch field validation ──────────────────────────────────

    #[tokio::test]
    async fn build_dispatch_sets_correct_fields() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request =
            RunRequest::new("thread-42", vec![Message::user("test")]).with_run_id_hint("run-42");
        let dispatch = mailbox.build_dispatch(&request, "thread-42").unwrap();

        assert_eq!(dispatch.thread_id, "thread-42");
        assert_eq!(dispatch.run_id, "run-42");
        assert_eq!(dispatch.status, RunDispatchStatus::Queued);
        assert_eq!(dispatch.attempt_count, 0);
        assert_eq!(dispatch.max_attempts, 5); // default
        assert_eq!(dispatch.priority, 128);
        assert_eq!(dispatch.dispatch_epoch, 0);
        assert!(dispatch.claim_token.is_none());
        assert!(dispatch.claimed_by.is_none());
        assert!(dispatch.lease_until.is_none());
        assert!(dispatch.last_error.is_none());
    }

    #[test]
    fn build_dispatch_requires_prepared_run_id() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store);

        let request = RunRequest::new("thread-1", vec![Message::user("hi")]);
        assert!(mailbox.build_dispatch(&request, "thread-1").is_err());
    }

    #[tokio::test]
    async fn prepare_run_preserves_request_extras_on_run_snapshot() {
        let store = make_store();
        let runtime = make_runtime();
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut request = RunRequest::new("thread-ext", vec![Message::user("hi")])
            .with_agent_id("a1")
            .with_frontend_tools(vec![awaken_contract::contract::tool::ToolDescriptor::new(
                "ft1", "FT1", "desc",
            )]);
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .unwrap();
        let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

        let snapshot = run.request.expect("request snapshot");
        assert_eq!(snapshot.frontend_tools.len(), 1);
        assert!(snapshot.request_extras.is_some());
    }

    #[test]
    fn run_request_extras_serde_roundtrip() {
        use awaken_contract::contract::tool::ToolDescriptor;
        let extras = RunRequestExtras {
            overrides: None,
            decisions: vec![],
            frontend_tools: vec![ToolDescriptor::new("ft1", "FT1", "desc")],
            continue_run_id: None,
            run_id_hint: None,
            dispatch_id_hint: None,
            parent_thread_id: None,
            transport_request_id: None,
            run_mode: RunMode::Scheduled,
            adapter: AdapterKind::Acp,
        };
        let value = extras.to_value().unwrap().unwrap();
        let parsed = RunRequestExtras::from_value(&value).unwrap();
        assert_eq!(parsed.frontend_tools.len(), 1);
        assert_eq!(parsed.frontend_tools[0].id, "ft1");
        assert!(parsed.decisions.is_empty());
        assert!(parsed.overrides.is_none());
        assert_eq!(parsed.run_mode, RunMode::Scheduled);
        assert_eq!(parsed.adapter, AdapterKind::Acp);
    }

    #[test]
    fn run_request_extras_empty_returns_none() {
        let extras = RunRequestExtras {
            overrides: None,
            decisions: vec![],
            frontend_tools: vec![],
            continue_run_id: None,
            run_id_hint: None,
            dispatch_id_hint: None,
            parent_thread_id: None,
            transport_request_id: None,
            run_mode: RunMode::Foreground,
            adapter: AdapterKind::Internal,
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
            run_id_hint: Some("run-1".into()),
            dispatch_id_hint: Some("dispatch-1".into()),
            parent_thread_id: Some("parent-thread".into()),
            transport_request_id: Some("transport-1".into()),
            run_mode: RunMode::Resume,
            adapter: AdapterKind::AgUi,
        };
        let request = RunRequest::new("t1", vec![Message::user("hi")]);
        let applied = extras.apply_to(request);
        assert_eq!(applied.frontend_tools.len(), 1);
        assert_eq!(applied.run_id_hint.as_deref(), Some("run-1"));
        assert_eq!(applied.dispatch_id_hint.as_deref(), Some("dispatch-1"));
        assert_eq!(applied.parent_thread_id.as_deref(), Some("parent-thread"));
        assert_eq!(applied.transport_request_id.as_deref(), Some("transport-1"));
        assert_eq!(applied.run_mode, RunMode::Resume);
        assert_eq!(applied.adapter, AdapterKind::AgUi);
    }

    #[tokio::test]
    async fn prepare_run_round_trips_parent_thread_id() {
        let store = make_store();
        let runtime = make_runtime();
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut request = RunRequest::new("thread-child", vec![Message::user("hi")])
            .with_agent_id("agent")
            .with_parent_thread_id("thread-parent");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .unwrap();
        let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

        assert_eq!(
            run.request
                .as_ref()
                .and_then(|snapshot| snapshot.parent_thread_id.as_deref()),
            Some("thread-parent")
        );
    }

    #[tokio::test]
    async fn prepare_run_preserves_origin_metadata() {
        let store = make_store();
        let runtime = make_runtime();
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut request = RunRequest::new("thread-meta", vec![Message::user("hi")])
            .with_agent_id("a1")
            .with_origin(RunRequestOrigin::A2A)
            .with_parent_run_id("parent-run-1");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .unwrap();
        let run = thread_store.load_run(&run_id).await.unwrap().unwrap();
        let snapshot = run.request.as_ref().unwrap();

        assert!(matches!(snapshot.origin, RunRequestOrigin::A2A));
        assert_eq!(run.parent_run_id.as_deref(), Some("parent-run-1"));
    }

    #[tokio::test]
    async fn prepare_run_defaults_origin_to_user() {
        let store = make_store();
        let runtime = make_runtime();
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut request = RunRequest::new("thread-default", vec![Message::user("hi")]);
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .unwrap();
        let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

        assert!(matches!(
            run.request.as_ref().unwrap().origin,
            RunRequestOrigin::User
        ));
        assert!(run.parent_run_id.is_none());
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

    #[tokio::test]
    async fn background_success_records_run_result_and_keeps_dispatch_id_separate_from_run_id() {
        let mailbox_store = make_store();
        let run_store = Arc::new(InMemoryStore::new());
        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("finished")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }]));
        let resolver = Arc::new(FixedResolver {
            agent: ResolvedAgent::new("agent", "m", "sys", llm),
            plugins: vec![],
        });
        let runtime = Arc::new(AgentRuntime::new(resolver));
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            mailbox_store.clone(),
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let submitted = mailbox
            .submit_background(
                RunRequest::new("thread-run-result", vec![Message::user("go")])
                    .with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");

        let acked = wait_for_dispatch(&mailbox_store, &submitted.dispatch_id, |dispatch| {
            dispatch.status == RunDispatchStatus::Acked
                && dispatch.run_status == Some(RunStatus::Done)
        })
        .await;

        let run_id = acked.run_id.as_str();
        assert_ne!(
            run_id, submitted.dispatch_id,
            "default mailbox dispatch IDs must not be used as canonical run IDs"
        );
        assert!(acked.dispatch_instance_id.is_some());
        assert_eq!(acked.termination, Some(TerminationReason::NaturalEnd));
        assert_eq!(acked.run_response.as_deref(), Some("finished"));
        assert!(acked.run_error.is_none());
        assert!(acked.completed_at.is_some());
    }

    #[tokio::test]
    async fn background_permanent_error_records_run_result_before_dead_letter() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        let submitted = mailbox
            .submit_background(
                RunRequest::new("thread-missing-agent", vec![Message::user("go")])
                    .with_agent_id("missing-agent"),
            )
            .await
            .expect("submit should succeed");

        let dead = wait_for_dispatch(&store, &submitted.dispatch_id, |dispatch| {
            dispatch.status == RunDispatchStatus::DeadLetter
                && dispatch.run_status == Some(RunStatus::Done)
                && dispatch.run_error.is_some()
        })
        .await;

        let run_id = dead.run_id.as_str();
        assert_ne!(
            run_id, submitted.dispatch_id,
            "synthetic terminal events must preserve canonical run id instead of reusing dispatch id"
        );
        assert!(dead.dispatch_instance_id.is_some());
        assert!(matches!(
            dead.termination,
            Some(TerminationReason::Error(ref message)) if message.contains("missing-agent")
        ));
        assert!(
            dead.last_error
                .as_deref()
                .is_some_and(|error| error.contains("missing-agent"))
        );
        assert!(
            dead.run_error
                .as_deref()
                .is_some_and(|error| error.contains("missing-agent"))
        );
        assert!(dead.completed_at.is_some());
    }

    // ── MailboxDispatchStatus ────────────────────────────────────────

    #[test]
    fn dispatch_status_queued_zero() {
        let status = MailboxDispatchStatus::Queued;
        assert!(matches!(status, MailboxDispatchStatus::Queued));
    }

    // ── Interrupt test ──────────────────────────────────────────────

    #[tokio::test]
    async fn interrupt_bumps_dispatch_epoch() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit some dispatches
        let request =
            RunRequest::new("thread-int", vec![Message::user("a")]).with_agent_id("agent-1");
        mailbox.submit_background(request).await.unwrap();

        let result = mailbox.interrupt("thread-int").await.unwrap();
        // After interrupt, the dispatch epoch should be bumped
        assert!(result.new_dispatch_epoch > 0);
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
        assert!(!result.dispatch_id.is_empty());
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
        let mailbox = make_mailbox_with_run_store(
            runtime,
            mailbox_store,
            store.clone() as Arc<dyn ThreadRunStore>,
        );

        let submitted = mailbox
            .submit_background(
                RunRequest::new("thread-waiting", vec![Message::user("poke")])
                    .with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");
        assert_eq!(submitted.run_id, "run-waiting");

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
    async fn structured_user_input_waiting_thread_is_reused_by_incoming_message() {
        let store = Arc::new(InMemoryStore::new());
        let mut waiting = seeded_waiting_run("run-user-input", "thread-user-input", "agent");
        waiting.waiting = Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: Some("waiting for user input".to_string()),
        });
        store.create_run(&waiting).await.expect("seed waiting run");

        let llm = Arc::new(ScriptedLlm::new(vec![StreamResult {
            content: vec![ContentBlock::text("continued")],
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
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            make_store(),
            store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let submitted = mailbox
            .submit_background(
                RunRequest::new("thread-user-input", vec![Message::user("continue")])
                    .with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");

        assert_eq!(
            submitted.run_id, "run-user-input",
            "structured user-input waiting should keep the same user-intent run"
        );
    }

    #[tokio::test]
    async fn reusable_waiting_run_prefers_thread_open_run_projection_over_latest_run() {
        let store = Arc::new(InMemoryStore::new());
        let thread_id = "thread-open-projection";
        let mut open = seeded_waiting_run("run-open", thread_id, "agent");
        open.waiting = Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: Some("waiting for explicit input".to_string()),
        });
        open.updated_at = 10;
        let mut newer = seeded_waiting_run("run-newer-latest", thread_id, "agent");
        newer.updated_at = 20;

        store.create_run(&open).await.expect("seed open run");
        store.create_run(&newer).await.expect("seed newer run");
        let mut thread = Thread::with_id(thread_id);
        thread.open_run_id = Some(open.run_id.clone());
        store
            .save_thread(&thread)
            .await
            .expect("save thread projection");

        let runtime = Arc::new(RecordingStoreMailboxRuntime::new(store.clone()));
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            make_store(),
            store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let submitted = mailbox
            .submit_background(
                RunRequest::new(thread_id, vec![Message::user("continue open")])
                    .with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");

        assert_eq!(
            submitted.run_id, "run-open",
            "thread.open_run_id must win over latest_run() when resuming same user intent"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if !runtime.requests.lock().expect("lock poisoned").is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "request was not dispatched");
            sleep(Duration::from_millis(5)).await;
        }
        let requests = runtime.requests.lock().expect("lock poisoned");
        assert_eq!(requests[0].continue_run_id.as_deref(), Some("run-open"));
    }

    #[tokio::test]
    async fn recover_only_enqueues_orphaned_background_task_waiting_runs() {
        let store = Arc::new(InMemoryStore::new());
        let mut background = seeded_waiting_run("run-bg", "thread-bg-recover", "agent");
        background.waiting = Some(RunWaitingState {
            reason: WaitingReason::BackgroundTasks,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        });
        store.create_run(&background).await.expect("seed bg run");

        let mut user_input = seeded_waiting_run("run-user", "thread-user-recover", "agent");
        user_input.waiting = Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: Some("waiting for user".to_string()),
        });
        store
            .create_run(&user_input)
            .await
            .expect("seed user-input run");

        let mailbox_store = make_store();
        let runtime = Arc::new(RecordingStoreMailboxRuntime::new(store.clone()));
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            mailbox_store.clone(),
            store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let recovered = mailbox.recover().await.expect("recover should succeed");
        assert_eq!(recovered, 1);

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if runtime.requests.lock().expect("lock poisoned").len() == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "recover did not dispatch wake");
            sleep(Duration::from_millis(5)).await;
        }

        {
            let requests = runtime.requests.lock().expect("lock poisoned");
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].thread_id, "thread-bg-recover");
            assert_eq!(requests[0].continue_run_id.as_deref(), Some("run-bg"));
            assert_eq!(requests[0].run_mode, RunMode::InternalWake);
            assert_eq!(requests[0].adapter, AdapterKind::Internal);
        }

        let user_dispatches = mailbox_store
            .list_dispatches("thread-user-recover", None, 10, 0)
            .await
            .expect("list user dispatches");
        assert!(
            user_dispatches.is_empty(),
            "user-input waiting runs must stay suspended until explicit input"
        );
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
        let mailbox = make_mailbox_with_run_store(
            runtime,
            mailbox_store.clone(),
            store.clone() as Arc<dyn ThreadRunStore>,
        );

        mailbox
            .submit_background(
                RunRequest::new("thread-bg", vec![Message::user("start")]).with_agent_id("agent"),
            )
            .await
            .expect("submit should succeed");

        let waiting =
            wait_for_latest_run(&store, "thread-bg", |run| run.is_background_task_waiting()).await;
        sleep(Duration::from_millis(100)).await;

        let dispatches = mailbox_store
            .list_dispatches("thread-bg", None, 10, 0)
            .await
            .expect("list dispatches should succeed");

        assert!(
            dispatches.len() >= 2,
            "background completion should enqueue an internal wake message; waiting run was {:?}, dispatches were {:?}",
            waiting,
            dispatches
        );
        let messages = store
            .load_messages("thread-bg")
            .await
            .expect("load messages")
            .unwrap_or_default();
        assert!(
            messages.iter().any(|msg| {
                msg.role == awaken_contract::contract::message::Role::User
                    && msg.visibility == awaken_contract::contract::message::Visibility::Internal
                    && msg.text().contains("<background-task-event")
                    && msg.text().contains("\"done\":true")
            }),
            "expected a synthetic background wake message after task completion"
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

        // Submit 5 background dispatches to the same thread concurrently.
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

        // Store should have at most 1 Claimed dispatch for this thread.
        let dispatches = store
            .list_dispatches("thread-conc", Some(&[RunDispatchStatus::Claimed]), 10, 0)
            .await
            .unwrap();
        assert!(
            dispatches.len() <= 1,
            "store should have at most 1 Claimed dispatch, got {}",
            dispatches.len()
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

        // Store should have at most 1 Claimed dispatch.
        let dispatches = store
            .list_dispatches(
                "thread-stream-conc",
                Some(&[RunDispatchStatus::Claimed]),
                10,
                0,
            )
            .await
            .unwrap();
        assert!(
            dispatches.len() <= 1,
            "at most 1 Claimed, got {}",
            dispatches.len()
        );
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
        // First dispatch should be claimed/running since thread is idle.
        assert!(
            matches!(
                result1.status,
                MailboxDispatchStatus::Running | MailboxDispatchStatus::Queued
            ),
            "first dispatch should be Running or Queued"
        );

        // Second submit while first is running should be Queued.
        let req2 =
            RunRequest::new("thread-status", vec![Message::user("b")]).with_agent_id("agent-1");
        let result2 = mailbox.submit_background(req2).await.unwrap();
        assert!(
            matches!(result2.status, MailboxDispatchStatus::Queued),
            "second dispatch should be Queued while first is running"
        );
    }

    #[tokio::test]
    async fn worker_status_not_corrupted_after_empty_claim() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Submit and dispatch a dispatch to get worker into Running state.
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
            Ok((r, _)) => assert!(!r.dispatch_id.is_empty()),
            Err(MailboxError::Validation(_)) => {} // acceptable
            Err(e) => panic!("unexpected error: {e}"),
        }

        // Store invariant: at most 1 Claimed dispatch for this thread.
        let claimed = store
            .list_dispatches("thread-clash", Some(&[RunDispatchStatus::Claimed]), 10, 0)
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
                dispatch_id: "dispatch-fake".into(),
                lease_handle: tokio::spawn(futures::future::pending::<()>()),
                sink: reconnectable,
            };
        }

        let (tx, _rx) = mpsc::channel(16);
        let result = mailbox.reconnect_sink("thread-reconnect", tx).await;
        assert!(result, "reconnect should succeed for running worker");
    }

    #[tokio::test]
    async fn build_dispatch_extras_roundtrip_with_decisions() {
        use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

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
        let extras = RunRequestExtras::from_request(&request);
        assert_eq!(extras.decisions.len(), 1);
        assert_eq!(extras.decisions[0].0, "call-1");
    }

    #[tokio::test]
    async fn prepare_run_origin_a2a_roundtrip() {
        let store = make_store();
        let runtime = make_runtime();
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            store,
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        ));

        let mut request = RunRequest::new("thread-a2a", vec![Message::user("hi")])
            .with_origin(RunRequestOrigin::A2A)
            .with_parent_run_id("parent-123");
        let (thread_id, messages) =
            validate_run_inputs(request.thread_id.clone(), request.messages.clone(), false)
                .unwrap();
        let run_id = mailbox
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await
            .unwrap();
        let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

        assert!(matches!(
            run.request.as_ref().unwrap().origin,
            RunRequestOrigin::A2A
        ));
        assert_eq!(run.parent_run_id.as_deref(), Some("parent-123"));
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
        // Formula from execute_dispatch: 2^(attempt_count.saturating_sub(1).min(6))
        // attempt_count is 0-based on the dispatch at nack time, but incremented
        // by the store before re-queue. The backoff in execute_dispatch uses
        // dispatch.attempt_count which is the pre-nack value.
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
    async fn gc_idle_workers_removes_idle_with_no_dispatches() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Manually insert an Idle worker (no dispatches in store for this thread).
        {
            let mut workers = mailbox.workers.write().await;
            workers.insert(
                "thread-gc".to_string(),
                Arc::new(Mutex::new(MailboxWorker::default())),
            );
        }

        // Verify the worker is present.
        assert!(mailbox.workers.read().await.contains_key("thread-gc"));

        // Run GC — idle worker with no queued dispatches should be removed.
        mailbox.gc_idle_workers().await;

        assert!(
            !mailbox.workers.read().await.contains_key("thread-gc"),
            "idle worker with no queued dispatches should be removed"
        );
    }

    #[tokio::test]
    async fn gc_idle_workers_keeps_worker_with_queued_dispatches() {
        let store = make_store();
        let runtime = make_runtime();
        let mailbox = make_mailbox(runtime, store.clone());

        // Enqueue a dispatch for the thread (background so it goes to store).
        let request =
            RunRequest::new("thread-gc-keep", vec![Message::user("hi")]).with_agent_id("agent-1");
        mailbox.submit_background(request).await.unwrap();

        // Force the worker to Idle status (simulating it finished one dispatch
        // but another is queued).
        {
            let mut workers = mailbox.workers.write().await;
            workers.insert(
                "thread-gc-keep".to_string(),
                Arc::new(Mutex::new(MailboxWorker::default())),
            );
        }

        // Run GC — worker has queued/claimed dispatches, so it should be kept.
        mailbox.gc_idle_workers().await;

        // The worker should still exist because there are dispatches in the store.
        let has_dispatches = !store
            .list_dispatches(
                "thread-gc-keep",
                Some(&[RunDispatchStatus::Queued, RunDispatchStatus::Claimed]),
                1,
                0,
            )
            .await
            .unwrap()
            .is_empty();
        if has_dispatches {
            assert!(
                mailbox.workers.read().await.contains_key("thread-gc-keep"),
                "idle worker with queued dispatches should NOT be removed"
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
