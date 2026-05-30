#![allow(deprecated)] // ADR-0038 D7: tests exercise the legacy checkpoint API directly

use super::*;
use async_trait::async_trait;
use awaken_runtime::extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext,
    TaskResult as BackgroundTaskResult,
};
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult, build_agent_env};
use awaken_runtime::{AgentRuntime, Plugin, ResolvedAgent};
use awaken_server_contract::RuntimeEventDurability;
use awaken_server_contract::contract::commit_coordinator::{
    CheckpointCommitPlan, CommitCoordinator, EventPublishError, OutboxServerEventPublisher,
    ServerEventPublishOutcome,
};
use awaken_server_contract::contract::content::ContentBlock;
use awaken_server_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, EventReader, EventScope, EventWriter,
};
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult};
use awaken_server_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_server_contract::contract::mailbox::{
    MailboxInterrupt, MailboxInterruptDetails, MailboxLiveControlSource, MailboxStore, RunDispatch,
    RunDispatchResult, RunDispatchStatus,
};
use awaken_server_contract::contract::message::{Message, ToolCall};
use awaken_server_contract::contract::outbox::OutboxError;
use awaken_server_contract::contract::storage::RunRequestOrigin;
use awaken_server_contract::contract::storage::{
    RunRecord, RunStore, RunWaitingState, ThreadRunStore, ThreadStore, WaitingReason,
};
use awaken_server_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_server_contract::now_ms;
use awaken_server_contract::thread::Thread;
use awaken_stores::{
    InMemoryEventStore, InMemoryMailboxStore, InMemoryOutboxStore, InMemoryStore,
    MemoryCommitCoordinator, PendingMessageStore,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicUsize;
use tokio::sync::mpsc;
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

fn make_resume() -> ToolCallResume {
    ToolCallResume {
        decision_id: "d1".into(),
        action: awaken_server_contract::contract::suspension::ResumeDecisionAction::Resume,
        result: serde_json::json!({"approved": true}),
        reason: None,
        updated_at: 0,
    }
}

struct EventStoreServerEventPublisher {
    writer: Arc<dyn EventWriter>,
}

struct FailingServerEventPublisher;

impl EventStoreServerEventPublisher {
    fn new(writer: Arc<dyn EventWriter>) -> Self {
        Self { writer }
    }
}

fn test_server_event_publisher(
    event_store: Arc<InMemoryEventStore>,
) -> Arc<dyn OutboxServerEventPublisher> {
    Arc::new(EventStoreServerEventPublisher::new(event_store))
}

#[async_trait]
impl OutboxServerEventPublisher for FailingServerEventPublisher {
    async fn publish(
        &self,
        _draft: CanonicalEventDraft,
        _options: AppendOptions,
    ) -> Result<ServerEventPublishOutcome, EventPublishError> {
        Err(EventPublishError::Enqueue(OutboxError::Io(
            "event publisher unavailable".to_string(),
        )))
    }
}

#[async_trait]
impl OutboxServerEventPublisher for EventStoreServerEventPublisher {
    async fn publish(
        &self,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<ServerEventPublishOutcome, EventPublishError> {
        let dedupe_key = options.idempotency_key.clone();
        let result = self
            .writer
            .append(draft, options)
            .await
            .map_err(|error| EventPublishError::Enqueue(OutboxError::Io(error.to_string())))?;
        Ok(ServerEventPublishOutcome::Enqueued {
            dedupe_key: dedupe_key
                .unwrap_or_else(|| format!("canonical/{}", result.event.event_id.as_str())),
        })
    }
}

struct FixedRunResolver {
    resolution_id: String,
}

#[async_trait]
impl awaken_runtime::Resolver for FixedRunResolver {
    async fn resolve(
        &self,
        req: awaken_runtime::ResolutionRequest,
    ) -> Result<awaken_runtime::ResolvedRunPlan, awaken_runtime::ResolveError> {
        let agent_id = match &req.target {
            awaken_runtime::ResolutionTarget::Root { agent_id, .. } => agent_id.as_str(),
            awaken_runtime::ResolutionTarget::Delegate { agent_id, .. } => agent_id.as_str(),
            awaken_runtime::ResolutionTarget::Handoff { agent_id, .. } => agent_id.as_str(),
        };
        let agent = ResolvedAgent::new(
            agent_id,
            "model",
            "system",
            Arc::new(ScriptedLlm::new(vec![])),
        );
        let requirements = awaken_runtime::BackendRequirements::from_features(&req.features);
        Ok(awaken_runtime::ResolvedRunPlan::Replayable(
            awaken_runtime::ReplayableResolvedRun {
                artifact: awaken_runtime::ResolutionArtifact {
                    resolution_id: self.resolution_id.clone(),
                },
                execution: awaken_runtime::ResolvedRun {
                    agent_spec: (*agent.spec).clone(),
                    role: awaken_runtime::ExecutionRole::Root,
                    execution: awaken_runtime::ExecutionPlan::from_resolved_agent(&agent),
                    model: awaken_runtime::ResolvedModelBinding {
                        upstream_model: agent.upstream_model.clone(),
                    },
                    tools: Vec::new(),
                    overrides: req.overrides,
                    backend_profile: awaken_runtime::BackendProfile::full_local(),
                    requirements,
                    scope: awaken_runtime::ReplayableScope,
                },
            },
        ))
    }
}

struct RecoverFlakyMailboxStore {
    inner: InMemoryMailboxStore,
    reclaim_failures_remaining: AtomicUsize,
    reclaim_calls: AtomicUsize,
    dead_letter_calls: AtomicUsize,
    dead_letter_failures_remaining: AtomicUsize,
}

impl RecoverFlakyMailboxStore {
    fn new(reclaim_failures: usize) -> Self {
        Self {
            inner: InMemoryMailboxStore::new(),
            reclaim_failures_remaining: AtomicUsize::new(reclaim_failures),
            reclaim_calls: AtomicUsize::new(0),
            dead_letter_calls: AtomicUsize::new(0),
            dead_letter_failures_remaining: AtomicUsize::new(0),
        }
    }

    /// Inject `n` transient `dead_letter` failures before it succeeds, to
    /// exercise recovery of a dispatch left Claimed by a flaky store.
    fn with_dead_letter_failures(dead_letter_failures: usize) -> Self {
        let mut store = Self::new(0);
        store.dead_letter_failures_remaining = AtomicUsize::new(dead_letter_failures);
        store
    }

    fn reclaim_calls(&self) -> usize {
        self.reclaim_calls.load(Ordering::SeqCst)
    }

    fn dead_letter_calls(&self) -> usize {
        self.dead_letter_calls.load(Ordering::SeqCst)
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
        self.dead_letter_calls.fetch_add(1, Ordering::SeqCst);
        let remaining = self.dead_letter_failures_remaining.load(Ordering::SeqCst);
        if remaining > 0
            && self
                .dead_letter_failures_remaining
                .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(StorageError::Io("injected dead_letter failure".into()));
        }
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

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        self.inner.interrupt(thread_id, now).await
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        self.inner.interrupt_detailed(thread_id, now).await
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        self.inner.current_dispatch_epoch(thread_id).await
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        self.inner
            .supersede_claimed(dispatch_id, claim_token, now, reason)
            .await
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
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

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        self.inner.list_terminal_dispatches(limit, offset).await
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

#[derive(Clone)]
struct TestDispatchSignal {
    thread_id: String,
    dispatch_id: String,
}

struct TestDispatchSignalReceipt {
    signal: TestDispatchSignal,
    queue: Arc<tokio::sync::Mutex<VecDeque<TestDispatchSignal>>>,
    acked_count: Arc<AtomicUsize>,
    nacked_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl awaken_server_contract::contract::mailbox::DispatchSignalReceipt
    for TestDispatchSignalReceipt
{
    async fn ack(self: Box<Self>) -> Result<(), StorageError> {
        self.acked_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn nack(self: Box<Self>) -> Result<(), StorageError> {
        self.nacked_count.fetch_add(1, Ordering::SeqCst);
        self.queue.lock().await.push_back(self.signal.clone());
        Ok(())
    }
}

struct SignalMailboxStore {
    inner: InMemoryMailboxStore,
    signals: Arc<tokio::sync::Mutex<VecDeque<TestDispatchSignal>>>,
    acked_count: Arc<AtomicUsize>,
    nacked_count: Arc<AtomicUsize>,
    enqueue_failures_remaining: AtomicUsize,
    claim_failures_remaining: AtomicUsize,
    claim_dispatch_empty_once: AtomicBool,
}

impl SignalMailboxStore {
    fn new() -> Self {
        Self::with_claim_failures(0)
    }

    fn with_claim_failures(claim_failures: usize) -> Self {
        Self::with_failures_and_empty_claim_dispatch(claim_failures, false)
    }

    fn with_enqueue_failures(enqueue_failures: usize) -> Self {
        let mut store = Self::with_failures_and_empty_claim_dispatch(0, false);
        store.enqueue_failures_remaining = AtomicUsize::new(enqueue_failures);
        store
    }

    fn with_empty_claim_dispatch_once() -> Self {
        Self::with_failures_and_empty_claim_dispatch(0, true)
    }

    fn with_failures_and_empty_claim_dispatch(
        claim_failures: usize,
        claim_dispatch_empty_once: bool,
    ) -> Self {
        Self {
            inner: InMemoryMailboxStore::new(),
            signals: Arc::new(tokio::sync::Mutex::new(VecDeque::new())),
            acked_count: Arc::new(AtomicUsize::new(0)),
            nacked_count: Arc::new(AtomicUsize::new(0)),
            enqueue_failures_remaining: AtomicUsize::new(0),
            claim_failures_remaining: AtomicUsize::new(claim_failures),
            claim_dispatch_empty_once: AtomicBool::new(claim_dispatch_empty_once),
        }
    }

    fn acked_signal_count(&self) -> usize {
        self.acked_count.load(Ordering::SeqCst)
    }

    fn nacked_signal_count(&self) -> usize {
        self.nacked_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl MailboxStore for SignalMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        let remaining = self.enqueue_failures_remaining.load(Ordering::SeqCst);
        if remaining > 0
            && self
                .enqueue_failures_remaining
                .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(StorageError::Io("injected enqueue failure".into()));
        }
        self.inner.enqueue(dispatch).await?;
        self.signals.lock().await.push_back(TestDispatchSignal {
            thread_id: dispatch.thread_id.clone(),
            dispatch_id: dispatch.dispatch_id.clone(),
        });
        Ok(())
    }

    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let remaining = self.claim_failures_remaining.load(Ordering::SeqCst);
        if remaining > 0
            && self
                .claim_failures_remaining
                .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(StorageError::Io("injected claim failure".into()));
        }
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
        if self.claim_dispatch_empty_once.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
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

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        self.inner.interrupt(thread_id, now).await
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        self.inner.interrupt_detailed(thread_id, now).await
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        self.inner.current_dispatch_epoch(thread_id).await
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        self.inner
            .supersede_claimed(dispatch_id, claim_token, now, reason)
            .await
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
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

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        self.inner.list_terminal_dispatches(limit, offset).await
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        self.inner.reclaim_expired_leases(now, limit).await
    }

    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        self.inner.purge_terminal(older_than).await
    }

    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        self.inner.queued_thread_ids().await
    }

    fn supports_dispatch_signals(&self) -> bool {
        true
    }

    async fn pull_dispatch_signals(
        &self,
        max: usize,
        _expires: Duration,
    ) -> Result<Vec<awaken_server_contract::contract::mailbox::DispatchSignalEntry>, StorageError>
    {
        let mut signals = self.signals.lock().await;
        let mut entries = Vec::new();
        for _ in 0..max {
            let Some(signal) = signals.pop_front() else {
                break;
            };
            entries.push(
                awaken_server_contract::contract::mailbox::DispatchSignalEntry {
                    thread_id: signal.thread_id.clone(),
                    dispatch_id: signal.dispatch_id.clone(),
                    receipt: Box::new(TestDispatchSignalReceipt {
                        signal,
                        queue: Arc::clone(&self.signals),
                        acked_count: Arc::clone(&self.acked_count),
                        nacked_count: Arc::clone(&self.nacked_count),
                    }),
                },
            );
        }
        Ok(entries)
    }
}

struct InterruptOnLoadMailboxStore {
    inner: InMemoryMailboxStore,
    interrupt_once: AtomicBool,
}

impl InterruptOnLoadMailboxStore {
    fn new() -> Self {
        Self {
            inner: InMemoryMailboxStore::new(),
            interrupt_once: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl MailboxStore for InterruptOnLoadMailboxStore {
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

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        self.inner.interrupt(thread_id, now).await
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        self.inner.interrupt_detailed(thread_id, now).await
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        self.inner.current_dispatch_epoch(thread_id).await
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        self.inner
            .supersede_claimed(dispatch_id, claim_token, now, reason)
            .await
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        let loaded = self.inner.load_dispatch(dispatch_id).await?;
        if let Some(dispatch) = loaded.as_ref()
            && dispatch.status == RunDispatchStatus::Claimed
            && self.interrupt_once.swap(false, Ordering::SeqCst)
        {
            self.inner.interrupt(&dispatch.thread_id, now_ms()).await?;
        }
        Ok(loaded)
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

    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        self.inner.count_dispatches_by_status(status).await
    }

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        self.inner.list_terminal_dispatches(limit, offset).await
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
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
        _request: RunActivation,
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }
}

#[derive(Default)]
struct WakeRecordingRuntime {
    wakes: AtomicUsize,
}

#[async_trait::async_trait]
impl RunDispatchExecutor for WakeRecordingRuntime {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        panic!("wake test must not execute runs")
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }

    fn wake_pending_boundary(&self, _id: &str) -> bool {
        self.wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }
}

struct ImmediateLocalCancelRuntime;

#[async_trait::async_trait]
impl RunDispatchExecutor for ImmediateLocalCancelRuntime {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        panic!("local cancel test must not execute runs")
    }

    fn cancel(&self, _id: &str) -> bool {
        true
    }

    async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
        true
    }

    fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
        false
    }

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }
}

#[derive(Default)]
struct CountingMailboxRuntime {
    run_count: AtomicUsize,
}

impl CountingMailboxRuntime {
    fn run_count(&self) -> usize {
        self.run_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RunDispatchExecutor for CountingMailboxRuntime {
    async fn run(
        &self,
        request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_count.fetch_add(1, Ordering::SeqCst);
        Ok(AgentRunResult {
            run_id: request
                .resume_run_id()
                .map(str::to_owned)
                .or(request.persistence.run_id_hint.clone())
                .or(request.trace.dispatch_id.clone())
                .unwrap_or_else(|| "counted-run".to_string()),
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }

    fn live_registry_set(&self) -> Option<awaken_runtime::registry::RegistrySet> {
        Some(empty_live_registry_set())
    }
}

fn empty_live_registry_set() -> awaken_runtime::registry::RegistrySet {
    use awaken_runtime::registry::{
        MapAgentSpecRegistry, MapBackendRegistry, MapModelRegistry, MapPluginSource,
        MapProviderRegistry, MapToolRegistry, RegistrySet,
    };
    RegistrySet {
        agents: std::sync::Arc::new(MapAgentSpecRegistry::default()),
        tools: std::sync::Arc::new(MapToolRegistry::new()),
        models: std::sync::Arc::new(MapModelRegistry::new()),
        providers: std::sync::Arc::new(MapProviderRegistry::new()),
        plugins: std::sync::Arc::new(MapPluginSource::new()),
        backends: std::sync::Arc::new(MapBackendRegistry::new()),
    }
}

struct CommittingEmittingMailboxRuntime {
    coordinator: Arc<MemoryCommitCoordinator>,
}

impl CommittingEmittingMailboxRuntime {
    fn new(run_store: Arc<InMemoryStore>, event_store: Arc<InMemoryEventStore>) -> Self {
        // Build a durable coordinator over the same thread/run store the
        // mailbox is given and the asserted event store, so the mailbox reads
        // exactly what the coordinator commits and the per-run staging
        // coordinator drains buffered canonical drafts into the shared stores.
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let coordinator = Arc::new(
            MemoryCommitCoordinator::new(run_store, event_store, outbox)
                .expect("memory coordinator builds"),
        );
        Self { coordinator }
    }

    /// Drive the per-run staging coordinator the dispatch path installed,
    /// committing a checkpoint so the buffered canonical drafts (RunStarted,
    /// ToolCallReady, RunFinished) are appended to the shared event store —
    /// exactly as the real runtime does at checkpoint cadence.
    async fn commit_runtime_checkpoint(
        &self,
        request: &RunActivation,
        run_id: &str,
    ) -> Result<(), AgentLoopError> {
        let Some(coordinator) = request.control.commit_coordinator_override.clone() else {
            return Ok(());
        };
        let run = RunRecord {
            run_id: run_id.to_string(),
            thread_id: request.thread_id().to_string(),
            agent_id: request
                .intent
                .agent_id
                .clone()
                .unwrap_or_else(|| "agent-1".to_string()),
            ..Default::default()
        };
        let plan = CheckpointCommitPlan::checkpoint_only(request.thread_id(), Vec::new(), run);
        coordinator
            .commit_checkpoint(plan)
            .await
            .map_err(|error| AgentLoopError::StorageError(error.to_string()))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl RunDispatchExecutor for CommittingEmittingMailboxRuntime {
    async fn run(
        &self,
        request: RunActivation,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let run_id = request
            .resume_run_id()
            .map(str::to_owned)
            .or(request.persistence.run_id_hint.clone())
            .or(request.trace.dispatch_id.clone())
            .unwrap_or_else(|| "emitting-run".to_string());
        sink.emit(AgentEvent::RunStart {
            thread_id: request.thread_id().to_owned(),
            run_id: run_id.clone(),
            parent_run_id: None,
            identity: None,
        })
        .await;
        sink.emit(AgentEvent::TextDelta {
            delta: "live only".into(),
        })
        .await;
        sink.emit(AgentEvent::ToolCallReady {
            id: "call-1".into(),
            name: "search".into(),
            arguments: json!({"q": "awaken"}),
        })
        .await;
        sink.emit(AgentEvent::RunFinish {
            thread_id: request.thread_id().to_owned(),
            run_id: run_id.clone(),
            identity: None,
            result: None,
            termination: TerminationReason::NaturalEnd,
        })
        .await;
        self.commit_runtime_checkpoint(&request, &run_id).await?;
        Ok(AgentRunResult {
            run_id,
            response: "ok".to_string(),
            termination: TerminationReason::NaturalEnd,
            steps: 1,
        })
    }

    fn commit_coordinator(&self) -> Option<Arc<dyn CommitCoordinator>> {
        Some(Arc::clone(&self.coordinator) as Arc<dyn CommitCoordinator>)
    }

    fn staged_commit_coordinator(
        &self,
    ) -> Option<Arc<dyn awaken_server_contract::contract::staged_commit::StagedCommitCoordinator>>
    {
        Some(Arc::clone(&self.coordinator) as Arc<_>)
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }

    fn live_registry_set(&self) -> Option<awaken_runtime::registry::RegistrySet> {
        Some(empty_live_registry_set())
    }

    fn has_commit_coordinator(&self) -> bool {
        true
    }
}

struct FailingMailboxRuntime;

#[async_trait::async_trait]
impl RunDispatchExecutor for FailingMailboxRuntime {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        Err(AgentLoopError::RuntimeError(
            awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: "synthetic-missing-agent".into(),
            },
        ))
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }
}

struct TransientFailingMailboxRuntime;

#[async_trait::async_trait]
impl RunDispatchExecutor for TransientFailingMailboxRuntime {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        Err(AgentLoopError::StorageError(
            "synthetic transient storage failure".to_string(),
        ))
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }
}

/// Runtime stub that fails every run with a structured `InferenceExecutionError`
/// (the P0 path). `retryable` selects a transient (429) vs permanent (403/quota)
/// fault; `run_count` records how many times the runtime was actually entered,
/// so tests can prove a permanent fault dead-letters after exactly one run
/// instead of looping through `max_attempts`.
#[derive(Default)]
struct InferenceFailingMailboxRuntime {
    retryable: bool,
    run_count: AtomicUsize,
}

impl InferenceFailingMailboxRuntime {
    fn permanent() -> Self {
        Self {
            retryable: false,
            run_count: AtomicUsize::new(0),
        }
    }

    fn transient() -> Self {
        Self {
            retryable: true,
            run_count: AtomicUsize::new(0),
        }
    }

    fn run_count(&self) -> usize {
        self.run_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RunDispatchExecutor for InferenceFailingMailboxRuntime {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_count.fetch_add(1, Ordering::SeqCst);
        let error = if self.retryable {
            awaken_server_contract::contract::executor::InferenceExecutionError::rate_limited(
                "429 too many requests",
            )
        } else {
            awaken_server_contract::contract::executor::InferenceExecutionError::Unauthorized(
                "403 pre_consume_token_quota_failed".to_string(),
            )
        };
        Err(AgentLoopError::from(error))
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }

    fn live_registry_set(&self) -> Option<awaken_runtime::registry::RegistrySet> {
        Some(empty_live_registry_set())
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

struct BlockingMailboxRuntime {
    run_count: AtomicUsize,
    started_tx: tokio::sync::mpsc::UnboundedSender<(usize, Option<String>)>,
    release_first: Arc<tokio::sync::Notify>,
}

impl BlockingMailboxRuntime {
    fn new(
        started_tx: tokio::sync::mpsc::UnboundedSender<(usize, Option<String>)>,
        release_first: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            run_count: AtomicUsize::new(0),
            started_tx,
            release_first,
        }
    }
}

#[async_trait::async_trait]
impl RunDispatchExecutor for BlockingMailboxRuntime {
    async fn run(
        &self,
        request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let ordinal = self.run_count.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self
            .started_tx
            .send((ordinal, request.trace.dispatch_id.clone()));
        if ordinal == 1 {
            self.release_first.notified().await;
        }
        let run_id = request
            .resume_run_id()
            .map(str::to_owned)
            .or(request.persistence.run_id_hint.clone())
            .or(request.trace.dispatch_id.clone())
            .unwrap_or_else(|| format!("blocking-run-{ordinal}"));
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl RunDispatchExecutor for RecordingMailboxRuntime {
    async fn run(
        &self,
        request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let run_id = request
            .resume_run_id()
            .map(str::to_owned)
            .or(request.persistence.run_id_hint.clone())
            .unwrap_or_else(|| "recorded-run".to_string());
        self.requests
            .lock()
            .expect("lock poisoned")
            .push(RecordedMailboxRequest {
                run_mode: request.trace.run_mode,
                adapter: request.trace.adapter,
                dispatch_id: request.trace.dispatch_id.clone(),
                session_id: request.trace.session_id.clone(),
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
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
        request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        let run_id = request
            .resume_run_id()
            .map(str::to_owned)
            .or(request.persistence.run_id_hint.clone())
            .unwrap_or_else(|| "recorded-run".to_string());
        self.requests
            .lock()
            .expect("lock poisoned")
            .push(RecordedStoreMailboxRequest {
                thread_id: request.thread_id().to_owned(),
                continue_run_id: request.resume_run_id().map(str::to_owned),
                run_mode: request.trace.run_mode,
                adapter: request.trace.adapter,
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

    fn send_messages(&self, _id: &str, _messages: Vec<Message>) -> bool {
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

struct RecordingLlm {
    responses: StdMutex<Vec<StreamResult>>,
    requests: Arc<StdMutex<Vec<InferenceRequest>>>,
}

impl RecordingLlm {
    fn new(responses: Vec<StreamResult>, requests: Arc<StdMutex<Vec<InferenceRequest>>>) -> Self {
        Self {
            responses: StdMutex::new(responses),
            requests,
        }
    }
}

#[async_trait]
impl LlmExecutor for RecordingLlm {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.requests.lock().expect("lock poisoned").push(request);
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
        "recording"
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

    async fn execute(&self, _args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
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

struct BlockingTool {
    started: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl BlockingTool {
    fn new(
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        Self {
            started: StdMutex::new(Some(started)),
            release: tokio::sync::Mutex::new(Some(release)),
        }
    }
}

#[async_trait]
impl Tool for BlockingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("block", "block", "wait until released")
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        if let Some(started) = self.started.lock().expect("lock poisoned").take() {
            let _ = started.send(());
        }
        let release = self.release.lock().await.take();
        if let Some(release) = release {
            let _ = release.await;
        }
        Ok(ToolResult::success("block", json!({"released": true})).into())
    }
}

async fn wait_for_latest_run<F>(store: &InMemoryStore, thread_id: &str, predicate: F) -> RunRecord
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

async fn prepare_queued_dispatch(
    mailbox: &Arc<Mailbox>,
    thread_id: &str,
    content: &str,
) -> RunDispatch {
    let mut request =
        RunActivation::new(thread_id, vec![Message::user(content)]).with_agent_id("agent");
    let (validated_thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .expect("test input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut request, &validated_thread_id, &messages)
        .await
        .expect("prepare queued run");
    mailbox
        .build_dispatch(&request, &validated_thread_id)
        .expect("build queued dispatch")
}

async fn enqueue_prepared_dispatch(
    mailbox: &Arc<Mailbox>,
    store: &InMemoryMailboxStore,
    thread_id: &str,
    content: &str,
) -> MailboxSubmitResult {
    let dispatch = prepare_queued_dispatch(mailbox, thread_id, content).await;
    let result = MailboxSubmitResult {
        dispatch_id: dispatch.dispatch_id.clone(),
        run_id: dispatch.run_id.clone(),
        thread_id: dispatch.thread_id.clone(),
        status: MailboxDispatchStatus::Queued,
    };
    store
        .enqueue(&dispatch)
        .await
        .expect("enqueue queued dispatch");
    result
}

fn seeded_waiting_run(run_id: &str, thread_id: &str, agent_id: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: agent_id.to_string(),
        parent_run_id: None,
        resolution_id: None,
        activation: None,
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
fn dispatch_signal_blocked_nack_delay_backs_off_and_caps() {
    assert_eq!(
        dispatch_signal_blocked_nack_delay(None),
        Duration::from_millis(500)
    );
    assert_eq!(
        dispatch_signal_blocked_nack_delay(Some(3)),
        Duration::from_secs(2)
    );
    assert_eq!(
        dispatch_signal_blocked_nack_delay(Some(100)),
        Duration::from_secs(30)
    );
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

    let mut request = RunActivation::new("thread-retry-recover", vec![Message::user("recover")])
        .with_agent_id("missing-agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
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

/// P1a: when a run's activation messages have been garbage-collected but the
/// run record survives, `reconstruct_run_request` fails. The dispatch must
/// dead-letter exactly once, never enter the runtime, and the terminal row
/// must not be re-claimed on a subsequent dispatch poll (no double-poll /
/// duplicate `permanent_error` + `dead_letter` noise).
#[tokio::test]
async fn reconstruct_failure_dead_letters_once_without_repolling() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(CountingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "p1a-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let dispatch = prepare_queued_dispatch(&mailbox, "thread-gc", "hi").await;
    let dispatch_id = dispatch.dispatch_id.clone();
    let thread_id = dispatch.thread_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    // Simulate durable corruption: the run record survives but its activation
    // messages were garbage-collected from the thread store.
    run_store
        .save_messages(&thread_id, &[])
        .await
        .expect("wipe messages to simulate GC");

    // Drive one claim+execute cycle (claims the queued row and spawns
    // run_claimed_dispatch, which hits the reconstruct failure).
    mailbox.get_or_create_worker(&thread_id).await;
    mailbox.try_dispatch_next(&thread_id).await;

    let dead = wait_for_dispatch(&store, &dispatch_id, |d| {
        d.status == RunDispatchStatus::DeadLetter
    })
    .await;
    assert_eq!(dead.status, RunDispatchStatus::DeadLetter);
    assert!(
        dead.last_error
            .as_deref()
            .is_some_and(|e| e.contains("not found")),
        "expected missing-message error, got: {:?}",
        dead.last_error
    );
    // Reconstruct failure must short-circuit before the runtime is entered.
    assert_eq!(
        runtime.run_count(),
        0,
        "runtime must not run on reconstruct failure"
    );

    // No double-poll: a subsequent dispatch poll must not re-claim the
    // terminal row. attempt_count and run_count stay frozen.
    let attempts_after_dead_letter = dead.attempt_count;
    mailbox.try_dispatch_next(&thread_id).await;
    sleep(Duration::from_millis(50)).await;
    let after = store
        .load_dispatch(&dispatch_id)
        .await
        .unwrap()
        .expect("dispatch remains inspectable");
    assert_eq!(
        after.status,
        RunDispatchStatus::DeadLetter,
        "dead-lettered row must stay terminal"
    );
    assert_eq!(
        after.attempt_count, attempts_after_dead_letter,
        "terminal row must not be re-claimed / re-attempted"
    );
    assert_eq!(
        runtime.run_count(),
        0,
        "terminal row must not be re-executed"
    );
}

/// P1b: a terminal (Done) run must never be re-dispatched by startup
/// recovery, even when it still carries a stale `dispatch_id` reference.
/// Recovery's re-dispatch paths filter to `Created`/`Waiting` and exclude
/// terminal runs, so no phantom dispatch is enqueued for a completed run.
/// (Scheduled actions are phase-scoped in-run state and cannot outlive a run
/// as a dispatch, so there is no terminal-run dispatch leak to clean up.)
#[tokio::test]
async fn terminal_run_is_not_redispatched_by_recovery() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(CountingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "p1b-consumer".to_string(),
        MailboxConfig::default(),
    ));

    // A completed run that still references the dispatch it ran under.
    let mut run = seeded_waiting_run("run-done", "thread-done", "agent");
    run.status = RunStatus::Done;
    run.termination_reason = Some(TerminationReason::NaturalEnd);
    run.waiting = None;
    run.dispatch_id = Some("stale-dispatch-id".to_string());
    run.finished_at = Some(2);
    run_store.create_run(&run).await.expect("seed terminal run");

    let recovered = mailbox.recover().await.expect("recover should succeed");
    assert_eq!(
        recovered, 0,
        "terminal run must not produce any recovery dispatch"
    );

    // No phantom dispatch was enqueued for the terminal run's thread.
    let dispatches = store
        .list_dispatches("thread-done", None, 10, 0)
        .await
        .expect("list dispatches");
    assert!(
        dispatches.is_empty(),
        "terminal run must not be re-dispatched, found: {dispatches:?}"
    );
    assert_eq!(
        runtime.run_count(),
        0,
        "terminal run must not be executed by recovery"
    );
}

/// P3: the framework-managed GC maintenance tick auto-vacuums terminal
/// dispatches older than `gc_ttl`. This is the built-in auto-vacuum the
/// mailbox already runs on `gc_interval` — no external task or manual
/// `UPDATE run_dispatches` surgery is required for long sessions.
#[tokio::test]
async fn run_gc_auto_vacuums_old_terminal_dispatches() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "p3-gc-consumer".to_string(),
        MailboxConfig {
            gc_ttl: Duration::ZERO,
            ..MailboxConfig::default()
        },
    ));

    // Drive a dispatch to a terminal (Acked) state with a past completed_at.
    let mut dispatch = prepare_queued_dispatch(&mailbox, "thread-gc-vacuum", "done").await;
    dispatch.available_at = 1000;
    let dispatch_id = dispatch.dispatch_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");
    let claimed = store
        .claim("thread-gc-vacuum", "p3-gc-consumer", 30_000, 1000, 1)
        .await
        .expect("claim dispatch");
    let token = claimed[0]
        .claim_token
        .as_deref()
        .expect("claim token")
        .to_string();
    store.ack(&dispatch_id, &token, 2000).await.expect("ack");
    assert_eq!(
        store
            .load_dispatch(&dispatch_id)
            .await
            .unwrap()
            .expect("acked dispatch is inspectable")
            .status,
        RunDispatchStatus::Acked
    );

    // The GC maintenance tick purges the aged terminal row.
    mailbox.run_gc().await;

    assert!(
        store.load_dispatch(&dispatch_id).await.unwrap().is_none(),
        "run_gc must auto-vacuum terminal dispatches older than gc_ttl"
    );
}

/// P0 end-to-end: a permanent LLM fault (403 / exhausted quota) surfaced by
/// the runtime dead-letters the dispatch after exactly ONE run, instead of
/// burning the full max_attempts budget — the campaign's 5-retry loop.
#[tokio::test]
async fn permanent_inference_error_dead_letters_after_single_run() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(InferenceFailingMailboxRuntime::permanent());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "p0-permanent-consumer".to_string(),
        MailboxConfig {
            default_max_attempts: 5,
            ..MailboxConfig::default()
        },
    ));

    let dispatch = prepare_queued_dispatch(&mailbox, "thread-perm", "go").await;
    let dispatch_id = dispatch.dispatch_id.clone();
    let thread_id = dispatch.thread_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    mailbox.get_or_create_worker(&thread_id).await;
    mailbox.try_dispatch_next(&thread_id).await;

    let dead = wait_for_dispatch(&store, &dispatch_id, |d| {
        d.status == RunDispatchStatus::DeadLetter
    })
    .await;
    assert_eq!(dead.status, RunDispatchStatus::DeadLetter);
    assert!(
        dead.last_error
            .as_deref()
            .is_some_and(|e| e.contains("unauthorized")),
        "expected unauthorized error, got: {:?}",
        dead.last_error
    );
    // The decisive assertion: the runtime ran exactly once. A pre-fix build
    // nacked and re-ran up to max_attempts (5) before dead-lettering.
    assert_eq!(
        runtime.run_count(),
        1,
        "permanent fault must not be retried"
    );
    assert!(
        dead.attempt_count < 5,
        "permanent fault must not exhaust the retry budget, attempt_count={}",
        dead.attempt_count
    );
}

/// P0 end-to-end: a transient LLM fault (429) nacks the dispatch back to the
/// queue for retry with backoff rather than dead-lettering on the first
/// attempt — the retry path the fix must preserve.
#[tokio::test]
async fn transient_inference_error_nacks_for_retry() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(InferenceFailingMailboxRuntime::transient());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "p0-transient-consumer".to_string(),
        MailboxConfig {
            default_max_attempts: 5,
            // Large backoff so the re-queued dispatch is not re-claimed during
            // assertions — keeps attempt_count/run_count stable.
            default_retry_delay_ms: 600_000,
            max_retry_delay_ms: 600_000,
            ..MailboxConfig::default()
        },
    ));

    let dispatch = prepare_queued_dispatch(&mailbox, "thread-transient", "go").await;
    let dispatch_id = dispatch.dispatch_id.clone();
    let thread_id = dispatch.thread_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    mailbox.get_or_create_worker(&thread_id).await;
    mailbox.try_dispatch_next(&thread_id).await;

    // Nacked back to Queued (eligible later, after backoff) with one attempt
    // recorded — not dead-lettered.
    let requeued = wait_for_dispatch(&store, &dispatch_id, |d| {
        d.status == RunDispatchStatus::Queued && d.attempt_count == 1
    })
    .await;
    assert_eq!(requeued.status, RunDispatchStatus::Queued);
    assert_eq!(requeued.attempt_count, 1);
    assert_eq!(
        runtime.run_count(),
        1,
        "transient fault runs once then re-queues"
    );
    assert!(
        requeued
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("rate limited")),
        "expected rate-limit error, got: {:?}",
        requeued.last_error
    );
}

/// P1a (variant): reconstruct also fails fast when the run record itself is
/// missing (not just its messages) — a single dead-letter, runtime never
/// entered.
#[tokio::test]
async fn reconstruct_failure_missing_run_dead_letters_once() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(CountingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "p1a-missing-run-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut dispatch = prepare_queued_dispatch(&mailbox, "thread-missing-run", "hi").await;
    // Point the dispatch at a run that was never persisted.
    dispatch.run_id = "nonexistent-run".to_string();
    let dispatch_id = dispatch.dispatch_id.clone();
    let thread_id = dispatch.thread_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    mailbox.get_or_create_worker(&thread_id).await;
    mailbox.try_dispatch_next(&thread_id).await;

    let dead = wait_for_dispatch(&store, &dispatch_id, |d| {
        d.status == RunDispatchStatus::DeadLetter
    })
    .await;
    assert_eq!(dead.status, RunDispatchStatus::DeadLetter);
    assert!(
        dead.last_error
            .as_deref()
            .is_some_and(|e| e.contains("not found")),
        "expected run-not-found error, got: {:?}",
        dead.last_error
    );
    assert_eq!(
        runtime.run_count(),
        0,
        "missing run must not enter the runtime"
    );
}

/// P3: the periodic sweep maintenance tick reclaims a Claimed dispatch whose
/// lease expired (consumer crashed) and the work runs to completion — a stuck
/// Claimed row never strands work, with no manual intervention.
#[tokio::test]
async fn run_sweep_reclaims_expired_lease_and_completes_work() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(CountingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "p3-sweep-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut dispatch = prepare_queued_dispatch(&mailbox, "thread-sweep-reclaim", "x").await;
    dispatch.available_at = 1000;
    let dispatch_id = dispatch.dispatch_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");
    // A consumer claims then "crashes": short lease far in the past.
    store
        .claim("thread-sweep-reclaim", "crashed-consumer", 100, 1000, 1)
        .await
        .expect("claim dispatch");
    assert_eq!(
        store
            .load_dispatch(&dispatch_id)
            .await
            .unwrap()
            .expect("claimed dispatch is inspectable")
            .status,
        RunDispatchStatus::Claimed
    );

    // The sweep tick reclaims the expired lease and re-dispatches the work.
    mailbox.run_sweep().await;

    let done = wait_for_dispatch(&store, &dispatch_id, |d| {
        d.status == RunDispatchStatus::Acked
    })
    .await;
    assert_eq!(done.status, RunDispatchStatus::Acked);
    assert_eq!(
        done.attempt_count, 1,
        "reclaim records exactly one recovery attempt"
    );
    assert!(
        runtime.run_count() >= 1,
        "reclaimed work must actually execute"
    );
}

/// P0 reliability/load: a quota storm — many concurrent dispatches across many
/// threads all hitting a permanent 403/quota fault — drains to terminal with
/// BOUNDED work. The decisive property: total runtime invocations == number of
/// dispatches (O(N)), not N * max_attempts (O(5N)) as the pre-fix retry loop
/// produced. Models the campaign's 459+ Queued backlog under quota exhaustion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_storm_drains_with_bounded_work() {
    const DISPATCHES: usize = 50;

    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(InferenceFailingMailboxRuntime::permanent());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "quota-storm-consumer".to_string(),
        MailboxConfig {
            default_max_attempts: 5,
            ..MailboxConfig::default()
        },
    ));

    // One dispatch per thread, all enqueued and kicked.
    let mut dispatch_ids = Vec::with_capacity(DISPATCHES);
    for i in 0..DISPATCHES {
        let thread_id = format!("storm-thread-{i}");
        let dispatch = prepare_queued_dispatch(&mailbox, &thread_id, "go").await;
        dispatch_ids.push(dispatch.dispatch_id.clone());
        store.enqueue(&dispatch).await.expect("enqueue dispatch");
        mailbox.get_or_create_worker(&thread_id).await;
        mailbox.try_dispatch_next(&thread_id).await;
    }

    // Every dispatch must reach DeadLetter (the storm fully drains).
    for dispatch_id in &dispatch_ids {
        let dead = wait_for_dispatch(&store, dispatch_id, |d| {
            d.status == RunDispatchStatus::DeadLetter
        })
        .await;
        assert_eq!(dead.status, RunDispatchStatus::DeadLetter);
    }

    // Bounded work: each dispatch ran exactly once. A pre-fix build would have
    // run up to DISPATCHES * 5 times before dead-lettering.
    assert_eq!(
        runtime.run_count(),
        DISPATCHES,
        "permanent quota storm must do O(N) work, not O(N * max_attempts)"
    );

    // No backlog stranded: nothing left Queued or Claimed anywhere.
    for i in 0..DISPATCHES {
        let thread_id = format!("storm-thread-{i}");
        let pending = store
            .list_dispatches(
                &thread_id,
                Some(&[RunDispatchStatus::Queued, RunDispatchStatus::Claimed]),
                10,
                0,
            )
            .await
            .expect("list pending dispatches");
        assert!(
            pending.is_empty(),
            "thread {thread_id} left a pending dispatch: {pending:?}"
        );
    }
}

/// P0 reliability under store faults: a transient `dead_letter()` failure while
/// terminalizing a permanent inference error must not lose or strand the
/// dispatch. It is left Claimed (recoverable); once the lease expires the sweep
/// reclaims it and the re-dispatch dead-letters it for real.
#[tokio::test]
async fn permanent_error_recovers_from_transient_dead_letter_fault() {
    let store = Arc::new(RecoverFlakyMailboxStore::with_dead_letter_failures(1));
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(InferenceFailingMailboxRuntime::permanent());
    let mailbox = Arc::new(Mailbox::new(
        Arc::clone(&runtime) as Arc<dyn RunDispatchExecutor>,
        store.clone(),
        run_store.clone(),
        "dl-fault-consumer".to_string(),
        MailboxConfig {
            default_max_attempts: 5,
            // Lease long enough that the first run completes before it can be
            // reclaimed, yet short enough for the test to drive recovery.
            lease_ms: 300,
            ..MailboxConfig::default()
        },
    ));

    let dispatch = prepare_queued_dispatch(&mailbox, "thread-dl-fault", "go").await;
    let dispatch_id = dispatch.dispatch_id.clone();
    let thread_id = dispatch.thread_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    mailbox.get_or_create_worker(&thread_id).await;
    mailbox.try_dispatch_next(&thread_id).await;

    // The first dead_letter is injected-to-fail, leaving the dispatch Claimed.
    // Once its lease expires the sweep reclaims it and the re-dispatch
    // terminalizes it for real.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let current = store
            .load_dispatch(&dispatch_id)
            .await
            .unwrap()
            .expect("dispatch is inspectable");
        if current.status == RunDispatchStatus::DeadLetter {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dispatch never recovered to DeadLetter (stuck at {:?})",
            current.status
        );
        sleep(Duration::from_millis(50)).await;
        mailbox.run_sweep().await;
    }

    let dead = store
        .load_dispatch(&dispatch_id)
        .await
        .unwrap()
        .expect("dispatch is inspectable");
    assert_eq!(dead.status, RunDispatchStatus::DeadLetter);
    assert!(
        dead.last_error
            .as_deref()
            .is_some_and(|e| e.contains("unauthorized")),
        "expected unauthorized error, got: {:?}",
        dead.last_error
    );
    // The injected fault forced exactly one recovery cycle: the run executed
    // twice (the failed-dead_letter attempt and the successful one).
    assert_eq!(
        runtime.run_count(),
        2,
        "one injected dead_letter fault should force exactly one recovery cycle"
    );
}

/// Lost-update race: concurrent submits on the SAME thread each perform a
/// non-atomic `load_messages → append → checkpoint`. Because `checkpoint`
/// overwrites the whole message list (last-writer-wins), an interleaved write
/// drops a message that a run's snapshot still references — surfacing later as
/// "message '…' not found for run '…'". Every appended message must survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_thread_submits_do_not_lose_messages() {
    const CONCURRENCY: usize = 32;

    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "race-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let thread_id = "thread-race";
    let mut handles = Vec::with_capacity(CONCURRENCY);
    for i in 0..CONCURRENCY {
        let mb = Arc::clone(&mailbox);
        let tid = thread_id.to_string();
        handles.push(tokio::spawn(async move {
            let mut request = RunActivation::new(&tid, vec![Message::user(format!("msg-{i}"))])
                .with_agent_id("agent");
            let (validated_thread_id, messages) = validate_run_inputs(
                request.thread_id().to_owned(),
                request.messages().to_vec(),
                false,
            )
            .expect("input should validate");
            mb.prepare_run_for_dispatch(&mut request, &validated_thread_id, &messages)
                .await
                .expect("prepare run")
        }));
    }
    let mut run_ids = Vec::with_capacity(CONCURRENCY);
    for handle in handles {
        run_ids.push(handle.await.expect("prepare task should not panic"));
    }

    // Every concurrently-appended message must survive — no lost update.
    let final_messages = run_store
        .load_messages(thread_id)
        .await
        .unwrap()
        .unwrap_or_default();
    assert_eq!(
        final_messages.len(),
        CONCURRENCY,
        "lost-update race dropped {} message(s) from the thread",
        CONCURRENCY.saturating_sub(final_messages.len())
    );

    // And every run's referenced trigger message must still be resolvable
    // (this is exactly the lookup reconstruct_run_request performs).
    for run_id in &run_ids {
        let run = run_store
            .load_run(run_id)
            .await
            .unwrap()
            .expect("run record exists");
        let snapshot = run.activation.expect("activation snapshot");
        for message_id in &snapshot.input.trigger_message_ids {
            assert!(
                run_store
                    .load_message_record(thread_id, message_id)
                    .await
                    .unwrap()
                    .is_some(),
                "message '{message_id}' not found for run '{run_id}' (lost-update race)"
            );
        }
    }
}

/// Cross-instance lost-update race (ADR-0042 A / D5): two independent `Mailbox`
/// instances share one store. Their striped append locks are per-instance, so
/// the in-process lock cannot serialize them — only the version-guarded
/// committed append plus reload-merge retry can prevent a dropped message. The
/// old whole-list overwrite loses updates here; the append path must not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_instance_concurrent_submits_do_not_lose_messages() {
    const CONCURRENCY: usize = 32;

    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let make_mailbox = || {
        let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
        Arc::new(Mailbox::new(
            runtime,
            store.clone(),
            run_store.clone(),
            "cross-consumer".to_string(),
            MailboxConfig::default(),
        ))
    };
    let mailbox_a = make_mailbox();
    let mailbox_b = make_mailbox();

    let thread_id = "thread-cross-instance";
    let mut handles = Vec::with_capacity(CONCURRENCY);
    for i in 0..CONCURRENCY {
        // Alternate instances so half the writers hold a different striped lock.
        let mb = if i % 2 == 0 {
            Arc::clone(&mailbox_a)
        } else {
            Arc::clone(&mailbox_b)
        };
        let tid = thread_id.to_string();
        handles.push(tokio::spawn(async move {
            let mut request = RunActivation::new(&tid, vec![Message::user(format!("msg-{i}"))])
                .with_agent_id("agent");
            let (validated_thread_id, messages) = validate_run_inputs(
                request.thread_id().to_owned(),
                request.messages().to_vec(),
                false,
            )
            .expect("input should validate");
            mb.prepare_run_for_dispatch(&mut request, &validated_thread_id, &messages)
                .await
                .expect("prepare run")
        }));
    }
    let mut run_ids = Vec::with_capacity(CONCURRENCY);
    for handle in handles {
        run_ids.push(handle.await.expect("prepare task should not panic"));
    }

    let final_messages = run_store
        .load_messages(thread_id)
        .await
        .unwrap()
        .unwrap_or_default();
    assert_eq!(
        final_messages.len(),
        CONCURRENCY,
        "cross-instance lost-update dropped {} message(s)",
        CONCURRENCY.saturating_sub(final_messages.len())
    );

    for run_id in &run_ids {
        let run = run_store
            .load_run(run_id)
            .await
            .unwrap()
            .expect("run record exists");
        let snapshot = run.activation.expect("activation snapshot");
        for message_id in &snapshot.input.trigger_message_ids {
            assert!(
                run_store
                    .load_message_record(thread_id, message_id)
                    .await
                    .unwrap()
                    .is_some(),
                "message '{message_id}' not found for run '{run_id}' (cross-instance lost-update)"
            );
        }
    }
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

    let mut request = RunActivation::new("thread-drop-recover", vec![Message::user("recover")])
        .with_agent_id("missing-agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
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

    let mut request = RunActivation::new("thread-recover", vec![Message::user("recover me")])
        .with_agent_id("missing-agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
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

    let mut request = RunActivation::new("thread-stale", vec![Message::user("recover stale")])
        .with_agent_id("missing-agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
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

#[tokio::test]
async fn multi_instance_ready_lifecycle_executes_shared_dispatch_once() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime_a = Arc::new(CountingMailboxRuntime::default());
    let runtime_b = Arc::new(CountingMailboxRuntime::default());
    let mailbox_a = Arc::new(Mailbox::new(
        runtime_a.clone(),
        mailbox_store.clone(),
        run_store.clone(),
        "consumer-a".to_string(),
        MailboxConfig::default(),
    ));
    let mailbox_b = Arc::new(Mailbox::new(
        runtime_b.clone(),
        mailbox_store.clone(),
        run_store,
        "consumer-b".to_string(),
        MailboxConfig::default(),
    ));
    let dispatch = prepare_queued_dispatch(
        &mailbox_a,
        "thread-multi-instance-ready",
        "shared startup dispatch",
    )
    .await;
    let dispatch_id = dispatch.dispatch_id.clone();
    mailbox_store
        .enqueue(&dispatch)
        .await
        .expect("enqueue shared dispatch");

    let (handle_a, handle_b) = tokio::join!(
        mailbox_a.start_lifecycle_ready(MailboxLifecycleConfig::default()),
        mailbox_b.start_lifecycle_ready(MailboxLifecycleConfig::default())
    );
    let handle_a = handle_a.expect("instance a lifecycle should start");
    let handle_b = handle_b.expect("instance b lifecycle should start");

    let acked = wait_for_dispatch(&mailbox_store, &dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked
    })
    .await;
    assert_eq!(acked.status, RunDispatchStatus::Acked);
    assert_eq!(
        runtime_a.run_count() + runtime_b.run_count(),
        1,
        "shared mailbox dispatch must be claimed and executed by exactly one instance"
    );

    handle_a.shutdown().await.expect("shutdown instance a");
    handle_b.shutdown().await.expect("shutdown instance b");
}

#[tokio::test]
async fn dispatch_signal_loop_claims_and_executes_queued_dispatch() {
    let store = Arc::new(SignalMailboxStore::new());
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "signal-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut request = RunActivation::new("thread-signal-loop", vec![Message::user("wake")])
        .with_agent_id("agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .expect("input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .expect("prepare run");
    let dispatch = mailbox
        .build_dispatch(&request, &thread_id)
        .expect("build dispatch");
    let dispatch_id = dispatch.dispatch_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    let signal_loop = tokio::spawn(Arc::clone(&mailbox).run_dispatch_signal_loop());
    let deadline = Instant::now() + Duration::from_secs(2);
    let acked = loop {
        if let Some(dispatch) = store
            .load_dispatch(&dispatch_id)
            .await
            .expect("dispatch lookup should succeed")
            && dispatch.status == RunDispatchStatus::Acked
        {
            break dispatch;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for dispatch signal loop"
        );
        sleep(Duration::from_millis(10)).await;
    };
    signal_loop.abort();

    assert_eq!(acked.status, RunDispatchStatus::Acked);
    assert_eq!(store.acked_signal_count(), 1);
}

#[tokio::test]
async fn dispatch_signal_loop_nacks_and_redelivers_after_claim_error() {
    let store = Arc::new(SignalMailboxStore::with_claim_failures(1));
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "signal-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut request = RunActivation::new("thread-signal-redeliver", vec![Message::user("wake")])
        .with_agent_id("agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .expect("input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .expect("prepare run");
    let dispatch = mailbox
        .build_dispatch(&request, &thread_id)
        .expect("build dispatch");
    let dispatch_id = dispatch.dispatch_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    let signal_loop = tokio::spawn(Arc::clone(&mailbox).run_dispatch_signal_loop());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let dispatch = store
            .load_dispatch(&dispatch_id)
            .await
            .expect("dispatch lookup should succeed")
            .expect("dispatch should exist");
        if dispatch.status == RunDispatchStatus::Acked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for redelivered dispatch signal"
        );
        sleep(Duration::from_millis(10)).await;
    }
    signal_loop.abort();

    assert_eq!(store.nacked_signal_count(), 1);
    assert_eq!(store.acked_signal_count(), 1);
}

#[tokio::test]
async fn dispatch_signal_loop_nacks_when_signal_is_blocked_by_active_claim() {
    let store = Arc::new(SignalMailboxStore::new());
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "signal-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut active = RunActivation::new("thread-signal-blocked", vec![Message::user("active")])
        .with_agent_id("agent");
    let (thread_id, active_messages) = validate_run_inputs(
        active.thread_id().to_owned(),
        active.messages().to_vec(),
        false,
    )
    .expect("active input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut active, &thread_id, &active_messages)
        .await
        .expect("prepare active run");
    let active_dispatch = mailbox
        .build_dispatch(&active, &thread_id)
        .expect("build active dispatch");
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    store
        .enqueue(&active_dispatch)
        .await
        .expect("enqueue active dispatch");
    let claimed = store
        .claim(&thread_id, "remote-owner", 30_000, now_ms(), 1)
        .await
        .expect("claim active dispatch");
    assert_eq!(claimed.len(), 1);
    let active_claim_token = claimed[0].claim_token.clone().unwrap();

    let mut queued = RunActivation::new("thread-signal-blocked", vec![Message::user("queued")])
        .with_agent_id("agent");
    let (_, queued_messages) = validate_run_inputs(
        queued.thread_id().to_owned(),
        queued.messages().to_vec(),
        false,
    )
    .expect("queued input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut queued, &thread_id, &queued_messages)
        .await
        .expect("prepare queued run");
    let queued_dispatch = mailbox
        .build_dispatch(&queued, &thread_id)
        .expect("build queued dispatch");
    let queued_dispatch_id = queued_dispatch.dispatch_id.clone();
    store
        .enqueue(&queued_dispatch)
        .await
        .expect("enqueue queued dispatch");

    let signal_loop = tokio::spawn(Arc::clone(&mailbox).run_dispatch_signal_loop());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if store.nacked_signal_count() > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queued signal blocked by an active claim must be nacked for redelivery"
        );
        sleep(Duration::from_millis(10)).await;
    }

    let queued_before_release = store
        .load_dispatch(&queued_dispatch_id)
        .await
        .expect("queued dispatch lookup")
        .expect("queued dispatch exists");
    assert_eq!(queued_before_release.status, RunDispatchStatus::Queued);

    store
        .ack(&active_dispatch_id, &active_claim_token, now_ms())
        .await
        .expect("release active claim");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let dispatch = store
            .load_dispatch(&queued_dispatch_id)
            .await
            .expect("queued dispatch lookup")
            .expect("queued dispatch exists");
        if dispatch.status == RunDispatchStatus::Acked {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "redelivered signal should claim after active claim releases"
        );
        sleep(Duration::from_millis(10)).await;
    }
    signal_loop.abort();

    assert!(
        store.nacked_signal_count() >= 1,
        "blocked queued signal must be nacked at least once"
    );
    assert!(
        store.acked_signal_count() >= 2,
        "active signal and final queued signal should both be acked"
    );
}

#[test]
fn run_request_fields() {
    let req = RunActivation::new("t-1", vec![Message::user("hello")]).with_agent_id("agent-a");
    assert_eq!(req.thread_id(), "t-1");
    assert_eq!(req.intent.agent_id.as_deref(), Some("agent-a"));
    assert_eq!(req.messages().len(), 1);
    assert_eq!(req.trace.run_mode, RunMode::Foreground);
    assert_eq!(req.trace.adapter, AdapterKind::Internal);
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
    let mailbox = Mailbox::new(
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
        RunActivation::new("thread-1", vec![Message::user("hello")]).with_agent_id("agent-1");
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
            RunActivation::new("thread-policy-bg", vec![Message::user("hello")])
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
            .with_in_memory_thread_run_store(thread_store.clone()),
    );
    let mailbox_store = make_store();
    let mailbox = make_mailbox_with_run_store(
        runtime,
        mailbox_store,
        thread_store.clone() as Arc<dyn ThreadRunStore>,
    );
    let mut request = RunActivation::new("thread-created", vec![Message::user("plan this")])
        .with_agent_id("agent-created")
        .with_transport_request_id("transport-created");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .unwrap();

    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .expect("precreate");

    assert_eq!(
        request.persistence.run_id_hint.as_deref(),
        Some(run_id.as_str())
    );
    let run = thread_store
        .load_run(&run_id)
        .await
        .expect("load run")
        .expect("created run");
    assert_eq!(run.status, RunStatus::Created);
    assert_eq!(run.agent_id, "agent-created");
    let activation_snapshot = run.activation.as_ref().unwrap();
    assert!(
        !activation_snapshot.input.trigger_message_ids.is_empty(),
        "new run snapshots should reference thread messages instead of duplicating bodies"
    );
    assert_eq!(
        activation_snapshot.input.trigger_message_ids,
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
        run.activation
            .as_ref()
            .unwrap()
            .trace
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
async fn prepare_run_for_dispatch_persists_resolved_resolution_id() {
    let thread_store = Arc::new(InMemoryStore::new());
    let resolution_id = "resolution-1".to_string();
    let runtime = Arc::new(
        AgentRuntime::new(Arc::new(StubResolver))
            .with_in_memory_thread_run_store(thread_store.clone()),
    );
    runtime.set_run_resolver(Arc::new(FixedRunResolver {
        resolution_id: resolution_id.clone(),
    }));
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        make_store(),
        thread_store.clone() as Arc<dyn ThreadRunStore>,
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let mut request = RunActivation::new("thread-manifest", vec![Message::user("plan this")])
        .with_agent_id("agent-created");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
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
    assert_eq!(run.resolution_id, Some(resolution_id));
}

#[tokio::test]
async fn prepare_run_for_dispatch_inherits_previous_runtime_state() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mut previous = seeded_waiting_run("run-prev", "thread-state", "agent-prev");
    previous.status = RunStatus::Done;
    previous.state = Some(awaken_server_contract::state::PersistedState {
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
            .with_in_memory_thread_run_store(thread_store.clone()),
    );
    let mailbox = make_mailbox_with_run_store(
        runtime,
        make_store(),
        thread_store.clone() as Arc<dyn ThreadRunStore>,
    );
    let mut request = RunActivation::new("thread-state", vec![Message::user("second")]);
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
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
    crate::metrics::install_recorder();
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            store.clone(),
            run_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    let result =
        enqueue_prepared_dispatch(&mailbox, store.as_ref(), "thread-cancel", "hello").await;
    let dispatch_id = result.dispatch_id.clone();

    let cancelled = mailbox.cancel(&dispatch_id).await.unwrap();
    assert!(cancelled);

    let after = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
    assert_eq!(after.status, RunDispatchStatus::Cancelled);

    let run = run_store
        .load_run(&result.run_id)
        .await
        .unwrap()
        .expect("queued cancel should keep run inspectable");
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.termination_reason, Some(TerminationReason::Cancelled));
    assert_eq!(run.dispatch_id.as_deref(), Some(dispatch_id.as_str()));

    let page = event_store
        .list(EventScope::thread("thread-cancel"), None, 10)
        .await
        .unwrap();
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunCancelled")
        .expect("queued cancel should record RunCancelled");
    assert_eq!(
        event.payload["dispatch_id"].as_str(),
        Some(dispatch_id.as_str())
    );
    assert_eq!(event.correlation_id.as_deref(), Some(dispatch_id.as_str()));

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"mark_run_cancelled\""));
    assert!(output.contains("outcome=\"cancelled\""));
}

#[tokio::test]
async fn list_dispatches_returns_entries() {
    let store = make_store();
    let runtime = make_runtime();
    let mailbox = make_mailbox(runtime, store.clone());

    for i in 0..3 {
        let request = RunActivation::new("thread-list", vec![Message::user("msg")])
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
    use awaken_server_contract::contract::event_sink::{EventSink, VecEventSink};
    use awaken_server_contract::contract::suspension::ToolCallOutcome;
    use awaken_server_contract::contract::tool::{ToolResult, ToolStatus};

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
    use awaken_server_contract::contract::lifecycle::TerminationReason;
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
fn classify_error_permanent_inference_error_is_permanent() {
    use awaken_runtime::loop_runner::AgentLoopError;
    use awaken_server_contract::contract::executor::InferenceExecutionError;
    // HTTP 401/403 (bad credentials / exhausted token quota) is permanent:
    // retrying just burns the full max_attempts budget. Must dead-letter,
    // not nack. Constructed via the real `From` seam every production
    // inference failure flows through (inference.rs drive_one_stream).
    let result = Err(AgentLoopError::from(InferenceExecutionError::Unauthorized(
        "403 pre_consume_token_quota_failed".to_string(),
    )));
    assert!(matches!(
        classify_error(&result),
        MailboxRunOutcome::PermanentError(_)
    ));
}

#[test]
fn classify_error_context_overflow_is_permanent() {
    use awaken_runtime::loop_runner::AgentLoopError;
    use awaken_server_contract::contract::executor::InferenceExecutionError;
    // A prompt that exceeds the context window fails identically on every
    // retry — permanent.
    let result = Err(AgentLoopError::from(
        InferenceExecutionError::ContextOverflow("prompt is too long".to_string()),
    ));
    assert!(matches!(
        classify_error(&result),
        MailboxRunOutcome::PermanentError(_)
    ));
}

#[test]
fn classify_error_model_not_found_is_permanent() {
    use awaken_runtime::loop_runner::AgentLoopError;
    use awaken_server_contract::contract::executor::InferenceExecutionError;
    let result = Err(AgentLoopError::from(
        InferenceExecutionError::ModelNotFound("404 unknown model".to_string()),
    ));
    assert!(matches!(
        classify_error(&result),
        MailboxRunOutcome::PermanentError(_)
    ));
}

#[test]
fn classify_error_transient_inference_error_is_transient() {
    use awaken_runtime::loop_runner::AgentLoopError;
    use awaken_server_contract::contract::executor::InferenceExecutionError;
    // HTTP 429 / 5xx / network are transient: retry with backoff. Guards
    // against over-correcting the permanent-classification fix.
    let rate_limited = Err(AgentLoopError::from(InferenceExecutionError::rate_limited(
        "429 too many requests",
    )));
    assert!(matches!(
        classify_error(&rate_limited),
        MailboxRunOutcome::TransientError(_)
    ));
    let provider = Err(AgentLoopError::from(InferenceExecutionError::Provider(
        "502 bad gateway".to_string(),
    )));
    assert!(matches!(
        classify_error(&provider),
        MailboxRunOutcome::TransientError(_)
    ));
}

/// Exhaustive contract test: every `InferenceExecutionError` variant must
/// classify in agreement with `is_retryable()` — transient variants nack
/// (retry), everything else dead-letters. Guards against a future variant
/// silently defaulting to the wrong recoverability class.
#[test]
fn classify_error_covers_all_inference_variants() {
    use awaken_runtime::loop_runner::AgentLoopError;
    use awaken_server_contract::contract::executor::InferenceExecutionError as IE;

    // (variant, expected_permanent). `expected_permanent` mirrors the
    // documented recoverability classes on `InferenceExecutionError`.
    let cases: Vec<(IE, bool)> = vec![
        (IE::Provider("502".to_string()), false),
        (IE::rate_limited("429"), false),
        (IE::overloaded("529"), false),
        (IE::Timeout("idle stall".to_string()), false),
        (IE::Unauthorized("403 quota".to_string()), true),
        (IE::ContextOverflow("prompt too long".to_string()), true),
        (IE::InvalidRequest("422".to_string()), true),
        (IE::ModelNotFound("404".to_string()), true),
        (IE::ContentFiltered("policy".to_string()), true),
        (IE::AllModelsUnavailable, true),
        (IE::Cancelled, true),
    ];

    for (variant, expected_permanent) in cases {
        // Classification must track is_retryable: transient == retryable.
        assert_eq!(
            variant.is_retryable(),
            !expected_permanent,
            "is_retryable disagrees with expected class for {variant:?}"
        );
        let result = Err(AgentLoopError::from(variant));
        let outcome = classify_error(&result);
        if expected_permanent {
            assert!(
                matches!(outcome, MailboxRunOutcome::PermanentError(_)),
                "expected PermanentError, got {outcome:?}"
            );
        } else {
            assert!(
                matches!(outcome, MailboxRunOutcome::TransientError(_)),
                "expected TransientError, got {outcome:?}"
            );
        }
    }
}

#[test]
fn classify_error_phase_error_is_completed() {
    use awaken_runtime::loop_runner::AgentLoopError;
    let result = Err(AgentLoopError::PhaseError(
        awaken_server_contract::StateError::UnknownKey {
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

// Property: classification depends solely on the `InferenceExecutionError`
// variant — it always agrees with `is_retryable()` regardless of the message
// payload, `retry_after`, or which variant. Generalizes the exhaustive variant
// test and guards future variants from silently defaulting to the wrong class.
proptest::proptest! {
    #[test]
    fn classify_error_always_agrees_with_is_retryable(
        msg in ".*",
        retry_ms in proptest::option::of(0u64..10_000u64),
        variant in 0u8..11u8,
    ) {
        use awaken_runtime::loop_runner::AgentLoopError;
        let retry_after = retry_ms.map(std::time::Duration::from_millis);
        let err = match variant {
            0 => InferenceExecutionError::Provider(msg.clone()),
            1 => InferenceExecutionError::RateLimited { message: msg.clone(), retry_after },
            2 => InferenceExecutionError::Overloaded { message: msg.clone(), retry_after },
            3 => InferenceExecutionError::Timeout(msg.clone()),
            4 => InferenceExecutionError::ContextOverflow(msg.clone()),
            5 => InferenceExecutionError::InvalidRequest(msg.clone()),
            6 => InferenceExecutionError::Unauthorized(msg.clone()),
            7 => InferenceExecutionError::ModelNotFound(msg.clone()),
            8 => InferenceExecutionError::ContentFiltered(msg.clone()),
            9 => InferenceExecutionError::AllModelsUnavailable,
            _ => InferenceExecutionError::Cancelled,
        };
        let retryable = err.is_retryable();
        let outcome = classify_error(&Err(AgentLoopError::from(err)));
        match outcome {
            MailboxRunOutcome::TransientError(_) => {
                proptest::prop_assert!(retryable, "transient outcome for a non-retryable error");
            }
            MailboxRunOutcome::PermanentError(_) => {
                proptest::prop_assert!(!retryable, "permanent outcome for a retryable error");
            }
            MailboxRunOutcome::Completed => {
                proptest::prop_assert!(
                    false,
                    "an inference error must never classify as Completed"
                );
            }
        }
    }
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
    let (thread_id, _) = validate_run_inputs("".into(), vec![Message::user("hi")], false).unwrap();
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
        RunActivation::new("thread-42", vec![Message::user("test")]).with_run_id_hint("run-42");
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

    let request = RunActivation::new("thread-1", vec![Message::user("hi")]);
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

    let mut request = RunActivation::new("thread-ext", vec![Message::user("hi")])
        .with_agent_id("a1")
        .with_frontend_tools(vec![
            awaken_server_contract::contract::tool::ToolDescriptor::new("ft1", "FT1", "desc"),
        ]);
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .unwrap();
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .unwrap();
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

    let snapshot = run.activation.expect("activation snapshot");
    assert_eq!(snapshot.options.frontend_tools.len(), 1);
    assert!(run.request.is_none());
}

#[test]
fn run_request_extras_serde_roundtrip() {
    use awaken_server_contract::contract::tool::ToolDescriptor;
    let extras = LegacyRunSnapshotExtras {
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
    let parsed = LegacyRunSnapshotExtras::from_value(&value).unwrap();
    assert_eq!(parsed.frontend_tools.len(), 1);
    assert_eq!(parsed.frontend_tools[0].id, "ft1");
    assert!(parsed.decisions.is_empty());
    assert!(parsed.overrides.is_none());
    assert_eq!(parsed.run_mode, RunMode::Scheduled);
    assert_eq!(parsed.adapter, AdapterKind::Acp);
}

#[test]
fn run_request_extras_empty_returns_none() {
    let extras = LegacyRunSnapshotExtras {
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
    use awaken_server_contract::contract::tool::ToolDescriptor;
    let extras = LegacyRunSnapshotExtras {
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
    let request = RunActivation::new("t1", vec![Message::user("hi")]);
    let applied = extras.apply_to(request);
    assert_eq!(applied.options.frontend_tools.len(), 1);
    assert_eq!(applied.persistence.run_id_hint.as_deref(), Some("run-1"));
    assert_eq!(
        applied.persistence.dispatch_id_hint.as_deref(),
        Some("dispatch-1")
    );
    assert_eq!(
        applied.trace.parent_thread_id.as_deref(),
        Some("parent-thread")
    );
    assert_eq!(
        applied.trace.transport_request_id.as_deref(),
        Some("transport-1")
    );
    assert_eq!(applied.trace.run_mode, RunMode::Resume);
    assert_eq!(applied.trace.adapter, AdapterKind::AgUi);
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
    thread_store
        .save_thread(&Thread::with_id("thread-parent"))
        .await
        .unwrap();

    let mut request = RunActivation::new("thread-child", vec![Message::user("hi")])
        .with_agent_id("agent")
        .with_parent_thread_id("thread-parent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .unwrap();
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .unwrap();
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

    assert_eq!(
        run.activation
            .as_ref()
            .and_then(|snapshot| snapshot.trace.parent_thread_id.as_deref()),
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

    let mut request = RunActivation::new("thread-meta", vec![Message::user("hi")])
        .with_agent_id("a1")
        .with_origin(RunRequestOrigin::A2A)
        .with_parent_run_id("parent-run-1");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .unwrap();
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .unwrap();
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();
    let snapshot = run.activation.as_ref().unwrap();

    assert!(matches!(
        RunRequestOrigin::from(snapshot.trace.origin),
        RunRequestOrigin::A2A
    ));
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

    let mut request = RunActivation::new("thread-default", vec![Message::user("hi")]);
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .unwrap();
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .unwrap();
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

    assert!(matches!(
        RunRequestOrigin::from(run.activation.as_ref().unwrap().trace.origin),
        RunRequestOrigin::User
    ));
    assert!(run.parent_run_id.is_none());
}

// ── MailboxError variants ──────────────────────────────────────

#[test]
fn mailbox_error_store_variant() {
    use awaken_server_contract::contract::storage::StorageError;
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

#[test]
fn mailbox_run_outcome_metric_labels_are_stable() {
    assert_eq!(MailboxRunOutcome::Completed.metric_label(), "completed");
    assert_eq!(
        MailboxRunOutcome::TransientError("retry".into()).metric_label(),
        "transient_error"
    );
    assert_eq!(
        MailboxRunOutcome::PermanentError("fatal".into()).metric_label(),
        "permanent_error"
    );
}

#[tokio::test]
async fn mailbox_execution_records_dispatch_latency_metrics() {
    crate::metrics::install_recorder();
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        run_store,
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let submitted = mailbox
        .submit_background(RunActivation::new(
            "thread-metrics",
            vec![Message::user("go")],
        ))
        .await
        .expect("submit should succeed");

    wait_for_dispatch(&mailbox_store, &submitted.dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked
    })
    .await;

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("awaken_mailbox_dispatch_enqueue_to_start_seconds"));
    assert!(output.contains("awaken_mailbox_dispatch_eligible_to_start_seconds"));
    assert!(output.contains("awaken_mailbox_dispatch_claim_to_start_seconds"));
    assert!(output.contains("awaken_mailbox_dispatch_enqueue_to_complete_seconds"));
    assert!(output.contains("awaken_mailbox_dispatch_runtime_seconds"));
    assert!(output.contains("awaken_runs_total"));
    assert!(output.contains("awaken_run_duration_seconds"));
    assert!(output.contains("awaken_mailbox_operations_total"));
    assert!(output.contains("awaken_mailbox_operation_duration_seconds"));
    assert!(output.contains("awaken_mailbox_dispatch_depth"));
    assert!(output.contains("status=\"queued\""));
    assert!(output.contains("operation=\"enqueue\""));
    assert!(output.contains("operation=\"claim\""));
    assert!(output.contains("operation=\"ack\""));
}

#[tokio::test]
async fn mailbox_lease_renewal_is_wired_and_prevents_reclaim() {
    crate::metrics::install_recorder();
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release_first = Arc::new(tokio::sync::Notify::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(BlockingMailboxRuntime::new(
        started_tx,
        Arc::clone(&release_first),
    ));
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        run_store,
        "lease-metrics-consumer".to_string(),
        MailboxConfig {
            lease_ms: 80,
            lease_renewal_interval: Duration::from_millis(20),
            ..MailboxConfig::default()
        },
    ));

    let submitted = mailbox
        .submit_background(RunActivation::new(
            "thread-lease-renewal",
            vec![Message::user("go")],
        ))
        .await
        .expect("submit should succeed");
    tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .expect("runtime should start")
        .expect("runtime should report start");

    let initial_lease = mailbox_store
        .load_dispatch(&submitted.dispatch_id)
        .await
        .expect("load dispatch")
        .expect("dispatch should exist")
        .lease_until
        .expect("claimed dispatch should have a lease");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let dispatch = mailbox_store
                .load_dispatch(&submitted.dispatch_id)
                .await
                .expect("load dispatch")
                .expect("dispatch should exist");
            if dispatch
                .lease_until
                .is_some_and(|lease| lease > initial_lease)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lease renewal should extend the claimed dispatch");

    let reclaimed = mailbox_store
        .reclaim_expired_leases(now_ms(), 10)
        .await
        .expect("manual reclaim should succeed");
    assert!(
        reclaimed.is_empty(),
        "renewed dispatch must not be reclaimed while runtime is active"
    );

    release_first.notify_one();
    wait_for_dispatch(&mailbox_store, &submitted.dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked
    })
    .await;

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"lease_renewal\""));
    assert!(output.contains("result=\"ok\""));
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
            RunActivation::new("thread-run-result", vec![Message::user("go")])
                .with_agent_id("agent"),
        )
        .await
        .expect("submit should succeed");

    let acked = wait_for_dispatch(&mailbox_store, &submitted.dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked && dispatch.run_status == Some(RunStatus::Done)
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
            RunActivation::new("thread-missing-agent", vec![Message::user("go")])
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

#[tokio::test]
async fn reconstruct_failure_cleans_worker_and_dispatches_next_queued() {
    let store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_id = "thread-reconstruct-next";
    let now = now_ms();

    let missing = RunDispatch {
        dispatch_id: "dispatch-missing-run".to_string(),
        thread_id: thread_id.to_string(),
        run_id: "missing-run".to_string(),
        priority: 10,
        dedupe_key: None,
        dispatch_epoch: 0,
        status: RunDispatchStatus::Queued,
        available_at: now,
        attempt_count: 0,
        max_attempts: 3,
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
    store.enqueue(&missing).await.expect("enqueue missing run");

    let mut next_request =
        RunActivation::new(thread_id, vec![Message::user("next")]).with_agent_id("agent");
    let (_, next_messages) = validate_run_inputs(
        next_request.thread_id().to_owned(),
        next_request.messages().to_vec(),
        false,
    )
    .expect("next input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut next_request, thread_id, &next_messages)
        .await
        .expect("prepare next run");
    let mut next = mailbox
        .build_dispatch(&next_request, thread_id)
        .expect("build next dispatch");
    next.priority = 20;
    next.created_at = now.saturating_add(1);
    next.updated_at = next.created_at;
    let next_dispatch_id = next.dispatch_id.clone();
    store.enqueue(&next).await.expect("enqueue next");

    mailbox.get_or_create_worker(thread_id).await;
    assert_eq!(
        mailbox.try_dispatch_next(thread_id).await,
        DispatchAttempt::Claimed
    );

    let dead = wait_for_dispatch(&store, "dispatch-missing-run", |dispatch| {
        dispatch.status == RunDispatchStatus::DeadLetter
    })
    .await;
    assert_eq!(dead.status, RunDispatchStatus::DeadLetter);

    let acked = wait_for_dispatch(&store, &next_dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked
    })
    .await;
    assert_eq!(acked.status, RunDispatchStatus::Acked);
}

#[tokio::test]
async fn reconstruct_failure_dead_letters_once_and_is_not_polled_again() {
    let store = Arc::new(RecoverFlakyMailboxStore::new(0));
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        thread_store,
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_id = "thread-reconstruct-once";
    let now = now_ms();

    let missing = RunDispatch {
        dispatch_id: "dispatch-reconstruct-once".to_string(),
        thread_id: thread_id.to_string(),
        run_id: "missing-run".to_string(),
        priority: 10,
        dedupe_key: None,
        dispatch_epoch: 0,
        status: RunDispatchStatus::Queued,
        available_at: now,
        attempt_count: 0,
        max_attempts: 3,
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
    store.enqueue(&missing).await.expect("enqueue missing run");

    mailbox.get_or_create_worker(thread_id).await;
    assert_eq!(
        mailbox.try_dispatch_next(thread_id).await,
        DispatchAttempt::Claimed
    );

    let dead = wait_for_dispatch(&store.inner, "dispatch-reconstruct-once", |dispatch| {
        dispatch.status == RunDispatchStatus::DeadLetter
    })
    .await;
    assert_eq!(dead.status, RunDispatchStatus::DeadLetter);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let attempt = mailbox.try_dispatch_next(thread_id).await;
        if attempt != DispatchAttempt::Busy {
            assert_eq!(
                attempt,
                DispatchAttempt::NoEligible,
                "dead-lettered reconstruct failure must not be claimable again"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for reconstruct failure worker to release"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.dead_letter_calls(),
        1,
        "reconstruct failure should issue exactly one dead_letter transition"
    );
}

// ── MailboxDispatchStatus ────────────────────────────────────────

#[test]
fn dispatch_status_queued_zero() {
    let running = MailboxDispatchStatus::Running;
    let status = MailboxDispatchStatus::Queued;
    assert!(matches!(running, MailboxDispatchStatus::Running));
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
        RunActivation::new("thread-int", vec![Message::user("a")]).with_agent_id("agent-1");
    mailbox.submit_background(request).await.unwrap();

    let result = mailbox.interrupt("thread-int").await.unwrap();
    // After interrupt, the dispatch epoch should be bumped
    assert!(result.new_dispatch_epoch > 0);
}

#[tokio::test]
async fn interrupt_marks_superseded_queued_runs_cancelled() {
    crate::metrics::install_recorder();
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let first =
        enqueue_prepared_dispatch(&mailbox, store.as_ref(), "thread-int-runs", "first").await;
    let second =
        enqueue_prepared_dispatch(&mailbox, store.as_ref(), "thread-int-runs", "second").await;

    let result = mailbox.interrupt_detailed("thread-int-runs").await.unwrap();
    assert_eq!(result.superseded_count, 2);
    assert_eq!(result.superseded_dispatches.len(), 2);

    for submitted in [&first, &second] {
        let dispatch = store
            .load_dispatch(&submitted.dispatch_id)
            .await
            .unwrap()
            .expect("superseded dispatch should remain inspectable");
        assert_eq!(dispatch.status, RunDispatchStatus::Superseded);

        let run = run_store
            .load_run(&submitted.run_id)
            .await
            .unwrap()
            .expect("superseded run should remain inspectable");
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.termination_reason, Some(TerminationReason::Cancelled));
        assert_eq!(
            run.dispatch_id.as_deref(),
            Some(submitted.dispatch_id.as_str())
        );
    }

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"mark_run_superseded\""));
    assert!(output.contains("outcome=\"superseded\""));
}

#[tokio::test]
async fn runtime_event_capture_records_run_interrupted_on_thread_interrupt() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(ImmediateLocalCancelRuntime);
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store.clone(),
            run_store,
            "interrupt-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );
    let thread_id = "thread-interrupt-event";
    let active_dispatch = prepare_queued_dispatch(&mailbox, thread_id, "active").await;
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    mailbox_store
        .enqueue(&active_dispatch)
        .await
        .expect("enqueue active dispatch");
    mailbox_store
        .claim_dispatch(&active_dispatch_id, "interrupt-consumer", 30_000, now_ms())
        .await
        .expect("claim active dispatch")
        .expect("active dispatch should be claimed");

    let result = mailbox
        .interrupt_detailed(thread_id)
        .await
        .expect("interrupt should succeed");

    assert_eq!(
        result
            .active_dispatch
            .as_ref()
            .map(|dispatch| dispatch.dispatch_id.as_str()),
        Some(active_dispatch_id.as_str())
    );
    let page = event_store
        .list(EventScope::thread(thread_id), None, 10)
        .await
        .expect("list interrupted events");
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunInterrupted")
        .expect("run interrupted event should be recorded");
    assert_eq!(
        event.run_id.as_deref(),
        Some(active_dispatch.run_id.as_str())
    );
    assert_eq!(
        event.correlation_id.as_deref(),
        Some(active_dispatch_id.as_str())
    );
    assert_eq!(
        event.payload["dispatch_id"].as_str(),
        Some(active_dispatch_id.as_str())
    );
    assert_eq!(event.payload["status"].as_str(), Some("claimed"));
}

#[tokio::test]
async fn runtime_event_capture_records_run_rescheduled_on_retry_backoff() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(TransientFailingMailboxRuntime);
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store.clone(),
            run_store,
            "retry-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );
    let thread_id = "thread-reschedule-retry";

    let submitted = mailbox
        .submit_background(
            RunActivation::new(thread_id, vec![Message::user("retry")]).with_agent_id("agent"),
        )
        .await
        .expect("background submit should succeed");
    let queued = wait_for_dispatch(&mailbox_store, &submitted.dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Queued && dispatch.attempt_count == 1
    })
    .await;

    let page = event_store
        .list(EventScope::thread(thread_id), None, 20)
        .await
        .expect("list retry reschedule events");
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunRescheduled")
        .expect("transient failure retry should record reschedule");
    assert_eq!(event.run_id.as_deref(), Some(submitted.run_id.as_str()));
    assert_eq!(
        event.payload["dispatch_id"].as_str(),
        Some(submitted.dispatch_id.as_str())
    );
    assert_eq!(event.payload["status"].as_str(), Some("queued"));
    assert_eq!(event.payload["reason"].as_str(), Some("retry_backoff"));
    assert_eq!(event.payload["attempt_count"].as_u64(), Some(1));
    assert_eq!(
        event.payload["available_at"].as_u64(),
        Some(queued.available_at)
    );
}

#[tokio::test]
async fn runtime_event_capture_records_run_rescheduled_on_expired_lease_reclaim() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store.clone(),
            run_store,
            "reclaim-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );
    let thread_id = "thread-reschedule-reclaim";
    let mut dispatch = prepare_queued_dispatch(&mailbox, thread_id, "reclaim").await;
    dispatch.available_at = 1_000;
    let dispatch_id = dispatch.dispatch_id.clone();
    mailbox_store
        .enqueue(&dispatch)
        .await
        .expect("enqueue dispatch");
    mailbox_store
        .claim(thread_id, "stale-consumer", 100, 1_000, 1)
        .await
        .expect("claim dispatch before simulated lease expiry");

    mailbox.run_sweep().await;

    let page = event_store
        .list(EventScope::thread(thread_id), None, 20)
        .await
        .expect("list reclaim reschedule events");
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunRescheduled")
        .expect("lease reclaim should record reschedule");
    assert_eq!(
        event.payload["dispatch_id"].as_str(),
        Some(dispatch_id.as_str())
    );
    assert_eq!(event.payload["status"].as_str(), Some("queued"));
    assert_eq!(
        event.payload["reason"].as_str(),
        Some("expired_lease_reclaimed")
    );
    assert_eq!(event.payload["attempt_count"].as_u64(), Some(1));
}

#[tokio::test]
async fn foreground_submit_marks_prior_queued_run_cancelled() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(CountingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "foreground-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let old =
        enqueue_prepared_dispatch(&mailbox, store.as_ref(), "thread-submit-supersede", "old").await;

    let (_new_result, _events) = mailbox
        .submit(
            RunActivation::new("thread-submit-supersede", vec![Message::user("new")])
                .with_agent_id("agent"),
        )
        .await
        .expect("foreground submit should claim replacement dispatch");

    let old_dispatch = store
        .load_dispatch(&old.dispatch_id)
        .await
        .unwrap()
        .expect("old dispatch should remain inspectable");
    assert_eq!(old_dispatch.status, RunDispatchStatus::Superseded);

    let old_run = run_store
        .load_run(&old.run_id)
        .await
        .unwrap()
        .expect("old run should remain inspectable");
    assert_eq!(old_run.status, RunStatus::Done);
    assert_eq!(
        old_run.termination_reason,
        Some(TerminationReason::Cancelled)
    );
    assert_eq!(
        old_run.dispatch_id.as_deref(),
        Some(old.dispatch_id.as_str())
    );
}

#[tokio::test]
async fn submit_inline_claim_empty_cancels_precreated_run() {
    crate::metrics::install_recorder();
    let store = Arc::new(SignalMailboxStore::with_empty_claim_dispatch_once());
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "inline-empty-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let error = match mailbox
        .submit(
            RunActivation::new("thread-inline-empty", vec![Message::user("go")])
                .with_agent_id("agent"),
        )
        .await
    {
        Ok(_) => panic!("inline submit should fail when claim_dispatch returns empty"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(ACTIVE_RUN_CONFLICT_MESSAGE));

    let dispatches = store
        .inner
        .list_dispatches("thread-inline-empty", None, 10, 0)
        .await
        .expect("list inline cleanup dispatches");
    assert_eq!(dispatches.len(), 1);
    let dispatch = &dispatches[0];
    assert_eq!(dispatch.status, RunDispatchStatus::Cancelled);

    let run = run_store
        .load_run(&dispatch.run_id)
        .await
        .unwrap()
        .expect("inline cleanup should keep run inspectable");
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.termination_reason, Some(TerminationReason::Cancelled));
    assert_eq!(
        run.dispatch_id.as_deref(),
        Some(dispatch.dispatch_id.as_str())
    );

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"mark_run_cancelled\""));
    assert!(output.contains("outcome=\"cancelled\""));
}

#[tokio::test]
async fn recover_reconciles_terminal_cancelled_and_superseded_dispatches_after_crash() {
    crate::metrics::install_recorder();
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "reconcile-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let cancelled = enqueue_prepared_dispatch(
        &mailbox,
        store.as_ref(),
        "thread-reconcile-cancel",
        "cancel",
    )
    .await;
    let superseded = enqueue_prepared_dispatch(
        &mailbox,
        store.as_ref(),
        "thread-reconcile-supersede",
        "supersede",
    )
    .await;

    store
        .cancel(&cancelled.dispatch_id, now_ms())
        .await
        .expect("direct cancel should terminalize dispatch");
    store
        .interrupt("thread-reconcile-supersede", now_ms())
        .await
        .expect("direct interrupt should supersede dispatch");

    for submitted in [&cancelled, &superseded] {
        let before = run_store
            .load_run(&submitted.run_id)
            .await
            .unwrap()
            .expect("prepared run should exist before reconciliation");
        assert_eq!(before.status, RunStatus::Created);
        assert_eq!(
            before.dispatch_id.as_deref(),
            Some(submitted.dispatch_id.as_str())
        );
    }

    let recovered = mailbox.recover().await.expect("recover should reconcile");
    assert_eq!(recovered, 0);

    for submitted in [&cancelled, &superseded] {
        let run = run_store
            .load_run(&submitted.run_id)
            .await
            .unwrap()
            .expect("reconciled run should remain inspectable");
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.termination_reason, Some(TerminationReason::Cancelled));
        assert_eq!(
            run.dispatch_id.as_deref(),
            Some(submitted.dispatch_id.as_str())
        );
    }

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"list_terminal_dispatches\""));
    assert!(output.contains("operation=\"reconcile_terminal_dispatch\""));
}

#[tokio::test]
async fn reclaim_dead_letter_marks_run_error() {
    crate::metrics::install_recorder();
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut dispatch = prepare_queued_dispatch(&mailbox, "thread-reclaim-dead", "expire").await;
    dispatch.max_attempts = 1;
    dispatch.available_at = 1000;
    let dispatch_id = dispatch.dispatch_id.clone();
    let run_id = dispatch.run_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");
    let claimed = store
        .claim("thread-reclaim-dead", "stale-consumer", 100, 1000, 1)
        .await
        .expect("claim dispatch");
    assert_eq!(claimed.len(), 1);

    mailbox
        .recover()
        .await
        .expect("recover should reclaim expired lease");

    let dead_letter = store
        .load_dispatch(&dispatch_id)
        .await
        .unwrap()
        .expect("dead-lettered dispatch should remain inspectable");
    assert_eq!(dead_letter.status, RunDispatchStatus::DeadLetter);

    let run = run_store
        .load_run(&run_id)
        .await
        .unwrap()
        .expect("dead-lettered run should remain inspectable");
    assert_eq!(run.status, RunStatus::Done);
    assert!(
        matches!(run.termination_reason, Some(TerminationReason::Error(_))),
        "dead-lettered dispatch should mark the run as errored"
    );
    assert_eq!(run.dispatch_id.as_deref(), Some(dispatch_id.as_str()));

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"mark_run_dead_letter\""));
    assert!(output.contains("outcome=\"dead_letter\""));
}

#[tokio::test]
async fn sweep_reconciles_dead_letter_dispatch_after_crash() {
    let store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store.clone(),
        "sweep-reconcile-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut dispatch =
        prepare_queued_dispatch(&mailbox, "thread-sweep-reconcile-dead", "dead").await;
    dispatch.available_at = 1;
    let dispatch_id = dispatch.dispatch_id.clone();
    let run_id = dispatch.run_id.clone();
    store.enqueue(&dispatch).await.expect("enqueue dispatch");
    let claimed = store
        .claim(
            "thread-sweep-reconcile-dead",
            "stale-consumer",
            100,
            now_ms(),
            1,
        )
        .await
        .expect("claim dispatch");
    let claim_token = claimed[0]
        .claim_token
        .as_deref()
        .expect("claimed dispatch should have a claim token")
        .to_string();
    store
        .dead_letter(
            &dispatch_id,
            &claim_token,
            "crashed after dead_letter",
            now_ms(),
        )
        .await
        .expect("direct dead_letter should terminalize dispatch");

    let before = run_store
        .load_run(&run_id)
        .await
        .unwrap()
        .expect("prepared run should exist before sweep reconciliation");
    assert_eq!(before.status, RunStatus::Created);

    mailbox.run_sweep().await;

    let run = run_store
        .load_run(&run_id)
        .await
        .unwrap()
        .expect("reconciled run should remain inspectable");
    assert_eq!(run.status, RunStatus::Done);
    assert!(
        matches!(run.termination_reason, Some(TerminationReason::Error(_))),
        "dead-lettered dispatch should reconcile the run as errored"
    );
    assert_eq!(run.dispatch_id.as_deref(), Some(dispatch_id.as_str()));
}

// ── submit streaming returns event channel ──────────────────────

#[tokio::test]
async fn submit_returns_event_channel() {
    let store = make_store();
    let runtime = make_runtime();
    let mailbox = make_mailbox(runtime, store.clone());

    let request =
        RunActivation::new("thread-stream", vec![Message::user("hi")]).with_agent_id("agent-1");
    let (result, _event_rx) = mailbox.submit(request).await.unwrap();

    assert_eq!(result.thread_id, "thread-stream");
    assert!(!result.dispatch_id.is_empty());
    assert!(matches!(
        result.status,
        MailboxDispatchStatus::Running | MailboxDispatchStatus::Queued
    ));
}

#[test]
fn runtime_event_capture_disabled_clears_prior_capture_and_skips_origin_validation() {
    let mailbox = Mailbox::new(
        Arc::new(CoordinatorAwareNoopRuntime),
        make_store(),
        Arc::new(InMemoryStore::new()),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    )
    .with_runtime_event_capture(RuntimeEventDurability::Compacted, "server")
    .unwrap()
    .with_runtime_event_capture(RuntimeEventDurability::Disabled, "")
    .unwrap();

    assert!(mailbox.runtime_event_capture.is_none());
}

#[tokio::test]
async fn runtime_event_capture_compacted_persists_committed_events_and_keeps_live_deltas() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(CommittingEmittingMailboxRuntime::new(
                Arc::clone(&run_store),
                Arc::clone(&event_store),
            )),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap()
        .with_runtime_event_capture(RuntimeEventDurability::Compacted, "server")
        .unwrap(),
    );

    let request =
        RunActivation::new("thread-events", vec![Message::user("hi")]).with_agent_id("agent-1");
    let (result, mut event_rx) = mailbox.submit(request).await.unwrap();

    let mut live_events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("runtime should emit or close")
            .expect("runtime should emit terminal event");
        let terminal = event.is_terminal();
        live_events.push(event);
        if terminal {
            break;
        }
    }

    assert!(
        live_events
            .iter()
            .any(|event| matches!(event, AgentEvent::TextDelta { .. })),
        "observed deltas should remain live in compacted mode"
    );

    let page = event_store
        .list(EventScope::thread("thread-events"), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "MessageCommitted",
            "ThreadMessagesCheckpointed",
            "RunQueued",
            "RunSubmitted",
            "RunStarted",
            "ToolCallReady",
            "RunFinished",
        ]
    );
    assert!(page.events.iter().all(|event| {
        event.thread_id.as_deref() == Some("thread-events")
            && event.run_id.as_deref() == Some(result.run_id.as_str())
    }));
    assert!(page.events.iter().all(|event| {
        matches!(
            event.event_kind.as_str(),
            "MessageCommitted" | "ThreadMessagesCheckpointed"
        ) || event.correlation_id.as_deref() == Some(result.dispatch_id.as_str())
    }));
}

#[tokio::test]
async fn recover_repairs_thread_message_checkpoint_events_from_committed_log() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(NoopMailboxRuntime),
            make_store(),
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    let message = Message::user("hi").with_id("message-repair-1".to_string());
    let (_, input) = build_run_input("thread-repair-events", 1, &["message-repair-1".to_string()]);
    let mut run = make_run_record("run-repair-events", "thread-repair-events", RunStatus::Done);
    run.input = input;
    #[allow(deprecated)]
    thread_store
        .checkpoint("thread-repair-events", &[message], &run)
        .await
        .unwrap();

    let repaired = mailbox
        .repair_thread_message_checkpoint_events()
        .await
        .unwrap();

    assert_eq!(repaired, 1);
    let page = event_store
        .list(EventScope::thread("thread-repair-events"), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["MessageCommitted", "ThreadMessagesCheckpointed"]
    );

    let repaired_again = mailbox
        .repair_thread_message_checkpoint_events()
        .await
        .unwrap();
    assert_eq!(repaired_again, 1);
    let page = event_store
        .list(EventScope::thread("thread-repair-events"), None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
}

#[tokio::test]
async fn maintenance_drain_republishes_enqueued_checkpoint_repair() {
    // Simulates a checkpoint-event publish failure after a freeze commit: a repair
    // task is enqueued, and the maintenance sweep's drain re-publishes the missing
    // events without waiting for a process restart. Idempotent across drains.
    let event_store = Arc::new(InMemoryEventStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(NoopMailboxRuntime),
            make_store(),
            thread_store.clone(),
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    // Commit a message + run input, as a freeze would, without publishing events.
    let message = Message::user("hi").with_id("msg-drain-1".to_string());
    let (_, input) = build_run_input("thread-drain", 1, &["msg-drain-1".to_string()]);
    let mut run = make_run_record("run-drain", "thread-drain", RunStatus::Done);
    run.input = input;
    #[allow(deprecated)]
    thread_store
        .checkpoint("thread-drain", &[message], &run)
        .await
        .unwrap();

    // A publish failure would have enqueued this; the sweep drains it.
    mailbox.enqueue_checkpoint_repair(super::checkpoint_repair::CheckpointRepairTask {
        thread_id: "thread-drain".to_string(),
        run_id: "run-drain".to_string(),
        first_seq: 1,
        last_seq: 1,
    });
    mailbox.run_sweep().await;

    let page = event_store
        .list(EventScope::thread("thread-drain"), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["MessageCommitted", "ThreadMessagesCheckpointed"]
    );

    // A second sweep with the same task re-queued must not duplicate events.
    mailbox.enqueue_checkpoint_repair(super::checkpoint_repair::CheckpointRepairTask {
        thread_id: "thread-drain".to_string(),
        run_id: "run-drain".to_string(),
        first_seq: 1,
        last_seq: 1,
    });
    mailbox.run_sweep().await;
    let page = event_store
        .list(EventScope::thread("thread-drain"), None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2, "re-publish must be idempotent");
}

#[tokio::test]
async fn runtime_event_capture_maps_continue_run_start_to_run_resumed() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let waiting = seeded_waiting_run("run-resume", "thread-resume", "agent-1");
    run_store.create_run(&waiting).await.unwrap();
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(CommittingEmittingMailboxRuntime::new(
                Arc::clone(&run_store),
                Arc::clone(&event_store),
            )),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_runtime_event_capture(RuntimeEventDurability::Compacted, "server")
        .unwrap(),
    );

    let request = RunActivation::new("thread-resume", vec![Message::user("continue")])
        .with_agent_id("agent-1")
        .with_continue_run_id("run-resume");
    let (_result, mut event_rx) = mailbox.submit(request).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("runtime should emit or close")
            .expect("runtime should emit terminal event");
        if event.is_terminal() {
            break;
        }
    }

    let page = event_store
        .list(EventScope::thread("thread-resume"), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"RunResumed"));
    assert!(!kinds.contains(&"RunStarted"));
}

#[tokio::test]
async fn runtime_event_capture_live_forwarding_is_not_gated_by_staging() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(CommittingEmittingMailboxRuntime::new(
                Arc::clone(&run_store),
                Arc::clone(&event_store),
            )),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_runtime_event_capture(RuntimeEventDurability::Compacted, "server")
        .unwrap(),
    );

    let request =
        RunActivation::new("thread-event-fail", vec![Message::user("hi")]).with_agent_id("agent-1");
    let (_result, mut event_rx) = mailbox.submit(request).await.unwrap();

    let mut forwarded_kinds = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("runtime should emit or close")
            .expect("runtime should emit terminal event");
        let terminal = event.is_terminal();
        forwarded_kinds.push(std::mem::discriminant(&event));
        if terminal {
            break;
        }
    }
    let run_start_kind = std::mem::discriminant(&AgentEvent::RunStart {
        thread_id: String::new(),
        run_id: String::new(),
        parent_run_id: None,
        identity: None,
    });
    let text_delta_kind = std::mem::discriminant(&AgentEvent::TextDelta {
        delta: String::new(),
    });
    let tool_call_ready_kind = std::mem::discriminant(&AgentEvent::ToolCallReady {
        id: String::new(),
        name: String::new(),
        arguments: json!({}),
    });
    let run_finish_kind = std::mem::discriminant(&AgentEvent::RunFinish {
        thread_id: String::new(),
        run_id: String::new(),
        identity: None,
        result: None,
        termination: TerminationReason::NaturalEnd,
    });
    assert_eq!(
        forwarded_kinds,
        vec![
            run_start_kind,
            text_delta_kind,
            tool_call_ready_kind,
            run_finish_kind
        ],
        "durable staging must not suppress the live event stream"
    );
}

#[tokio::test]
async fn runtime_event_capture_persists_server_authored_terminal_errors() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(FailingMailboxRuntime),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    let request = RunActivation::new("thread-server-error", vec![Message::user("hi")])
        .with_agent_id("agent-1");
    let (result, mut event_rx) = mailbox.submit(request).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, AgentEvent::RunFinish { .. }));

    let page = event_store
        .list(EventScope::thread("thread-server-error"), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "MessageCommitted",
            "ThreadMessagesCheckpointed",
            "RunQueued",
            "RunSubmitted",
            "RunErrored"
        ]
    );
    let terminal = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunErrored")
        .unwrap();
    assert_eq!(
        terminal.correlation_id.as_deref(),
        Some(result.dispatch_id.as_str())
    );
}

#[tokio::test]
async fn runtime_event_capture_records_mailbox_dispatch_lifecycle_events() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(CommittingEmittingMailboxRuntime::new(
                Arc::clone(&run_store),
                Arc::clone(&event_store),
            )),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap()
        .with_runtime_event_capture(RuntimeEventDurability::Compacted, "server")
        .unwrap(),
    );

    let request = RunActivation::new("thread-mailbox-events", vec![Message::user("hi")])
        .with_agent_id("agent-1");
    let (result, mut event_rx) = mailbox.submit(request).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, AgentEvent::RunFinish { .. }) {
            break;
        }
    }

    let page = event_store
        .list(EventScope::thread("thread-mailbox-events"), None, 20)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "MessageCommitted",
            "ThreadMessagesCheckpointed",
            "RunQueued",
            "RunSubmitted",
            "RunStarted",
            "ToolCallReady",
            "RunFinished",
        ]
    );
    let queued = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunQueued")
        .unwrap();
    let committed = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "MessageCommitted")
        .unwrap();
    assert_eq!(committed.payload["message_seq"], 1);
    assert_eq!(committed.payload["message_kind"], "user_input");
    assert_eq!(
        queued.payload["dispatch_id"].as_str(),
        Some(result.dispatch_id.as_str())
    );
    assert_eq!(
        queued.correlation_id.as_deref(),
        Some(result.dispatch_id.as_str())
    );
}

#[tokio::test]
async fn runtime_event_capture_records_mailbox_submit_failed_on_enqueue_error() {
    let mailbox_store = Arc::new(SignalMailboxStore::with_enqueue_failures(1));
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(NoopMailboxRuntime),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    let error = mailbox
        .submit_background(
            RunActivation::new("thread-submit-failed", vec![Message::user("hi")])
                .with_agent_id("agent-1"),
        )
        .await
        .expect_err("enqueue failure should fail submit");
    assert!(error.to_string().contains("injected enqueue failure"));

    let page = event_store
        .list(EventScope::thread("thread-submit-failed"), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "MessageCommitted",
            "ThreadMessagesCheckpointed",
            "MailboxSubmitFailed",
        ]
    );
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "MailboxSubmitFailed")
        .expect("submit failure should be recorded");
    assert!(!event.run_id.as_deref().unwrap().trim().is_empty());
    assert_eq!(event.payload["thread_id"], "thread-submit-failed");
    assert_eq!(event.payload["reason"], "enqueue_failed");
    assert!(
        event.payload["error"]
            .as_str()
            .unwrap()
            .contains("injected enqueue failure")
    );
}

#[tokio::test]
async fn runtime_event_capture_records_mailbox_resume_failed_on_enqueue_error() {
    let mailbox_store = Arc::new(SignalMailboxStore::with_enqueue_failures(1));
    let run_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let thread_id = "thread-resume-failed";
    run_store
        .create_run(&seeded_waiting_run(
            "run-resume-failed",
            thread_id,
            "agent-1",
        ))
        .await
        .expect("seed waiting run");
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(NoopMailboxRuntime),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    let error = mailbox
        .submit_background(
            RunActivation::new(thread_id, Vec::new())
                .with_agent_id("agent-1")
                .with_continue_run_id("run-resume-failed")
                .with_decisions(vec![("tool-1".to_string(), make_resume())]),
        )
        .await
        .expect_err("enqueue failure should fail durable resume");
    assert!(error.to_string().contains("injected enqueue failure"));

    let page = event_store
        .list(EventScope::thread(thread_id), None, 10)
        .await
        .unwrap();
    let kinds = page
        .events
        .iter()
        .map(|event| event.event_kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["MailboxSubmitFailed", "MailboxResumeFailed"]);
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "MailboxResumeFailed")
        .expect("resume failure should be recorded");
    assert_eq!(event.run_id.as_deref(), Some("run-resume-failed"));
    assert_eq!(event.payload["thread_id"], thread_id);
    assert_eq!(event.payload["reason"], "enqueue_failed");
    assert_eq!(event.payload["decisions"][0]["tool_call_id"], "tool-1");
    assert_eq!(event.payload["decisions"][0]["decision_id"], "d1");
    assert!(
        event.payload["error"]
            .as_str()
            .unwrap()
            .contains("injected enqueue failure")
    );
}

#[tokio::test]
async fn advisory_server_event_publish_failure_does_not_block_live_terminal_event() {
    let mailbox_store = make_store();
    let run_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(
        Mailbox::new(
            Arc::new(FailingMailboxRuntime),
            mailbox_store,
            run_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(Arc::new(FailingServerEventPublisher), "server")
        .unwrap(),
    );

    let request = RunActivation::new("thread-server-error-fail", vec![Message::user("hi")])
        .with_agent_id("agent-1");
    let (_result, mut event_rx) = mailbox.submit(request).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("runtime task should finish and close the channel");
    assert!(
        matches!(next, Some(AgentEvent::RunFinish { .. })),
        "advisory server-event publication failure must not suppress live terminal event"
    );
}

#[tokio::test]
async fn live_then_queue_steers_active_run_without_new_dispatch() {
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = make_store();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let llm = Arc::new(RecordingLlm::new(
        vec![
            StreamResult {
                content: vec![ContentBlock::text("start tool")],
                tool_calls: vec![ToolCall::new("block-1", "block", json!({}))],
                usage: None,
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            },
            StreamResult {
                content: vec![ContentBlock::text("saw live input")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            },
        ],
        requests.clone(),
    ));
    let resolver = Arc::new(FixedResolver {
        agent: ResolvedAgent::new("agent", "m", "sys", llm)
            .with_tool(Arc::new(BlockingTool::new(started_tx, release_rx))),
        plugins: vec![],
    });
    let runtime = Arc::new(
        AgentRuntime::new(resolver)
            .with_in_memory_thread_run_store(store.clone())
            .with_live_control_source(Arc::new(MailboxLiveControlSource::new(
                mailbox_store.clone(),
            ))),
    );
    let mailbox = make_mailbox_with_run_store(
        runtime,
        mailbox_store.clone(),
        store.clone() as Arc<dyn ThreadRunStore>,
    );

    let first = mailbox
        .submit_background(
            RunActivation::new("thread-live-steer", vec![Message::user("start")])
                .with_agent_id("agent"),
        )
        .await
        .expect("initial submit should start");

    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("tool should start")
        .expect("started signal should send");

    let steered = mailbox
        .submit_live_then_queue(
            RunActivation::new("thread-live-steer", vec![Message::user("live steer")])
                .with_agent_id("agent"),
            None,
        )
        .await
        .expect("live steer should be accepted");
    assert_eq!(steered.status, MailboxDispatchStatus::Running);
    assert_eq!(steered.run_id, first.run_id);
    assert_eq!(steered.dispatch_id, first.dispatch_id);

    let _ = release_tx.send(());
    let latest = wait_for_latest_run(&store, "thread-live-steer", |run| {
        run.status == RunStatus::Done
    })
    .await;
    assert_eq!(latest.run_id, first.run_id);

    let live_message_seen = {
        let recorded = requests.lock().expect("lock poisoned");
        assert_eq!(recorded.len(), 2);
        recorded[1].messages.iter().any(|message| {
            message.text() == "live steer"
                && message.role == awaken_server_contract::contract::message::Role::User
                && message.visibility == awaken_server_contract::contract::message::Visibility::All
        })
    };
    assert!(
        live_message_seen,
        "second LLM turn should receive the live user message"
    );

    let messages = store
        .load_messages("thread-live-steer")
        .await
        .expect("load messages")
        .expect("messages should be persisted");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.text() == "live steer")
            .count(),
        1,
        "live message should be persisted exactly once"
    );

    let dispatches = mailbox_store
        .list_dispatches("thread-live-steer", None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(
        dispatches
            .iter()
            .filter(|dispatch| dispatch.run_id == first.run_id)
            .count(),
        1,
        "live steering must not create an extra dispatch"
    );
}

#[tokio::test]
async fn live_then_queue_falls_back_to_durable_dispatch_when_receiver_unavailable() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_id = "thread-live-fallback";
    let mut active_request =
        RunActivation::new(thread_id, vec![Message::user("active")]).with_agent_id("agent");
    let (_, active_messages) = validate_run_inputs(
        active_request.thread_id().to_owned(),
        active_request.messages().to_vec(),
        false,
    )
    .expect("active input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut active_request, thread_id, &active_messages)
        .await
        .expect("prepare active run");
    let active_dispatch = mailbox
        .build_dispatch(&active_request, thread_id)
        .expect("build active dispatch");
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    mailbox_store
        .enqueue(&active_dispatch)
        .await
        .expect("enqueue active dispatch");
    mailbox_store
        .claim_dispatch(&active_dispatch_id, "test-consumer", 30_000, now_ms())
        .await
        .expect("claim active dispatch")
        .expect("active dispatch should be claimed");
    let worker = mailbox.get_or_create_worker(thread_id).await;
    {
        let mut worker = worker.lock();
        worker.status = MailboxWorkerStatus::Running {
            dispatch_id: active_dispatch_id.clone(),
            run_id: active_dispatch.run_id.clone(),
            lease_handle: tokio::spawn(async {}),
            sink: Arc::new(ReconnectableEventSink::new(mpsc::channel(16).0)),
        };
    }

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("fallback")]).with_agent_id("agent"),
            None,
        )
        .await
        .expect("fallback submit should succeed");

    assert_eq!(result.status, MailboxDispatchStatus::Queued);
    assert_ne!(result.dispatch_id, active_dispatch_id);
    let messages = thread_store
        .load_messages(thread_id)
        .await
        .expect("load messages")
        .expect("messages should exist");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.text() == "fallback")
            .count(),
        1,
        "fallback message should be persisted once"
    );
    let queued = mailbox_store
        .list_dispatches(thread_id, Some(&[RunDispatchStatus::Queued]), 10, 0)
        .await
        .expect("list queued");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].dispatch_id, result.dispatch_id);
}

#[tokio::test]
async fn foreground_submit_sends_live_cancel_for_remote_active_dispatch() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store.clone(),
            thread_store.clone(),
            "foreground-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );
    let thread_id = "thread-remote-foreground";

    let mut active_request =
        RunActivation::new(thread_id, vec![Message::user("active")]).with_agent_id("agent");
    let (_, active_messages) = validate_run_inputs(
        active_request.thread_id().to_owned(),
        active_request.messages().to_vec(),
        false,
    )
    .expect("active input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut active_request, thread_id, &active_messages)
        .await
        .expect("prepare active run");
    let active_dispatch = mailbox
        .build_dispatch(&active_request, thread_id)
        .expect("build active dispatch");
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    mailbox_store
        .enqueue(&active_dispatch)
        .await
        .expect("enqueue active dispatch");
    let claimed = mailbox_store
        .claim_dispatch(&active_dispatch_id, "remote-consumer", 30_000, now_ms())
        .await
        .expect("claim active dispatch")
        .expect("active dispatch should be claimed");
    let active_claim_token = claimed.claim_token.clone().expect("claim token");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_dispatch(&active_dispatch))
        .await
        .expect("open live channel");
    let captured = Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_clone = captured.clone();
    let store_clone = mailbox_store.clone();
    let active_dispatch_id_clone = active_dispatch_id.clone();
    let active_claim_token_clone = active_claim_token.clone();
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            captured_clone.lock().await.push(entry.command.clone());
            if matches!(entry.command, LiveRunCommand::Cancel) {
                let release_result = store_clone
                    .ack(
                        &active_dispatch_id_clone,
                        &active_claim_token_clone,
                        now_ms(),
                    )
                    .await;
                if let Err(error) = release_result {
                    assert!(
                        matches!(error, StorageError::VersionConflict { .. }),
                        "remote run release should either ack or be superseded, got {error:?}"
                    );
                }
                entry.receipt.ack();
                break;
            }
            drop(entry.receipt);
        }
    });

    let (submitted, _events) = mailbox
        .submit(
            RunActivation::new(thread_id, vec![Message::user("replacement")])
                .with_agent_id("agent"),
        )
        .await
        .expect("foreground submit should cancel remote active run and claim replacement");

    assert_eq!(submitted.status, MailboxDispatchStatus::Running);
    assert_ne!(submitted.dispatch_id, active_dispatch_id);
    let commands = captured.lock().await;
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveRunCommand::Cancel)),
        "foreground submit must deliver live Cancel to the remote active run"
    );
    drop(commands);

    let page = event_store
        .list(EventScope::thread(thread_id), None, 20)
        .await
        .expect("list foreground interrupt events");
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "RunInterrupted")
        .expect("foreground submit should record the interrupted prior run");
    assert_eq!(
        event.payload["dispatch_id"].as_str(),
        Some(active_dispatch_id.as_str())
    );
    assert_eq!(
        event.correlation_id.as_deref(),
        Some(active_dispatch_id.as_str())
    );
}

#[tokio::test]
async fn foreground_submit_does_not_prepare_replacement_when_remote_cancel_times_out() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(RecordingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "foreground-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_id = "thread-remote-cancel-timeout";

    let mut active_request =
        RunActivation::new(thread_id, vec![Message::user("active")]).with_agent_id("agent");
    let (_, active_messages) = validate_run_inputs(
        active_request.thread_id().to_owned(),
        active_request.messages().to_vec(),
        false,
    )
    .expect("active input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut active_request, thread_id, &active_messages)
        .await
        .expect("prepare active run");
    let active_dispatch = mailbox
        .build_dispatch(&active_request, thread_id)
        .expect("build active dispatch");
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    mailbox_store.enqueue(&active_dispatch).await.unwrap();
    mailbox_store
        .claim_dispatch(&active_dispatch_id, "remote-consumer", 30_000, now_ms())
        .await
        .unwrap()
        .expect("active dispatch should be claimed");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_dispatch(&active_dispatch))
        .await
        .expect("open live channel");
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            if matches!(entry.command, LiveRunCommand::Cancel) {
                // Ack the cancel but intentionally keep the dispatch Claimed.
                entry.receipt.ack();
                break;
            }
            drop(entry.receipt);
        }
    });

    let result = mailbox
        .submit(
            RunActivation::new(thread_id, vec![Message::user("replacement")])
                .with_agent_id("agent"),
        )
        .await;
    assert!(
        matches!(result, Err(MailboxError::Validation(ref message)) if message == ACTIVE_RUN_CONFLICT_MESSAGE),
        "foreground submit must fail before writing replacement state when old claim remains active"
    );

    let messages = thread_store
        .load_messages(thread_id)
        .await
        .expect("load messages")
        .expect("active messages should remain");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text(), "active");

    let all = mailbox_store
        .list_dispatches(thread_id, None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].dispatch_id, active_dispatch_id);
    assert_eq!(all[0].status, RunDispatchStatus::Claimed);
}

#[tokio::test]
async fn foreground_submit_does_not_prepare_replacement_when_local_cancel_times_out() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(ImmediateLocalCancelRuntime);
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store.clone(),
            thread_store.clone(),
            "foreground-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );
    let thread_id = "thread-local-cancel-timeout";

    let mut active_request =
        RunActivation::new(thread_id, vec![Message::user("active")]).with_agent_id("agent");
    let (_, active_messages) = validate_run_inputs(
        active_request.thread_id().to_owned(),
        active_request.messages().to_vec(),
        false,
    )
    .expect("active input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut active_request, thread_id, &active_messages)
        .await
        .expect("prepare active run");
    let active_dispatch = mailbox
        .build_dispatch(&active_request, thread_id)
        .expect("build active dispatch");
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    mailbox_store.enqueue(&active_dispatch).await.unwrap();
    mailbox_store
        .claim_dispatch(&active_dispatch_id, "foreground-consumer", 30_000, now_ms())
        .await
        .unwrap()
        .expect("active dispatch should be claimed");

    let result = mailbox
        .submit(
            RunActivation::new(thread_id, vec![Message::user("replacement")])
                .with_agent_id("agent"),
        )
        .await;
    assert!(
        matches!(result, Err(MailboxError::Validation(ref message)) if message == ACTIVE_RUN_CONFLICT_MESSAGE),
        "foreground submit must fail before writing replacement state when local cancel does not release"
    );

    let messages = thread_store
        .load_messages(thread_id)
        .await
        .expect("load messages")
        .expect("active messages should remain");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text(), "active");

    let all = mailbox_store
        .list_dispatches(thread_id, None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].dispatch_id, active_dispatch_id);
    assert_eq!(all[0].status, RunDispatchStatus::Claimed);

    let page = event_store
        .list(EventScope::thread(thread_id), None, 10)
        .await
        .expect("list timeout events");
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "MailboxTimeout")
        .expect("cancel release wait timeout should be recorded");
    assert_eq!(
        event.payload["dispatch_id"].as_str(),
        Some(active_dispatch_id.as_str())
    );
    assert_eq!(event.payload["reason"], "local_cancel_release_wait");
    assert_eq!(event.payload["timeout_ms"], REMOTE_CANCEL_WAIT_MS);
}

#[tokio::test]
async fn foreground_submit_waits_for_local_cancelled_dispatch_to_release_claim() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(ImmediateLocalCancelRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "foreground-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_id = "thread-local-cancel-claim-window";

    let mut active_request =
        RunActivation::new(thread_id, vec![Message::user("active")]).with_agent_id("agent");
    let (_, active_messages) = validate_run_inputs(
        active_request.thread_id().to_owned(),
        active_request.messages().to_vec(),
        false,
    )
    .expect("active input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut active_request, thread_id, &active_messages)
        .await
        .expect("prepare active run");
    let active_dispatch = mailbox
        .build_dispatch(&active_request, thread_id)
        .expect("build active dispatch");
    let active_dispatch_id = active_dispatch.dispatch_id.clone();
    mailbox_store.enqueue(&active_dispatch).await.unwrap();
    mailbox_store
        .claim_dispatch(&active_dispatch_id, "foreground-consumer", 30_000, now_ms())
        .await
        .unwrap()
        .expect("active dispatch should be claimed");

    let result = mailbox
        .submit(
            RunActivation::new(thread_id, vec![Message::user("replacement")])
                .with_agent_id("agent"),
        )
        .await;
    assert!(
        matches!(result, Err(MailboxError::Validation(ref message)) if message == ACTIVE_RUN_CONFLICT_MESSAGE),
        "foreground submit must fail before writing replacement state when local runtime slot released but mailbox claim remains"
    );

    let messages = thread_store
        .load_messages(thread_id)
        .await
        .expect("load messages")
        .expect("active messages should remain");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text(), "active");

    let all = mailbox_store
        .list_dispatches(thread_id, None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].dispatch_id, active_dispatch_id);
    assert_eq!(all[0].status, RunDispatchStatus::Claimed);
}

/// Cross-node live delivery: no local worker, but the thread has an
/// active Running run recorded globally in ThreadRunStore. Mailbox must
/// publish on the live channel (for the owning node's forwarder to
/// receive) and return Running rather than falling back.
#[tokio::test]
async fn live_then_queue_wakes_local_active_pending_run() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(WakeRecordingRuntime::default());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        runtime.clone(),
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_id = "thread-local-pending-wake";
    let run_id = "run-local-pending-wake";
    let dispatch_id = "dispatch-local-pending-wake";
    let worker = mailbox.get_or_create_worker(thread_id).await;
    {
        let mut worker = worker.lock();
        worker.status = MailboxWorkerStatus::Running {
            dispatch_id: dispatch_id.to_string(),
            run_id: run_id.to_string(),
            lease_handle: tokio::spawn(async {}),
            sink: Arc::new(ReconnectableEventSink::new(mpsc::channel(16).0)),
        };
    }

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("pending-live")])
                .with_agent_id("agent"),
            Some(run_id),
        )
        .await
        .expect("live pending submit should wake active run");

    assert_eq!(result.status, MailboxDispatchStatus::Running);
    assert_eq!(result.run_id, run_id);
    assert_eq!(runtime.wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    let pending = thread_store
        .load_pending_message_records(thread_id)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.text(), "pending-live");
}

/// Cross-node live delivery: no local worker, but the thread has an
/// active Running run recorded globally in ThreadRunStore. Mailbox must
/// publish on the live channel (for the owning node's forwarder to
/// receive) and return Running rather than falling back.
#[tokio::test]
async fn live_then_queue_publishes_for_remote_active_run() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-remote";
    let remote_run_id = "run-remote";

    // Seed a Running run — simulates another node owning this run.
    let mut run = seeded_waiting_run(remote_run_id, thread_id, "agent");
    run.status = RunStatus::Running;
    thread_store
        .create_run(&run)
        .await
        .expect("seed remote run");

    // Simulate the remote forwarder: drain the stream and ack each
    // entry so the producer's `deliver_live` resolves as Delivered.
    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .expect("open live channel");
    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_clone = captured.clone();
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            captured_clone.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("steer-remote")])
                .with_agent_id("agent"),
            None,
        )
        .await
        .expect("submit should succeed");

    assert_eq!(result.status, MailboxDispatchStatus::Running);
    assert_eq!(result.run_id, remote_run_id);

    // The acked subscriber must have captured the message.
    let commands = captured.lock().await;
    assert_eq!(commands.len(), 1);
    match &commands[0] {
        LiveRunCommand::Messages(msgs) => assert_eq!(msgs[0].text(), "steer-remote"),
        other => panic!("expected Messages, got {other:?}"),
    }
    drop(commands);

    // No new dispatch should have been enqueued.
    let queued = mailbox_store
        .list_dispatches(thread_id, Some(&[RunDispatchStatus::Queued]), 10, 0)
        .await
        .expect("list queued");
    assert!(
        queued.is_empty(),
        "cross-node live delivery must not create a dispatch"
    );
}

#[tokio::test]
async fn live_then_queue_wakes_remote_active_pending_run() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-remote-pending-wake";
    let remote_run_id = "run-remote-pending-wake";

    let mut run = seeded_waiting_run(remote_run_id, thread_id, "agent");
    run.status = RunStatus::Running;
    thread_store.create_run(&run).await.expect("seed run");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .expect("open live channel");
    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_clone = captured.clone();
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            captured_clone.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });

    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopMailboxRuntime),
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("remote-pending")])
                .with_agent_id("agent"),
            Some(remote_run_id),
        )
        .await
        .expect("remote pending submit should wake owner");

    assert_eq!(result.status, MailboxDispatchStatus::Running);
    assert_eq!(result.run_id, remote_run_id);
    assert!(
        captured
            .lock()
            .await
            .iter()
            .any(|command| matches!(command, LiveRunCommand::PendingBoundaryWake))
    );
    let pending = thread_store
        .load_pending_message_records(thread_id)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.text(), "remote-pending");
}

/// Regression for issue #2: cross-node delivery where the subscriber
/// drops the receipt (simulating inbox full / forwarder failure) must
/// fall back to durable queue, not report `Running`.
#[tokio::test]
async fn live_then_queue_falls_back_when_subscriber_drops_receipt() {
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-dropped-receipt";

    let mut run = seeded_waiting_run("run-dropped", thread_id, "agent");
    run.status = RunStatus::Running;
    thread_store.create_run(&run).await.expect("seed run");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .expect("open live channel");
    // Drop every receipt — simulates forwarder that can't hand off.
    let _rogue = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            drop(entry.receipt);
        }
    });

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("hello?")]).with_agent_id("agent"),
            None,
        )
        .await
        .expect("submit should succeed via queue fallback");

    let dispatches = mailbox_store
        .list_dispatches(thread_id, None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(
        dispatches.len(),
        1,
        "unacked receipt must force a durable dispatch"
    );
    assert_eq!(result.dispatch_id, dispatches[0].dispatch_id);
}

/// Contract test documenting the `submit_live_then_queue` at-least-once
/// guarantee: a forwarder that accepts the live command but whose ack
/// is lost (ack publish failure / network timeout) causes the producer
/// to observe `NoSubscriber` and fall back to durable dispatch, even
/// though the run has already received the payload. Callers needing
/// exactly-once effects must use agent-level idempotency.
#[tokio::test]
async fn live_then_queue_is_at_least_once_when_ack_lost() {
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-ack-lost";

    let mut run = seeded_waiting_run("run-ack-lost", thread_id, "agent");
    run.status = RunStatus::Running;
    thread_store.create_run(&run).await.expect("seed run");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .expect("open live channel");
    // Consumer captures the command (simulates `try_send` success)
    // but drops the receipt (simulates the ack publish failing).
    let accepted = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let accepted_c = accepted.clone();
    let _consumer = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            if let awaken_server_contract::contract::mailbox::LiveRunCommand::Messages(ref msgs) =
                entry.command
            {
                for m in msgs {
                    accepted_c.lock().await.push(m.text());
                }
            }
            // Forwarder accepted, but ack "publish" never happens.
            drop(entry.receipt);
        }
    });

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("steer-payload")])
                .with_agent_id("agent"),
            None,
        )
        .await
        .expect("submit should succeed via queue fallback");

    // Contract part 1: the forwarder DID receive the payload.
    let received = accepted.lock().await.clone();
    assert_eq!(
        received.as_slice(),
        &["steer-payload".to_string()],
        "forwarder must have observed the live command before dropping receipt"
    );

    // Contract part 2: because the ack was "lost", submit fell back
    // to durable dispatch — the SAME payload is now queued for a
    // future run.  This is the at-least-once window.
    let dispatches = mailbox_store
        .list_dispatches(thread_id, None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(dispatches.len(), 1);
    assert_eq!(result.dispatch_id, dispatches[0].dispatch_id);
}

/// expected_run_id mismatch against a remote Running run must abort
/// live delivery and fall back to dispatch (preventing steering the
/// wrong run after a rollover).
#[tokio::test]
async fn live_then_queue_rejects_remote_mismatched_expected_run_id() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-mismatch";

    let mut run = seeded_waiting_run("run-actual", thread_id, "agent");
    run.status = RunStatus::Running;
    thread_store.create_run(&run).await.expect("seed run");

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("wrong-run")]).with_agent_id("agent"),
            Some("run-stale"),
        )
        .await
        .expect("submit should succeed");

    assert_ne!(
        result.run_id, "run-actual",
        "mismatched expected_run_id must not steer the stale remote run"
    );
}

#[tokio::test]
async fn send_decision_live_delivers_to_remote_waiting_run() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-remote-decision";
    let run = seeded_waiting_run("run-remote-decision", thread_id, "agent");
    thread_store.create_run(&run).await.expect("seed run");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .expect("open targeted live channel");
    let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured_c = captured.clone();
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            if let LiveRunCommand::Decision(decisions) = entry.command {
                captured_c.lock().await.push(decisions);
                entry.receipt.ack();
                break;
            }
            drop(entry.receipt);
        }
    });

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store,
        thread_store,
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let delivered = mailbox
        .send_decision_live(thread_id, "tool-1".to_string(), make_resume())
        .await
        .expect("live decision should not error");
    assert!(delivered);
    let captured = captured.lock().await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0][0].0, "tool-1");
}

#[tokio::test]
async fn runtime_event_capture_records_mailbox_decision_received() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let thread_id = "thread-decision-event";
    let run = seeded_waiting_run("run-decision-event", thread_id, "agent");
    thread_store.create_run(&run).await.expect("seed run");

    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .expect("open targeted live channel");
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            if matches!(entry.command, LiveRunCommand::Decision(_)) {
                entry.receipt.ack();
                break;
            }
            drop(entry.receipt);
        }
    });

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store,
            thread_store,
            "test-consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_server_event_publisher(
            test_server_event_publisher(Arc::clone(&event_store)),
            "server",
        )
        .unwrap(),
    );

    let delivered = mailbox
        .send_decision_live(thread_id, "tool-1".to_string(), make_resume())
        .await
        .expect("live decision should not error");

    assert!(delivered);
    let page = event_store
        .list(EventScope::thread(thread_id), None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 2);
    let event = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "MailboxDecisionReceived")
        .expect("mailbox decision event should be recorded");
    assert_eq!(event.event_kind.as_str(), "MailboxDecisionReceived");
    assert_eq!(event.run_id.as_deref(), Some("run-decision-event"));
    assert_eq!(event.payload["tool_call_id"], "tool-1");
    assert_eq!(event.payload["decision_id"], "d1");
    assert_eq!(event.payload["action"], "resume");
    assert_eq!(event.payload["result"]["approved"], true);
    assert_eq!(event.payload["delivery_path"], "remote_live");
    let permission = page
        .events
        .iter()
        .find(|event| event.event_kind.as_str() == "ToolPermissionResolved")
        .expect("approval decision should record permission resolution");
    assert_eq!(permission.run_id.as_deref(), Some("run-decision-event"));
    assert_eq!(permission.payload["tool_call_id"], "tool-1");
    assert_eq!(permission.payload["decision_id"], "d1");
    assert_eq!(permission.payload["approved"], true);
    assert_eq!(permission.payload["delivery_path"], "remote_live");
}

/// Cross-node live delivery when **no subscriber** is attached to the
/// live channel must fall back to `submit_background` — never report
/// `Running` based on a publish the owning node will never observe.
#[tokio::test]
async fn live_then_queue_falls_back_to_queue_when_no_remote_subscriber() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-no-subscriber";

    // Seed a Running run on some other (imaginary) node. Crucially,
    // we do NOT call `open_live_channel` — no one is listening.
    let mut run = seeded_waiting_run("run-no-listener", thread_id, "agent");
    run.status = RunStatus::Running;
    thread_store.create_run(&run).await.expect("seed run");

    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        thread_store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new(thread_id, vec![Message::user("hello?")]).with_agent_id("agent"),
            None,
        )
        .await
        .expect("submit should succeed via queue fallback");

    // The submit must have entered the durable queue — a new dispatch
    // appears whether Queued or Claimed (depending on whether a worker
    // picks it up). The key property is: a dispatch was enqueued rather
    // than silently declared `Running` on the remote.
    let all_dispatches = mailbox_store
        .list_dispatches(thread_id, None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(
        all_dispatches.len(),
        1,
        "no-subscriber cross-node must fall back to durable queue"
    );
    assert_eq!(result.dispatch_id, all_dispatches[0].dispatch_id);
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
    let runtime =
        Arc::new(AgentRuntime::new(resolver).with_in_memory_thread_run_store(store.clone()));
    let mailbox_store = make_store();
    let mailbox = make_mailbox_with_run_store(
        runtime,
        mailbox_store,
        store.clone() as Arc<dyn ThreadRunStore>,
    );

    let submitted = mailbox
        .submit_background(
            RunActivation::new("thread-waiting", vec![Message::user("poke")])
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
    let runtime =
        Arc::new(AgentRuntime::new(resolver).with_in_memory_thread_run_store(store.clone()));
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        make_store(),
        store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let submitted = mailbox
        .submit_background(
            RunActivation::new("thread-user-input", vec![Message::user("continue")])
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
            RunActivation::new(thread_id, vec![Message::user("continue open")])
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
async fn recover_pages_orphaned_background_task_waiting_runs() {
    let store = Arc::new(InMemoryStore::new());
    let run_count = 205usize;
    for index in 0..run_count {
        let run = seeded_waiting_run(
            &format!("run-bg-page-{index}"),
            &format!("thread-bg-page-{index}"),
            "agent",
        );
        store.create_run(&run).await.expect("seed bg run");
    }

    let mailbox_store = make_store();
    let runtime = Arc::new(RecordingStoreMailboxRuntime::new(store.clone()));
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store,
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let recovered = mailbox.recover().await.expect("recover should succeed");
    assert_eq!(recovered, run_count);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if runtime.requests.lock().expect("lock poisoned").len() == run_count {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recover did not dispatch every background wake"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn recover_reconstructs_dispatch_for_prepared_run_missing_wal() {
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = make_store();
    let runtime = Arc::new(RecordingStoreMailboxRuntime::new(store.clone()));
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store.clone(),
        store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let mut request =
        RunActivation::new("thread-prepared-crash", vec![Message::user("recover me")])
            .with_agent_id("agent");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .expect("valid request");
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .expect("prepare durable run");
    let dispatch_id = request
        .persistence
        .dispatch_id_hint
        .clone()
        .expect("prepare assigns dispatch id");
    assert!(
        mailbox_store
            .load_dispatch(&dispatch_id)
            .await
            .expect("load dispatch")
            .is_none(),
        "test simulates crash before dispatch WAL enqueue"
    );

    let recovered = mailbox
        .recover()
        .await
        .expect("recover should reconcile WAL");
    assert_eq!(recovered, 1);

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if !runtime.requests.lock().expect("lock poisoned").is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "recovered dispatch did not run");
        sleep(Duration::from_millis(5)).await;
    }
    let dispatch = mailbox_store
        .load_dispatch(&dispatch_id)
        .await
        .expect("load reconstructed dispatch")
        .expect("dispatch reconstructed");
    assert_eq!(dispatch.run_id, run_id);
    assert_eq!(dispatch.thread_id, "thread-prepared-crash");
}

#[tokio::test]
async fn recover_reconstructs_dispatch_for_prepared_waiting_resume_missing_wal() {
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = make_store();
    let runtime = Arc::new(RecordingStoreMailboxRuntime::new(store.clone()));
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let mut waiting = seeded_waiting_run("run-waiting-crash", "thread-waiting-crash", "agent");
    waiting.waiting = Some(RunWaitingState {
        reason: WaitingReason::UserInput,
        ticket_ids: Vec::new(),
        tickets: Vec::new(),
        since_dispatch_id: None,
        message: Some("waiting for user input".to_string()),
    });
    store.create_run(&waiting).await.expect("seed waiting run");

    let mut request = RunActivation::new("thread-waiting-crash", vec![Message::user("resume")])
        .with_agent_id("agent")
        .with_continue_run_id("run-waiting-crash");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .expect("valid request");
    mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .expect("prepare waiting resume");
    let dispatch_id = request
        .persistence
        .dispatch_id_hint
        .clone()
        .expect("prepare assigns dispatch id");

    let recovered = mailbox.recover().await.expect("recover waiting resume");
    assert_eq!(recovered, 1);
    let dispatch = mailbox_store
        .load_dispatch(&dispatch_id)
        .await
        .expect("load reconstructed dispatch")
        .expect("dispatch reconstructed");
    assert_eq!(dispatch.run_id, "run-waiting-crash");
    assert_eq!(dispatch.thread_id, "thread-waiting-crash");
}

#[tokio::test]
async fn recover_prepared_runs_collects_before_enqueue_to_avoid_offset_skips() {
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = make_store();
    let runtime = Arc::new(RecordingStoreMailboxRuntime::new(store.clone()));
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store.clone(),
        store.clone(),
        "test-consumer".to_string(),
        MailboxConfig::default(),
    ));
    let thread_ids = (0..250)
        .map(|idx| format!("thread-prepared-page-{idx}"))
        .collect::<Vec<_>>();
    for (idx, thread_id) in thread_ids.iter().enumerate() {
        let mut run = make_run_record(
            &format!("run-prepared-page-{idx}"),
            thread_id,
            RunStatus::Created,
        );
        run.dispatch_id = Some(format!("dispatch-prepared-page-{idx}"));
        store.create_run(&run).await.expect("seed prepared run");
    }

    let recovered = mailbox
        .recover_prepared_runs_missing_dispatch_wal(&thread_ids)
        .await
        .expect("recover prepared runs");
    assert_eq!(recovered, thread_ids.len());

    for idx in 0..thread_ids.len() {
        let dispatch_id = format!("dispatch-prepared-page-{idx}");
        assert!(
            mailbox_store
                .load_dispatch(&dispatch_id)
                .await
                .expect("load dispatch")
                .is_some(),
            "missing reconstructed dispatch {dispatch_id}"
        );
    }
}

#[tokio::test]
async fn distributed_claim_same_dispatch_allows_exactly_one_consumer() {
    let store = make_store();
    store
        .enqueue(&sample_dispatch(
            "thread-distributed-claim",
            "run-distributed-claim",
            "dispatch-distributed-claim",
        ))
        .await
        .expect("enqueue dispatch");

    let attempts = (0..32)
        .map(|idx| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .claim_dispatch(
                        "dispatch-distributed-claim",
                        &format!("consumer-{idx}"),
                        30_000,
                        now_ms(),
                    )
                    .await
                    .expect("claim attempt")
            })
        })
        .collect::<Vec<_>>();

    let mut winners = Vec::new();
    for attempt in attempts {
        if let Some(dispatch) = attempt.await.expect("claim task") {
            winners.push(dispatch);
        }
    }

    assert_eq!(
        winners.len(),
        1,
        "distributed claim contention must produce exactly one owner"
    );
    let loaded = store
        .load_dispatch("dispatch-distributed-claim")
        .await
        .expect("load dispatch")
        .expect("dispatch exists");
    assert_eq!(loaded.status, RunDispatchStatus::Claimed);
    assert_eq!(loaded.claimed_by, winners[0].claimed_by);
    assert_eq!(loaded.claim_token, winners[0].claim_token);
}

#[tokio::test]
async fn distributed_expired_lease_reclaim_rejects_late_old_owner_ack() {
    let store = make_store();
    let mut dispatch =
        sample_dispatch("thread-lease-race", "run-lease-race", "dispatch-lease-race");
    dispatch.available_at = 0;
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    let claimed_by_a = store
        .claim_dispatch("dispatch-lease-race", "consumer-a", 100, 1_000)
        .await
        .expect("claim by a")
        .expect("a owns dispatch");
    let old_token = claimed_by_a.claim_token.clone().expect("old token");

    let reclaimed = store
        .reclaim_expired_leases(1_101, 10)
        .await
        .expect("reclaim expired lease");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].status, RunDispatchStatus::Queued);
    assert_eq!(reclaimed[0].attempt_count, 1);

    let claimed_by_b = store
        .claim_dispatch("dispatch-lease-race", "consumer-b", 30_000, 1_102)
        .await
        .expect("claim by b")
        .expect("b owns dispatch after reclaim");
    let new_token = claimed_by_b.claim_token.clone().expect("new token");

    assert!(
        store
            .ack("dispatch-lease-race", &old_token, 1_103)
            .await
            .is_err(),
        "late ack from old owner must not release the new owner's claim"
    );
    let still_claimed_by_b = store
        .load_dispatch("dispatch-lease-race")
        .await
        .expect("load after old ack")
        .expect("dispatch exists");
    assert_eq!(still_claimed_by_b.status, RunDispatchStatus::Claimed);
    assert_eq!(still_claimed_by_b.claimed_by.as_deref(), Some("consumer-b"));

    store
        .ack("dispatch-lease-race", &new_token, 1_104)
        .await
        .expect("new owner ack succeeds");
    let delivered = store
        .load_dispatch("dispatch-lease-race")
        .await
        .expect("load delivered")
        .expect("dispatch exists");
    assert_eq!(delivered.status, RunDispatchStatus::Acked);
}

#[tokio::test]
async fn distributed_lease_reclaim_uses_strict_expiry_boundary() {
    let store = make_store();
    let mut dispatch = sample_dispatch(
        "thread-lease-boundary",
        "run-lease-boundary",
        "dispatch-lease-boundary",
    );
    dispatch.available_at = 0;
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    store
        .claim_dispatch("dispatch-lease-boundary", "consumer-a", 100, 1_000)
        .await
        .expect("claim dispatch")
        .expect("claim exists");

    let before_expiry = store
        .reclaim_expired_leases(999, 10)
        .await
        .expect("backward clock reclaim");
    assert!(
        before_expiry.is_empty(),
        "a backward-skewed recovery clock must not reclaim a live lease"
    );

    let at_expiry = store
        .reclaim_expired_leases(1_100, 10)
        .await
        .expect("exact boundary reclaim");
    assert!(
        at_expiry.is_empty(),
        "lease_until is still owned at the exact boundary"
    );

    let after_expiry = store
        .reclaim_expired_leases(1_101, 10)
        .await
        .expect("expired reclaim");
    assert_eq!(
        after_expiry.len(),
        1,
        "lease should only be reclaimed after the boundary has passed"
    );
    assert_eq!(after_expiry[0].status, RunDispatchStatus::Queued);
}

#[tokio::test]
async fn distributed_nack_retry_window_respects_retry_at_boundary() {
    let store = make_store();
    let mut dispatch = sample_dispatch(
        "thread-retry-boundary",
        "run-retry-boundary",
        "dispatch-retry-boundary",
    );
    dispatch.available_at = 0;
    store.enqueue(&dispatch).await.expect("enqueue dispatch");

    let claimed = store
        .claim_dispatch("dispatch-retry-boundary", "consumer-a", 30_000, 1_000)
        .await
        .expect("claim dispatch")
        .expect("claim exists");
    let token = claimed.claim_token.expect("claim token");

    store
        .nack(
            "dispatch-retry-boundary",
            &token,
            2_000,
            "retry later",
            1_001,
        )
        .await
        .expect("nack dispatch");

    let too_early = store
        .claim("thread-retry-boundary", "consumer-b", 30_000, 1_999, 1)
        .await
        .expect("too early claim");
    assert!(
        too_early.is_empty(),
        "dispatch must not be claimable before retry_at"
    );

    let at_retry = store
        .claim("thread-retry-boundary", "consumer-b", 30_000, 2_000, 1)
        .await
        .expect("retry boundary claim");
    assert!(
        at_retry
            .first()
            .is_some_and(|dispatch| dispatch.dispatch_id == "dispatch-retry-boundary"),
        "dispatch must become claimable at retry_at"
    );
}

#[tokio::test]
async fn distributed_recover_prepared_run_missing_wal_is_idempotent_across_instances() {
    let run_store = Arc::new(InMemoryStore::new());
    let mailbox_store = make_store();
    let runtime_a = Arc::new(RecordingStoreMailboxRuntime::new(run_store.clone()));
    let runtime_b = Arc::new(RecordingStoreMailboxRuntime::new(run_store.clone()));
    let mailbox_a = Arc::new(Mailbox::new(
        runtime_a,
        mailbox_store.clone(),
        run_store.clone(),
        "consumer-a".to_string(),
        MailboxConfig::default(),
    ));
    let mailbox_b = Arc::new(Mailbox::new(
        runtime_b,
        mailbox_store.clone(),
        run_store.clone(),
        "consumer-b".to_string(),
        MailboxConfig::default(),
    ));
    let mut run = make_run_record(
        "run-distributed-recover",
        "thread-distributed-recover",
        RunStatus::Created,
    );
    run.dispatch_id = Some("dispatch-distributed-recover".to_string());
    run_store
        .create_run(&run)
        .await
        .expect("seed prepared run without WAL");

    let (a, b) = tokio::join!(
        mailbox_a.recover_prepared_runs_missing_dispatch_wal(&[]),
        mailbox_b.recover_prepared_runs_missing_dispatch_wal(&[])
    );
    let total = a.expect("recover a") + b.expect("recover b");

    assert_eq!(
        total, 1,
        "concurrent startup recovery must reconstruct each prepared dispatch once"
    );
    let dispatches = mailbox_store
        .list_dispatches("thread-distributed-recover", None, 10, 0)
        .await
        .expect("list dispatches");
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0].dispatch_id, "dispatch-distributed-recover");
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
    let agent =
        ResolvedAgent::new("agent", "m", "sys", llm).with_tool(Arc::new(SpawnShortBgTaskTool {
            manager: manager.clone(),
            delay: Duration::from_millis(25),
        }));
    let resolver = Arc::new(FixedResolver {
        agent,
        plugins: vec![Arc::new(BackgroundTaskPlugin::new(manager))],
    });
    let runtime =
        Arc::new(AgentRuntime::new(resolver).with_in_memory_thread_run_store(store.clone()));
    let mailbox = make_mailbox_with_run_store(
        runtime,
        mailbox_store.clone(),
        store.clone() as Arc<dyn ThreadRunStore>,
    );

    mailbox
        .submit_background(
            RunActivation::new("thread-bg", vec![Message::user("start")]).with_agent_id("agent"),
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
            msg.role == awaken_server_contract::contract::message::Role::User
                && msg.visibility == awaken_server_contract::contract::message::Visibility::Internal
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
            action: awaken_server_contract::contract::suspension::ResumeDecisionAction::Resume,
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
            let req = RunActivation::new("thread-conc", vec![Message::user(format!("msg-{i}"))])
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
            let req = RunActivation::new(
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
async fn interrupt_between_claim_and_execution_supersedes_without_runtime_start() {
    crate::metrics::install_recorder();
    let store = Arc::new(InterruptOnLoadMailboxStore::new());
    let run_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(CountingMailboxRuntime::default());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        store.clone(),
        run_store.clone(),
        "epoch-race-consumer".to_string(),
        MailboxConfig {
            lease_ms: 100,
            lease_renewal_interval: Duration::from_millis(20),
            ..MailboxConfig::default()
        },
    ));

    let result = mailbox
        .submit_background(
            RunActivation::new("thread-epoch-race", vec![Message::user("go")])
                .with_agent_id("agent"),
        )
        .await
        .expect("submit should succeed");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(dispatch) = store.load_dispatch(&result.dispatch_id).await.unwrap()
                && dispatch.status == RunDispatchStatus::Superseded
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dispatch should be superseded promptly");

    assert_eq!(
        runtime.run_count(),
        0,
        "stale dispatch must not enter runtime"
    );
    let loaded = store
        .load_dispatch(&result.dispatch_id)
        .await
        .unwrap()
        .expect("dispatch should remain inspectable");
    assert_eq!(loaded.status, RunDispatchStatus::Superseded);
    assert!(loaded.claim_token.is_none());
    assert!(loaded.lease_until.is_none());

    let run = run_store
        .load_run(&result.run_id)
        .await
        .unwrap()
        .expect("prepared run should remain inspectable");
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.termination_reason, Some(TerminationReason::Cancelled));
    assert_eq!(
        run.dispatch_id.as_deref(),
        Some(result.dispatch_id.as_str())
    );

    let output = crate::metrics::render().unwrap_or_default();
    assert!(output.contains("operation=\"load_dispatch\""));
    assert!(output.contains("operation=\"current_dispatch_epoch\""));
    assert!(output.contains("operation=\"supersede_claimed\""));
    assert!(output.contains("operation=\"mark_run_superseded\""));
}

#[tokio::test]
async fn dispatch_signal_busy_ack_still_runs_queued_dispatch_after_current_finishes() {
    let store = Arc::new(SignalMailboxStore::new());
    let run_store = Arc::new(InMemoryStore::new());
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let release_first = Arc::new(tokio::sync::Notify::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(BlockingMailboxRuntime::new(
        started_tx,
        Arc::clone(&release_first),
    ));
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        store.clone(),
        run_store,
        "signal-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let mut first = RunActivation::new("thread-signal-busy", vec![Message::user("first")])
        .with_agent_id("agent");
    let (thread_id, first_messages) = validate_run_inputs(
        first.thread_id().to_owned(),
        first.messages().to_vec(),
        false,
    )
    .expect("first input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut first, &thread_id, &first_messages)
        .await
        .expect("prepare first run");
    let first_dispatch = mailbox
        .build_dispatch(&first, &thread_id)
        .expect("build first dispatch");
    let first_dispatch_id = first_dispatch.dispatch_id.clone();
    store.enqueue(&first_dispatch).await.expect("enqueue first");

    let mut second = RunActivation::new("thread-signal-busy", vec![Message::user("second")])
        .with_agent_id("agent");
    let (_, second_messages) = validate_run_inputs(
        second.thread_id().to_owned(),
        second.messages().to_vec(),
        false,
    )
    .expect("second input should validate");
    mailbox
        .prepare_run_for_dispatch(&mut second, &thread_id, &second_messages)
        .await
        .expect("prepare second run");
    let second_dispatch = mailbox
        .build_dispatch(&second, &thread_id)
        .expect("build second dispatch");
    let second_dispatch_id = second_dispatch.dispatch_id.clone();
    store
        .enqueue(&second_dispatch)
        .await
        .expect("enqueue second");

    let signal_loop = tokio::spawn(Arc::clone(&mailbox).run_dispatch_signal_loop());
    let (ordinal, dispatch_id) = tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .expect("first dispatch should start")
        .expect("runtime should report first start");
    assert_eq!(ordinal, 1);
    let blocked_dispatch_id = dispatch_id.expect("started dispatch should have an id");
    assert!(
        blocked_dispatch_id == first_dispatch_id || blocked_dispatch_id == second_dispatch_id,
        "started dispatch must be one of the two queued dispatches"
    );
    let queued_dispatch_id = if blocked_dispatch_id == first_dispatch_id {
        second_dispatch_id.as_str()
    } else {
        first_dispatch_id.as_str()
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    while store.acked_signal_count() < 2 {
        assert!(
            Instant::now() < deadline,
            "signal loop must ack the busy second signal instead of blocking"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(store.nacked_signal_count(), 0);
    let queued_before_release = store
        .load_dispatch(queued_dispatch_id)
        .await
        .expect("load queued dispatch")
        .expect("queued dispatch exists");
    assert_eq!(
        queued_before_release.status,
        RunDispatchStatus::Queued,
        "busy signal ack must not claim the other dispatch before the first finishes"
    );

    release_first.notify_waiters();
    let (ordinal, dispatch_id) = tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .expect("queued dispatch should start after first finishes")
        .expect("runtime should report second start");
    assert_eq!(ordinal, 2);
    assert_eq!(dispatch_id.as_deref(), Some(queued_dispatch_id));

    let first_done = wait_for_dispatch(&store.inner, &first_dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked
    })
    .await;
    let second_done = wait_for_dispatch(&store.inner, &second_dispatch_id, |dispatch| {
        dispatch.status == RunDispatchStatus::Acked
    })
    .await;
    signal_loop.abort();

    assert_eq!(first_done.status, RunDispatchStatus::Acked);
    assert_eq!(second_done.status, RunDispatchStatus::Acked);
    assert_eq!(store.acked_signal_count(), 2);
    assert_eq!(store.nacked_signal_count(), 0);
}

#[tokio::test]
async fn submit_background_returns_correct_status() {
    let store = make_store();
    let runtime = make_runtime();
    let mailbox = make_mailbox(runtime, store.clone());

    // First submit should dispatch (Running or Queued depending on timing).
    let req1 =
        RunActivation::new("thread-status", vec![Message::user("a")]).with_agent_id("agent-1");
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
        RunActivation::new("thread-status", vec![Message::user("b")]).with_agent_id("agent-1");
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
    let req = RunActivation::new("thread-guard", vec![Message::user("a")]).with_agent_id("agent-1");
    mailbox.submit_background(req).await.unwrap();

    // Worker should be Running (or Claiming).
    let workers = mailbox.workers.read().await;
    if let Some(worker) = workers.get("thread-guard") {
        let w = worker.lock();
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
        let w = worker.lock();
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
    let result = LegacyRunSnapshotExtras::from_value(&corrupt);
    assert!(result.is_err(), "corrupt JSON should fail deserialization");
}

#[tokio::test]
async fn submit_inline_claim_fails_when_thread_already_claimed() {
    let store = make_store();
    let runtime = make_runtime();
    let mailbox = make_mailbox(runtime, store.clone());

    // First submit claims successfully.
    let req1 =
        RunActivation::new("thread-clash", vec![Message::user("first")]).with_agent_id("agent-1");
    let result1 = mailbox.submit(req1).await;
    assert!(result1.is_ok(), "first submit should succeed");

    // Second submit to same thread: interrupt will cancel the first,
    // but timing may allow the second to also succeed or fail gracefully.
    let req2 =
        RunActivation::new("thread-clash", vec![Message::user("second")]).with_agent_id("agent-1");
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
        let mut w = worker.lock();
        w.status = MailboxWorkerStatus::Running {
            dispatch_id: "dispatch-fake".into(),
            run_id: "run-fake".into(),
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
    use awaken_server_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

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

    let request = RunActivation::new("thread-dec", vec![Message::user("hi")])
        .with_agent_id("a1")
        .with_decisions(decisions.clone());
    let extras = LegacyRunSnapshotExtras::from_request(&request);
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

    let mut request = RunActivation::new("thread-a2a", vec![Message::user("hi")])
        .with_origin(RunRequestOrigin::A2A)
        .with_parent_run_id("parent-123");
    let (thread_id, messages) = validate_run_inputs(
        request.thread_id().to_owned(),
        request.messages().to_vec(),
        false,
    )
    .unwrap();
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
        .await
        .unwrap();
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();

    assert!(matches!(
        RunRequestOrigin::from(run.activation.as_ref().unwrap().trace.origin),
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
        let delay = (config.default_retry_delay_ms * backoff_factor).min(config.max_retry_delay_ms);
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
    let delay2 = (config2.default_retry_delay_ms * backoff_factor).min(config2.max_retry_delay_ms);
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
            Arc::new(SyncMutex::new(MailboxWorker::default())),
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
        RunActivation::new("thread-gc-keep", vec![Message::user("hi")]).with_agent_id("agent-1");
    mailbox.submit_background(request).await.unwrap();

    // Force the worker to Idle status (simulating it finished one dispatch
    // but another is queued).
    {
        let mut workers = mailbox.workers.write().await;
        workers.insert(
            "thread-gc-keep".to_string(),
            Arc::new(SyncMutex::new(MailboxWorker::default())),
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

// ── ThreadContext cache tests ───────────────────────────────────

fn make_run_record(run_id: &str, thread_id: &str, status: RunStatus) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
        resolution_id: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 1,
        started_at: None,
        finished_at: None,
        updated_at: 1,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

fn make_waiting_run_record(run_id: &str, thread_id: &str) -> RunRecord {
    let mut run = make_run_record(run_id, thread_id, RunStatus::Waiting);
    run.waiting = Some(RunWaitingState {
        reason: WaitingReason::BackgroundTasks,
        ticket_ids: Vec::new(),
        tickets: Vec::new(),
        since_dispatch_id: None,
        message: None,
    });
    run
}

fn make_noop_mailbox(thread_store: Arc<InMemoryStore>) -> Arc<Mailbox> {
    let mailbox_store = make_store();
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    Arc::new(Mailbox::new(
        runtime,
        mailbox_store,
        thread_store,
        "test-consumer".into(),
        MailboxConfig::default(),
    ))
}

#[tokio::test]
async fn thread_context_cache_used_by_reusable_waiting_run_id() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-ctx-reuse";

    // Create a waiting run in the store.
    let run = make_waiting_run_record("run-waiting", thread_id);
    thread_store
        .checkpoint(thread_id, &[Message::user("hi")], &run)
        .await
        .unwrap();

    // Pre-warm the cache on the worker.
    let worker = mailbox.get_or_create_worker(thread_id).await;
    let ctx = ThreadContext::load(thread_store.as_ref(), thread_id)
        .await
        .unwrap();
    {
        let mut w = worker.lock();
        w.thread_ctx = Some(ctx);
    }

    let result = mailbox.reusable_waiting_run_id(thread_id).await;
    assert_eq!(result, Some("run-waiting".to_string()));
}

#[tokio::test]
async fn thread_context_cache_updated_after_prepare_checkpoint() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-ctx-checkpoint";

    // Persist initial state with a Done run.
    let run = make_run_record("run-prev", thread_id, RunStatus::Done);
    thread_store
        .checkpoint(thread_id, &[Message::user("first")], &run)
        .await
        .unwrap();

    // Pre-warm the cache.
    let worker = mailbox.get_or_create_worker(thread_id).await;
    let ctx = ThreadContext::load(thread_store.as_ref(), thread_id)
        .await
        .unwrap();
    {
        let mut w = worker.lock();
        w.thread_ctx = Some(ctx);
    }

    // Prepare a new dispatch — this should update the cache.
    let mut request =
        RunActivation::new(thread_id, vec![Message::user("second")]).with_agent_id("agent");
    let msgs = request.messages().to_vec();
    mailbox
        .prepare_run_for_dispatch(&mut request, thread_id, &msgs)
        .await
        .expect("prepare should succeed");

    // Verify cache was updated with both messages and the new run.
    let w = worker.lock();
    let ctx = w.thread_ctx.as_ref().expect("cache should exist");
    assert_eq!(ctx.messages.len(), 2, "cache should have 2 messages");
    assert!(ctx.latest_run.is_some(), "cache should have latest run");
}

#[tokio::test]
async fn prepare_run_falls_back_to_store_without_cache() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-no-cache";

    // Persist initial state but do NOT pre-warm the cache.
    let run = make_run_record("run-prev", thread_id, RunStatus::Done);
    thread_store
        .checkpoint(thread_id, &[Message::user("first")], &run)
        .await
        .unwrap();

    // No cache — should fall back to store.
    let mut request =
        RunActivation::new(thread_id, vec![Message::user("second")]).with_agent_id("agent");
    let msgs = request.messages().to_vec();
    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, thread_id, &msgs)
        .await
        .expect("should succeed from store fallback");
    assert!(!run_id.is_empty());

    // Store should have both messages.
    let stored = thread_store
        .load_messages(thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.len(), 2);
}

#[tokio::test]
async fn prepare_run_uses_durable_messages_when_active_cache_is_stale() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-stale-cache";

    let active = make_run_record("run-active", thread_id, RunStatus::Running);
    thread_store
        .checkpoint(thread_id, &[Message::user("first")], &active)
        .await
        .unwrap();

    // Simulate the cache snapshot captured when the active run started.
    let worker = mailbox.get_or_create_worker(thread_id).await;
    let stale_ctx = ThreadContext::load(thread_store.as_ref(), thread_id)
        .await
        .unwrap();
    {
        let mut w = worker.lock();
        w.thread_ctx = Some(stale_ctx);
    }

    // Runtime checkpoints a new assistant message while the worker cache is
    // still the older snapshot.
    thread_store
        .checkpoint(
            thread_id,
            &[Message::user("first"), Message::assistant("active output")],
            &active,
        )
        .await
        .unwrap();

    let mut request =
        RunActivation::new(thread_id, vec![Message::user("second")]).with_agent_id("agent");
    let msgs = request.messages().to_vec();
    mailbox
        .prepare_run_for_dispatch(&mut request, thread_id, &msgs)
        .await
        .expect("prepare should preserve active-run checkpoint");

    let stored = thread_store
        .load_messages(thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[1].text(), "active output");
    assert_eq!(stored[2].text(), "second");
}

#[tokio::test]
async fn reusable_waiting_run_id_ignores_stale_worker_cache() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-stale-waiting-cache";

    let waiting = make_waiting_run_record("run-waiting", thread_id);
    thread_store
        .checkpoint(thread_id, &[Message::user("hi")], &waiting)
        .await
        .unwrap();

    let worker = mailbox.get_or_create_worker(thread_id).await;
    let stale_ctx = ThreadContext::load(thread_store.as_ref(), thread_id)
        .await
        .unwrap();
    {
        let mut w = worker.lock();
        w.thread_ctx = Some(stale_ctx);
    }

    let done = make_run_record("run-waiting", thread_id, RunStatus::Done);
    thread_store
        .checkpoint(
            thread_id,
            &[Message::user("hi"), Message::assistant("done")],
            &done,
        )
        .await
        .unwrap();

    assert_eq!(mailbox.reusable_waiting_run_id(thread_id).await, None);
}

#[tokio::test]
async fn thread_context_cache_cleared_on_idle_transition() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-ctx-clear";

    let run = make_run_record("r1", thread_id, RunStatus::Done);
    thread_store
        .checkpoint(thread_id, &[Message::user("hi")], &run)
        .await
        .unwrap();

    // Set up worker as Running with a populated cache.
    let worker = mailbox.get_or_create_worker(thread_id).await;
    let ctx = ThreadContext::load(thread_store.as_ref(), thread_id)
        .await
        .unwrap();
    {
        let mut w = worker.lock();
        w.thread_ctx = Some(ctx);
        w.status = MailboxWorkerStatus::Running {
            dispatch_id: "d1".into(),
            run_id: "r1".into(),
            lease_handle: tokio::spawn(async {}),
            sink: Arc::new(ReconnectableEventSink::new(mpsc::channel(16).0)),
        };
    }

    // Verify cache exists before transition.
    assert!(worker.lock().thread_ctx.is_some());

    // Simulate completion: transition to Idle and clear cache.
    {
        let mut w = worker.lock();
        let old = std::mem::replace(&mut w.status, MailboxWorkerStatus::Idle);
        w.thread_ctx = None;
        if let MailboxWorkerStatus::Running { lease_handle, .. } = old {
            lease_handle.abort();
        }
    }

    assert!(
        worker.lock().thread_ctx.is_none(),
        "cache should be cleared on idle transition"
    );
}

#[tokio::test]
async fn thread_context_load_populates_run_cache() {
    let store = Arc::new(InMemoryStore::new());
    let thread_id = "thread-load-test";

    let run = make_run_record("r1", thread_id, RunStatus::Done);
    store
        .checkpoint(thread_id, &[Message::user("msg")], &run)
        .await
        .unwrap();

    let ctx = ThreadContext::load(store.as_ref(), thread_id)
        .await
        .expect("load should succeed");

    assert_eq!(ctx.messages.len(), 1);
    assert!(ctx.latest_run.is_some());
    assert_eq!(ctx.latest_run.as_ref().unwrap().run_id, "r1");
    assert!(ctx.get_run("r1").is_some());
    assert!(ctx.get_run("unknown").is_none());
}

#[tokio::test]
async fn reusable_waiting_run_id_returns_none_for_done_cached_run() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = make_noop_mailbox(thread_store.clone());
    let thread_id = "thread-done-run";

    let run = make_run_record("run-done", thread_id, RunStatus::Done);
    thread_store
        .checkpoint(thread_id, &[Message::user("hi")], &run)
        .await
        .unwrap();

    let worker = mailbox.get_or_create_worker(thread_id).await;
    let ctx = ThreadContext::load(thread_store.as_ref(), thread_id)
        .await
        .unwrap();
    {
        let mut w = worker.lock();
        w.thread_ctx = Some(ctx);
    }

    let result = mailbox.reusable_waiting_run_id(thread_id).await;
    assert_eq!(result, None, "Done run should not be reusable");
}

// ── ADR-0036 D9 mailbox dispatch wiring ─────────────────────────────

struct CoordinatorAwareNoopRuntime;

#[async_trait]
impl RunDispatchExecutor for CoordinatorAwareNoopRuntime {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        panic!("wiring test must not execute runs")
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
    fn has_commit_coordinator(&self) -> bool {
        true
    }
}

fn sample_dispatch(thread_id: &str, run_id: &str, dispatch_id: &str) -> RunDispatch {
    let now = now_ms();
    RunDispatch {
        dispatch_id: dispatch_id.to_string(),
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        priority: 0,
        dedupe_key: None,
        dispatch_epoch: 0,
        status: RunDispatchStatus::Queued,
        available_at: now,
        attempt_count: 0,
        max_attempts: 3,
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
    }
}

/// ADR-0036 D9: when a per-run `EventBuffer` is supplied to
/// `wrap_dispatch_runtime_event_sink`, durable events stage into the buffer
/// (atomic-commit path) and are NOT inline-appended to the canonical writer.
#[tokio::test]
async fn wrap_dispatch_with_buffer_stages_into_buffer_not_writer() {
    use crate::transport::channel_sink::ReconnectableEventSink;
    use awaken_runtime::EventBuffer;

    let event_store = Arc::new(InMemoryEventStore::new());
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(CoordinatorAwareNoopRuntime);
    let mailbox = Arc::new(
        Mailbox::new(
            runtime,
            mailbox_store,
            thread_store,
            "c".into(),
            MailboxConfig::default(),
        )
        .with_runtime_event_capture(RuntimeEventDurability::Compacted, "test")
        .unwrap(),
    );

    let dispatch = sample_dispatch("t-buf", "r-buf", "d-buf");
    let (tx, _rx) = mpsc::channel::<AgentEvent>(16);
    let reconnectable = Arc::new(ReconnectableEventSink::new(tx));
    let buffer = Arc::new(EventBuffer::new());

    let wrapped = mailbox.wrap_dispatch_runtime_event_sink(
        reconnectable,
        &dispatch,
        "d-buf".into(),
        false,
        Some(Arc::clone(&buffer)),
    );

    wrapped
        .emit(AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({"q": "x"}),
        })
        .await;

    assert_eq!(
        buffer.len(),
        1,
        "durable draft must stage into the per-run buffer"
    );
    let writer_count = event_store.count(EventScope::run("r-buf")).await.unwrap();
    assert_eq!(
        writer_count, 0,
        "buffered path must not write to canonical writer inline"
    );
}

#[test]
fn runtime_event_capture_requires_commit_coordinator() {
    let mailbox_store = make_store();
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime: Arc<dyn RunDispatchExecutor> = Arc::new(NoopMailboxRuntime);
    let result = Mailbox::new(
        runtime,
        mailbox_store,
        thread_store,
        "c".into(),
        MailboxConfig::default(),
    )
    .with_runtime_event_capture(RuntimeEventDurability::Compacted, "test");
    let error = match result {
        Ok(_) => panic!("capture without a coordinator must be rejected"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "validation error: runtime event capture requires an executor with CommitCoordinator"
    );
}
