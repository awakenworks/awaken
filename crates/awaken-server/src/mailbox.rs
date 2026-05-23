//! Mailbox service: persistent run queue, dispatch execution, leasing, and lifecycle management.

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

use awaken_contract::contract::commit_coordinator::{
    CommitCoordinator, OutboxServerEventPublisher,
};
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::mailbox::{MailboxStore, RunDispatchStatus};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::run::{
    RunActivationSnapshot, RunInputSnapshot, RunIntent, RunKind, RunOptions, RunTraceContext,
};
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};
use awaken_contract::contract::suspension::{ToolCallOutcome, ToolCallResume};
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_runtime::RunActivation;

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

// ── Legacy run snapshot conversion ───────────────────────────────

/// Typed envelope for RunActivation fields that Mailbox stores opaquely.
/// Centralizes the legacy run snapshot round-trip round-trip.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct LegacyRunSnapshotExtras {
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

impl LegacyRunSnapshotExtras {
    #[cfg(test)]
    fn from_request(request: &awaken_runtime::RunActivation) -> Self {
        Self {
            overrides: request.options.overrides.clone(),
            decisions: request.control.seeded_decisions.clone(),
            frontend_tools: request.options.frontend_tools.clone(),
            continue_run_id: request.resume_run_id().map(str::to_owned),
            run_id_hint: request.persistence.run_id_hint.clone(),
            dispatch_id_hint: request.persistence.dispatch_id_hint.clone(),
            parent_thread_id: request.trace.parent_thread_id.clone(),
            transport_request_id: request.trace.transport_request_id.clone(),
            run_mode: request.trace.run_mode,
            adapter: request.trace.adapter,
        }
    }

    #[cfg(test)]
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

    #[cfg(test)]
    fn apply_to(self, mut request: awaken_runtime::RunActivation) -> awaken_runtime::RunActivation {
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

pub(super) struct LegacyRunRequestSnapshotAdapter {
    pub snapshot: awaken_contract::contract::storage::RunRequestSnapshot,
    pub input: RunInputSnapshot,
    pub manifest: awaken_contract::contract::storage::PinnedRegistryManifest,
    pub thread_id: String,
    pub agent_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub extras: Option<LegacyRunSnapshotExtras>,
}

impl TryFrom<LegacyRunRequestSnapshotAdapter> for RunActivationSnapshot {
    type Error = String;

    fn try_from(value: LegacyRunRequestSnapshotAdapter) -> Result<Self, Self::Error> {
        let mut options = RunOptions {
            overrides: None,
            frontend_tools: value.snapshot.frontend_tools,
        };
        let mut trace = RunTraceContext {
            parent_run_id: value.parent_run_id,
            parent_thread_id: value.snapshot.parent_thread_id,
            origin: value.snapshot.origin.into(),
            adapter: AdapterKind::Internal,
            run_mode: RunMode::Resume,
            dispatch_id: None,
            session_id: None,
            transport_request_id: value.snapshot.transport_request_id,
            correlation_id: None,
        };
        let mut kind = RunKind::NewIntent;
        let mut seeded_decisions = value
            .snapshot
            .decisions
            .into_iter()
            .map(|decision| (decision.call_id, decision.resume))
            .collect::<Vec<_>>();

        if let Some(extras) = value.extras {
            if let Some(overrides) = extras.overrides {
                options.overrides = Some(overrides);
            }
            if !extras.frontend_tools.is_empty() {
                options.frontend_tools = extras.frontend_tools;
            }
            if !extras.decisions.is_empty() {
                seeded_decisions = extras.decisions;
            }
            if let Some(run_id) = extras.continue_run_id {
                kind = RunKind::HitlResume { run_id };
            }
            if let Some(parent_thread_id) = extras.parent_thread_id {
                trace.parent_thread_id = Some(parent_thread_id);
            }
            if let Some(transport_request_id) = extras.transport_request_id {
                trace.transport_request_id = Some(transport_request_id);
            }
            trace.run_mode = extras.run_mode;
            trace.adapter = extras.adapter;
        }

        Ok(RunActivationSnapshot {
            intent: RunIntent {
                agent_id: value.agent_id,
                thread_id: value.thread_id,
                kind,
            },
            input: value.input,
            options,
            trace,
            seeded_decisions,
            resolution_scope: value.manifest,
        })
    }
}

pub(super) fn legacy_input_snapshot(
    run: &RunRecord,
    snapshot: &awaken_contract::contract::storage::RunRequestSnapshot,
) -> RunInputSnapshot {
    if let Some(input) = run.input.as_ref() {
        return RunInputSnapshot {
            thread_id: input.thread_id.clone(),
            range: input.range,
            trigger_message_ids: input.trigger_message_ids.clone(),
            selected_message_ids: input.selected_message_ids.clone(),
            context_policy: input.context_policy.clone(),
            compacted_snapshot_id: input.compacted_snapshot_id.clone(),
        };
    }
    RunInputSnapshot {
        thread_id: run.thread_id.clone(),
        range: None,
        trigger_message_ids: snapshot.input_message_ids.clone(),
        selected_message_ids: Vec::new(),
        context_policy: None,
        compacted_snapshot_id: None,
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
            let mut request = RunActivation::new(thread_id.clone(), vec![wake_message])
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
    /// Single durable-write boundary. Sourced from
    /// `executor.commit_coordinator()` when the runtime supplies one, or
    /// from an implicit `MailboxRunStoreCoordinator` wrapping the
    /// constructor-supplied `run_store` for embedded callers whose
    /// executor doesn't expose a coordinator.
    coordinator: Arc<dyn CommitCoordinator>,
    /// Projection of `coordinator.thread_run_store()` cached for repeated
    /// read access; never diverges from the coordinator by construction.
    run_store: Arc<dyn ThreadRunStore>,
    consumer_id: String,
    workers: RwLock<HashMap<String, Arc<SyncMutex<MailboxWorker>>>>,
    config: MailboxConfig,
    runtime_event_capture: Option<RuntimeEventCaptureConfig>,
    server_event_publisher: Option<Arc<dyn OutboxServerEventPublisher>>,
    server_event_origin: String,
    lifecycle_tasks: Arc<StdMutex<Option<MailboxLifecycleTasks>>>,
    lifecycle_start_lock: Arc<Mutex<()>>,
}

impl Mailbox {
    /// Create a new Mailbox service.
    ///
    /// ADR-0038 D7: the mailbox holds a single `CommitCoordinator` and
    /// derives its `ThreadRunStore` from that coordinator's
    /// `thread_run_store()` — there is no parallel store handle to keep in
    /// sync. When the executor exposes its own coordinator (production
    /// runtime wiring), it is adopted directly; otherwise the supplied
    /// `run_store` is wrapped in an implicit `MailboxRunStoreCoordinator`
    /// for embedded/test callers whose executors carry no persistence
    /// boundary.
    pub fn new(
        executor: impl IntoDispatchExecutor,
        store: Arc<dyn MailboxStore>,
        run_store: Arc<dyn ThreadRunStore>,
        consumer_id: String,
        config: MailboxConfig,
    ) -> Self {
        let executor = executor.into_dispatch_executor();
        let coordinator = executor.commit_coordinator().unwrap_or_else(|| {
            Arc::new(MailboxRunStoreCoordinator::new(Arc::clone(&run_store)))
                as Arc<dyn CommitCoordinator>
        });
        let run_store = coordinator.thread_run_store();
        Self {
            executor,
            store,
            coordinator,
            run_store,
            consumer_id,
            workers: RwLock::new(HashMap::new()),
            config,
            runtime_event_capture: None,
            server_event_publisher: None,
            server_event_origin: "mailbox".to_string(),
            lifecycle_tasks: Arc::new(StdMutex::new(None)),
            lifecycle_start_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Default bounded channel capacity for the runtime->SSE relay.
    const EVENT_CHANNEL_CAPACITY: usize = 256;

    /// Single source of truth for the thread/run store. Always returns the
    /// store wrapped by the mailbox's [`CommitCoordinator`] — callers that
    /// previously held a parallel `Arc<dyn ThreadRunStore>` should reach
    /// it through this accessor instead.
    pub fn thread_run_store(&self) -> &Arc<dyn ThreadRunStore> {
        &self.run_store
    }

    /// Borrow the durable-write coordinator.
    pub fn coordinator(&self) -> &Arc<dyn CommitCoordinator> {
        &self.coordinator
    }

    /// Forward a tool-call decision to an active run in this process only.
    ///
    /// Distributed callers should use [`Self::send_decision_live`] so remote
    /// active runs can receive the decision through targeted live delivery.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        self.executor.send_decision(id, tool_call_id, resume)
    }
}

mod cancel;
mod checkpoint;
mod coordinator_facade;
mod decision;
mod dispatch_execution;
mod executor;
mod helpers;
mod lifecycle;
mod live_delivery;
mod runtime_event_capture;
mod server_event_capture;
mod signal_loop;
mod submit;

use self::coordinator_facade::MailboxRunStoreCoordinator;
use self::{helpers::*, runtime_event_capture::RuntimeEventCaptureConfig};
pub use coordinator_facade::IntoDispatchExecutor;
pub use executor::RunDispatchExecutor;

#[cfg(test)]
mod tests;
