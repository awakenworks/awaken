//! Fault injection tests for server-side components.
//!
//! Tests mailbox store failure modes and event sink channel disconnection
//! under various failure conditions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::event_sink::EventSink;
use awaken_server_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_server_contract::contract::inference::{StopReason, StreamResult};
use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::mailbox::{
    MailboxInterrupt, MailboxInterruptDetails, MailboxStore, RunDispatch, RunDispatchResult,
    RunDispatchStatus,
};
use awaken_server_contract::contract::storage::{
    RunRecord, RunStore, RunWaitingState, StorageError, WaitingReason,
};
use awaken_server_contract::{AgentSpec, ModelSpec};
use awaken_stores::{InMemoryMailboxStore, InMemoryStore};
use tokio::sync::mpsc;

// ============================================================================
// FailingMailboxStore — wraps InMemoryMailboxStore with injectable failures
// ============================================================================

struct FailingMailboxStore {
    inner: InMemoryMailboxStore,
    fail_enqueue: AtomicBool,
    fail_claim: AtomicBool,
    fail_ack: AtomicBool,
    fail_nack: AtomicBool,
}

impl FailingMailboxStore {
    fn new() -> Self {
        Self {
            inner: InMemoryMailboxStore::new(),
            fail_enqueue: AtomicBool::new(false),
            fail_claim: AtomicBool::new(false),
            fail_ack: AtomicBool::new(false),
            fail_nack: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl MailboxStore for FailingMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        if self.fail_enqueue.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected enqueue failure".into()));
        }
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
        if self.fail_claim.load(Ordering::SeqCst) {
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
        if self.fail_claim.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected claim_dispatch failure".into()));
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
        if self.fail_ack.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected ack failure".into()));
        }
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
        if self.fail_nack.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected nack failure".into()));
        }
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
}

fn make_dispatch(dispatch_id: &str, thread_id: &str) -> RunDispatch {
    RunDispatch::queued(
        dispatch_id.to_string(),
        thread_id.to_string(),
        format!("run-{dispatch_id}"),
        1000,
    )
    .with_priority(128)
    .with_max_attempts(5)
}

struct ImmediateLlm;

#[async_trait]
impl LlmExecutor for ImmediateLlm {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "immediate"
    }
}

fn background_waiting_run(run_id: &str, thread_id: &str) -> RunRecord {
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
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

// ============================================================================
// Enqueue failure propagates error
// ============================================================================

#[tokio::test]
async fn enqueue_failure_propagates_error() {
    let store = FailingMailboxStore::new();
    store.fail_enqueue.store(true, Ordering::SeqCst);

    let dispatch = make_dispatch("j-1", "thread-1");
    let result = store.enqueue(&dispatch).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected enqueue failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

#[tokio::test]
async fn mailbox_recovery_propagates_orphaned_background_enqueue_failure() {
    let run_store = Arc::new(InMemoryStore::new());
    run_store
        .create_run(&background_waiting_run(
            "run-bg-recover-fail",
            "thread-bg-recover-fail",
        ))
        .await
        .expect("seed background wait");

    let mailbox_store = Arc::new(FailingMailboxStore::new());
    mailbox_store.fail_enqueue.store(true, Ordering::SeqCst);
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("provider", Arc::new(ImmediateLlm))
            .with_model(ModelSpec::new("model", "provider", "model"))
            .with_agent_spec(AgentSpec {
                id: "agent".to_string(),
                model_id: "model".to_string(),
                system_prompt: "test".to_string(),
                max_rounds: 1,
                ..Default::default()
            })
            .with_in_memory_thread_run_store(run_store.clone())
            .build()
            .expect("runtime"),
    );
    let mailbox = Arc::new(Mailbox::new(
        runtime,
        mailbox_store,
        run_store,
        "fault-injection-consumer".to_string(),
        MailboxConfig::default(),
    ));

    let error = mailbox
        .recover()
        .await
        .expect_err("startup recovery must fail closed when wake enqueue fails");
    assert!(
        error.to_string().contains("injected enqueue failure"),
        "unexpected recovery error: {error}"
    );
}

// ============================================================================
// Claim failure propagates error
// ============================================================================

#[tokio::test]
async fn claim_failure_propagates_error() {
    let store = FailingMailboxStore::new();

    // Enqueue a dispatch first
    let dispatch = make_dispatch("j-1", "thread-1");
    store.enqueue(&dispatch).await.unwrap();

    // Now make claim fail
    store.fail_claim.store(true, Ordering::SeqCst);

    let result = store.claim("thread-1", "consumer-1", 30_000, 1000, 1).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected claim failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ============================================================================
// Ack failure leaves dispatch in Claimed state (lease will expire for reclaim)
// ============================================================================

#[tokio::test]
async fn ack_failure_leaves_dispatch_claimed_for_reclaim() {
    let store = FailingMailboxStore::new();

    // Enqueue and claim
    let dispatch = make_dispatch("j-1", "thread-1");
    store.enqueue(&dispatch).await.unwrap();
    let claimed = store
        .claim("thread-1", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let claim_token = claimed[0].claim_token().unwrap().to_string();

    // Make ack fail
    store.fail_ack.store(true, Ordering::SeqCst);
    let result = store.ack("j-1", &claim_token, 2000).await;
    assert!(result.is_err());

    // Dispatch should still be in Claimed state
    let loaded = store.load_dispatch("j-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::Claimed);

    // After lease expiry, reclaim should recover the dispatch
    store.fail_ack.store(false, Ordering::SeqCst);
    let lease_expiry = loaded.lease_until().unwrap() + 1;
    let reclaimed = store
        .reclaim_expired_leases(lease_expiry, 10)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].dispatch_id(), "j-1");
    assert_eq!(reclaimed[0].status(), RunDispatchStatus::Queued);
    assert_eq!(reclaimed[0].attempt_count(), 1);
}

// ============================================================================
// Nack failure leaves dispatch in Claimed state
// ============================================================================

#[tokio::test]
async fn nack_failure_leaves_dispatch_claimed() {
    let store = FailingMailboxStore::new();

    let dispatch = make_dispatch("j-1", "thread-1");
    store.enqueue(&dispatch).await.unwrap();
    let claimed = store
        .claim("thread-1", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let claim_token = claimed[0].claim_token().unwrap().to_string();

    store.fail_nack.store(true, Ordering::SeqCst);
    let result = store
        .nack("j-1", &claim_token, 5000, "processing error", 2000)
        .await;
    assert!(result.is_err());

    // Dispatch remains Claimed
    let loaded = store.load_dispatch("j-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::Claimed);
}

// ============================================================================
// Enqueue failure does not affect existing dispatches
// ============================================================================

#[tokio::test]
async fn enqueue_failure_does_not_corrupt_existing_dispatches() {
    let store = FailingMailboxStore::new();

    // Successfully enqueue first dispatch
    let dispatch1 = make_dispatch("j-1", "thread-1");
    store.enqueue(&dispatch1).await.unwrap();

    // Fail second enqueue
    store.fail_enqueue.store(true, Ordering::SeqCst);
    let dispatch2 = make_dispatch("j-2", "thread-1");
    assert!(store.enqueue(&dispatch2).await.is_err());

    // First dispatch is still intact
    let loaded = store.load_dispatch("j-1").await.unwrap().unwrap();
    assert_eq!(loaded.dispatch_id(), "j-1");
    assert_eq!(loaded.status(), RunDispatchStatus::Queued);

    // Second dispatch was never persisted
    assert!(store.load_dispatch("j-2").await.unwrap().is_none());
}

// ============================================================================
// Claim failure after enqueue — dispatch remains claimable after recovery
// ============================================================================

#[tokio::test]
async fn dispatch_remains_claimable_after_claim_failure_recovery() {
    let store = FailingMailboxStore::new();

    let dispatch = make_dispatch("j-1", "thread-1");
    store.enqueue(&dispatch).await.unwrap();

    // Fail claim
    store.fail_claim.store(true, Ordering::SeqCst);
    assert!(
        store
            .claim("thread-1", "consumer-1", 30_000, 1000, 1)
            .await
            .is_err()
    );

    // Recover
    store.fail_claim.store(false, Ordering::SeqCst);
    let claimed = store
        .claim("thread-1", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id(), "j-1");
}

// ============================================================================
// EventSink channel disconnection — various sink types
// ============================================================================

#[tokio::test]
async fn unbounded_channel_sink_handles_closed_receiver() {
    use awaken_server::transport::channel_sink::ChannelEventSink;

    let (tx, rx) = mpsc::unbounded_channel();
    let sink = ChannelEventSink::new(tx);
    drop(rx); // Close the receiver

    // Emit multiple events — none should panic
    sink.emit(AgentEvent::TextDelta {
        delta: "test".into(),
    })
    .await;
    sink.emit(AgentEvent::StepEnd).await;
    sink.emit(AgentEvent::RunFinish {
        thread_id: "t1".into(),
        run_id: "r1".into(),
        identity: None,
        result: None,
        termination: awaken_server_contract::contract::lifecycle::TerminationReason::NaturalEnd,
    })
    .await;
    sink.close().await;
}

#[tokio::test]
async fn bounded_channel_sink_handles_closed_receiver() {
    use awaken_server::transport::channel_sink::BoundedChannelEventSink;

    let (tx, rx) = mpsc::channel(1);
    let sink = BoundedChannelEventSink::new(tx);
    drop(rx);

    // Should not panic even though receiver is gone
    sink.emit(AgentEvent::TextDelta {
        delta: "test".into(),
    })
    .await;
    sink.emit(AgentEvent::StepEnd).await;
    sink.close().await;
}

#[tokio::test]
async fn reconnectable_sink_handles_receiver_drop_mid_stream() {
    use awaken_server::transport::channel_sink::ReconnectableEventSink;

    let (tx, rx) = mpsc::channel(16);
    let sink = ReconnectableEventSink::new(tx);

    // Emit successfully first
    sink.emit(AgentEvent::TextDelta {
        delta: "before".into(),
    })
    .await;

    // Drop receiver mid-stream
    drop(rx);

    // Emit should not panic
    sink.emit(AgentEvent::TextDelta {
        delta: "after-drop".into(),
    })
    .await;

    // Reconnect to fresh channel and continue
    let (tx2, mut rx2) = mpsc::channel(16);
    sink.reconnect(tx2);
    sink.emit(AgentEvent::TextDelta {
        delta: "reconnected".into(),
    })
    .await;

    let event = rx2.recv().await.unwrap();
    assert!(matches!(event, AgentEvent::TextDelta { delta } if delta == "reconnected"));
}

// ============================================================================
// Bounded channel sink under backpressure (full buffer)
// ============================================================================

#[tokio::test]
async fn bounded_channel_sink_under_backpressure() {
    use awaken_server::transport::channel_sink::BoundedChannelEventSink;

    // Buffer of 1 — will block when full
    let (tx, mut rx) = mpsc::channel(1);
    let sink = Arc::new(BoundedChannelEventSink::new(tx));

    // Fill the buffer
    sink.emit(AgentEvent::TextDelta {
        delta: "first".into(),
    })
    .await;

    // Spawn a task that emits (will block on full buffer)
    let sink_clone = Arc::clone(&sink);
    let emit_handle = tokio::spawn(async move {
        sink_clone
            .emit(AgentEvent::TextDelta {
                delta: "second".into(),
            })
            .await;
    });

    // Drain receiver to unblock the sender
    let e1 = rx.recv().await.unwrap();
    assert!(matches!(e1, AgentEvent::TextDelta { delta } if delta == "first"));

    emit_handle.await.unwrap();
    let e2 = rx.recv().await.unwrap();
    assert!(matches!(e2, AgentEvent::TextDelta { delta } if delta == "second"));
}

// ============================================================================
// VecEventSink is resilient (no channel, no failure mode)
// ============================================================================

#[tokio::test]
async fn vec_sink_handles_massive_event_volume() {
    use awaken_server_contract::contract::event_sink::VecEventSink;

    let sink = VecEventSink::new();

    // Emit 10,000 events — should not panic or OOM in test context
    for i in 0..10_000 {
        sink.emit(AgentEvent::TextDelta {
            delta: format!("chunk-{i}"),
        })
        .await;
    }

    let events = sink.take();
    assert_eq!(events.len(), 10_000);

    // Buffer is empty after take
    assert!(sink.take().is_empty());
}
