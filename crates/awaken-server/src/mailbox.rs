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
use parking_lot::Mutex as SyncMutex;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::mailbox::{MailboxStore, RunDispatchStatus};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};
use awaken_contract::contract::suspension::{ToolCallOutcome, ToolCallResume};
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_runtime::{AgentRuntime, RunRequest, ThreadContextSnapshot};

use crate::transport::channel_sink::ReconnectableEventSink;

/// Guard window for inline-claimed dispatches: if the process crashes between
/// enqueue and claim, the sweep will reclaim the dispatch after this period.
const INLINE_CLAIM_GUARD_MS: u64 = 60_000;
#[cfg(not(test))]
const REMOTE_CANCEL_WAIT_MS: u64 = 5_000;
#[cfg(test)]
const REMOTE_CANCEL_WAIT_MS: u64 = 250;
const REMOTE_CANCEL_POLL_MS: u64 = 25;
const DISPATCH_SIGNAL_BATCH_DEFAULT: usize = 32;
const DISPATCH_SIGNAL_EXPIRES_DEFAULT: Duration = Duration::from_millis(500);
const DISPATCH_SIGNAL_ERROR_DELAY: Duration = Duration::from_millis(250);
const DISPATCH_SIGNAL_BLOCKED_NACK_BASE_DELAY_DEFAULT: Duration = Duration::from_millis(500);
const DISPATCH_SIGNAL_BLOCKED_NACK_MAX_DELAY_DEFAULT: Duration = Duration::from_secs(30);
const DISPATCH_SIGNAL_BATCH_ENV: &str = "AWAKEN_DISPATCH_SIGNAL_BATCH_SIZE";
const DISPATCH_SIGNAL_EXPIRES_ENV: &str = "AWAKEN_DISPATCH_SIGNAL_FETCH_EXPIRES_MS";
const DISPATCH_SIGNAL_NACK_BASE_DELAY_ENV: &str = "AWAKEN_DISPATCH_SIGNAL_NACK_BASE_DELAY_MS";
const DISPATCH_SIGNAL_NACK_MAX_DELAY_ENV: &str = "AWAKEN_DISPATCH_SIGNAL_NACK_MAX_DELAY_MS";
const DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS_DEFAULT: usize = 32;
const DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS_ENV: &str =
    "AWAKEN_DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS";
const TERMINAL_RECONCILE_BATCH: usize = 100;
const MAILBOX_DEPTH_STATUSES: [RunDispatchStatus; 6] = [
    RunDispatchStatus::Queued,
    RunDispatchStatus::Claimed,
    RunDispatchStatus::Acked,
    RunDispatchStatus::Cancelled,
    RunDispatchStatus::Superseded,
    RunDispatchStatus::DeadLetter,
];

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

impl MailboxRunOutcome {
    fn metric_label(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::TransientError(_) => "transient_error",
            Self::PermanentError(_) => "permanent_error",
        }
    }
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

    /// Execute a run with an optional mailbox-provided thread cache.
    async fn run_with_thread_context(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let _ = thread_ctx;
        self.run(request, sink).await
    }

    /// Cancel an active run by run id or thread id.
    fn cancel(&self, id: &str) -> bool;

    /// Cancel an active run by thread id and wait for it to unregister.
    async fn cancel_and_wait_by_thread(&self, thread_id: &str) -> bool;

    /// Forward one human/tool decision to an active run.
    fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool;

    /// Forward direct input messages to an active run.
    fn send_messages(&self, id: &str, messages: Vec<Message>) -> bool {
        let _ = (id, messages);
        false
    }
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

    async fn run_with_thread_context(
        &self,
        request: RunRequest,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        AgentRuntime::run_with_thread_context(self, request, sink, thread_ctx).await
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

    fn send_messages(&self, id: &str, messages: Vec<Message>) -> bool {
        AgentRuntime::send_messages(self, id, messages)
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
    dispatch_signal_task: Option<JoinHandle<()>>,
    maintenance_task: JoinHandle<()>,
}

impl MailboxLifecycleTasks {
    fn abort(self) {
        if let Some(task) = self.recover_task {
            task.abort();
        }
        if let Some(task) = self.dispatch_signal_task {
            task.abort();
        }
        self.maintenance_task.abort();
    }

    async fn shutdown(self) -> Result<(), MailboxError> {
        if let Some(task) = self.recover_task {
            task.abort();
            await_lifecycle_task("mailbox startup recovery", task).await?;
        }
        if let Some(task) = self.dispatch_signal_task {
            task.abort();
            await_lifecycle_task("mailbox dispatch signal loop", task).await?;
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
        run_id: String,
        lease_handle: JoinHandle<()>,
        sink: Arc<ReconnectableEventSink>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchAttempt {
    Claimed,
    Busy,
    NoEligible,
    TransientError,
}

impl DispatchAttempt {
    fn started_execution(self) -> bool {
        matches!(self, DispatchAttempt::Claimed)
    }
}

/// Cached thread state, valid for the duration of a lease.
struct ThreadContext {
    messages: Vec<Message>,
    latest_run: Option<RunRecord>,
    run_cache: HashMap<String, RunRecord>,
}

impl ThreadContext {
    async fn load(run_store: &dyn ThreadRunStore, thread_id: &str) -> Result<Self, MailboxError> {
        let messages = run_store
            .load_messages(thread_id)
            .await?
            .unwrap_or_default();
        let latest_run = run_store.latest_run(thread_id).await?;
        let mut run_cache = HashMap::new();
        if let Some(ref run) = latest_run {
            run_cache.insert(run.run_id.clone(), run.clone());
        }
        Ok(Self {
            messages,
            latest_run,
            run_cache,
        })
    }

    fn get_run(&self, run_id: &str) -> Option<&RunRecord> {
        self.run_cache.get(run_id)
    }

    fn apply_checkpoint(&mut self, messages: &[Message], run: &RunRecord) {
        self.messages = messages.to_vec();
        self.latest_run = Some(run.clone());
        self.run_cache.insert(run.run_id.clone(), run.clone());
    }
}

/// Per-thread worker. Store is the sole queue authority.
struct MailboxWorker {
    status: MailboxWorkerStatus,
    thread_ctx: Option<ThreadContext>,
}

impl Default for MailboxWorker {
    fn default() -> Self {
        Self {
            status: MailboxWorkerStatus::Idle,
            thread_ctx: None,
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
    workers: RwLock<HashMap<String, Arc<SyncMutex<MailboxWorker>>>>,
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

    /// Default bounded channel capacity for the runtime->SSE relay.
    const EVENT_CHANNEL_CAPACITY: usize = 256;

    /// Forward a tool-call decision to an active run in this process only.
    ///
    /// Distributed callers should use [`Self::send_decision_live`] so remote
    /// active runs can receive the decision through targeted live delivery.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        self.executor.send_decision(id, tool_call_id, resume)
    }
}

mod cancel;
mod decision;
mod helpers;
mod lifecycle;
mod live_delivery;
mod signal_loop;
mod submit;

use helpers::*;

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
