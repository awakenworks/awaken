//! Fault injection tests for server-side components.
//!
//! Tests mailbox store failure modes and event sink channel disconnection
//! under various failure conditions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::mailbox::{
    MailboxInterrupt, MailboxJob, MailboxJobOrigin, MailboxJobStatus, MailboxStore,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::StorageError;
use awaken_stores::InMemoryMailboxStore;
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
    async fn enqueue(&self, job: &MailboxJob) -> Result<(), StorageError> {
        if self.fail_enqueue.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected enqueue failure".into()));
        }
        self.inner.enqueue(job).await
    }

    async fn claim(
        &self,
        mailbox_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        if self.fail_claim.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected claim failure".into()));
        }
        self.inner
            .claim(mailbox_id, consumer_id, lease_ms, now, limit)
            .await
    }

    async fn claim_job(
        &self,
        job_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<MailboxJob>, StorageError> {
        if self.fail_claim.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected claim_job failure".into()));
        }
        self.inner
            .claim_job(job_id, consumer_id, lease_ms, now)
            .await
    }

    async fn ack(&self, job_id: &str, claim_token: &str, now: u64) -> Result<(), StorageError> {
        if self.fail_ack.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected ack failure".into()));
        }
        self.inner.ack(job_id, claim_token, now).await
    }

    async fn nack(
        &self,
        job_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        if self.fail_nack.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected nack failure".into()));
        }
        self.inner
            .nack(job_id, claim_token, retry_at, error, now)
            .await
    }

    async fn dead_letter(
        &self,
        job_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .dead_letter(job_id, claim_token, error, now)
            .await
    }

    async fn cancel(&self, job_id: &str, now: u64) -> Result<Option<MailboxJob>, StorageError> {
        self.inner.cancel(job_id, now).await
    }

    async fn extend_lease(
        &self,
        job_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        self.inner
            .extend_lease(job_id, claim_token, extension_ms, now)
            .await
    }

    async fn interrupt(
        &self,
        mailbox_id: &str,
        now: u64,
    ) -> Result<MailboxInterrupt, StorageError> {
        self.inner.interrupt(mailbox_id, now).await
    }

    async fn load_job(&self, job_id: &str) -> Result<Option<MailboxJob>, StorageError> {
        self.inner.load_job(job_id).await
    }

    async fn list_jobs(
        &self,
        mailbox_id: &str,
        status_filter: Option<&[MailboxJobStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        self.inner
            .list_jobs(mailbox_id, status_filter, limit, offset)
            .await
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        self.inner.reclaim_expired_leases(now, limit).await
    }

    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        self.inner.purge_terminal(older_than).await
    }

    async fn queued_mailbox_ids(&self) -> Result<Vec<String>, StorageError> {
        self.inner.queued_mailbox_ids().await
    }
}

fn make_job(job_id: &str, mailbox_id: &str) -> MailboxJob {
    MailboxJob {
        job_id: job_id.to_string(),
        mailbox_id: mailbox_id.to_string(),
        agent_id: "agent-1".to_string(),
        messages: vec![Message::user("hello")],
        origin: MailboxJobOrigin::User,
        sender_id: None,
        parent_run_id: None,
        request_extras: None,
        priority: 128,
        dedupe_key: None,
        generation: 0,
        status: MailboxJobStatus::Queued,
        available_at: 1000,
        attempt_count: 0,
        max_attempts: 5,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        created_at: 1000,
        updated_at: 1000,
    }
}

// ============================================================================
// Enqueue failure propagates error
// ============================================================================

#[tokio::test]
async fn enqueue_failure_propagates_error() {
    let store = FailingMailboxStore::new();
    store.fail_enqueue.store(true, Ordering::SeqCst);

    let job = make_job("j-1", "mbox-1");
    let result = store.enqueue(&job).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected enqueue failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ============================================================================
// Claim failure propagates error
// ============================================================================

#[tokio::test]
async fn claim_failure_propagates_error() {
    let store = FailingMailboxStore::new();

    // Enqueue a job first
    let job = make_job("j-1", "mbox-1");
    store.enqueue(&job).await.unwrap();

    // Now make claim fail
    store.fail_claim.store(true, Ordering::SeqCst);

    let result = store.claim("mbox-1", "consumer-1", 30_000, 1000, 1).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected claim failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ============================================================================
// Ack failure leaves job in Claimed state (lease will expire for reclaim)
// ============================================================================

#[tokio::test]
async fn ack_failure_leaves_job_claimed_for_reclaim() {
    let store = FailingMailboxStore::new();

    // Enqueue and claim
    let job = make_job("j-1", "mbox-1");
    store.enqueue(&job).await.unwrap();
    let claimed = store
        .claim("mbox-1", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let claim_token = claimed[0].claim_token.clone().unwrap();

    // Make ack fail
    store.fail_ack.store(true, Ordering::SeqCst);
    let result = store.ack("j-1", &claim_token, 2000).await;
    assert!(result.is_err());

    // Job should still be in Claimed state
    let loaded = store.load_job("j-1").await.unwrap().unwrap();
    assert_eq!(loaded.status, MailboxJobStatus::Claimed);

    // After lease expiry, reclaim should recover the job
    store.fail_ack.store(false, Ordering::SeqCst);
    let lease_expiry = loaded.lease_until.unwrap() + 1;
    let reclaimed = store
        .reclaim_expired_leases(lease_expiry, 10)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].job_id, "j-1");
    assert_eq!(reclaimed[0].status, MailboxJobStatus::Queued);
    assert_eq!(reclaimed[0].attempt_count, 1);
}

// ============================================================================
// Nack failure leaves job in Claimed state
// ============================================================================

#[tokio::test]
async fn nack_failure_leaves_job_claimed() {
    let store = FailingMailboxStore::new();

    let job = make_job("j-1", "mbox-1");
    store.enqueue(&job).await.unwrap();
    let claimed = store
        .claim("mbox-1", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let claim_token = claimed[0].claim_token.clone().unwrap();

    store.fail_nack.store(true, Ordering::SeqCst);
    let result = store
        .nack("j-1", &claim_token, 5000, "processing error", 2000)
        .await;
    assert!(result.is_err());

    // Job remains Claimed
    let loaded = store.load_job("j-1").await.unwrap().unwrap();
    assert_eq!(loaded.status, MailboxJobStatus::Claimed);
}

// ============================================================================
// Enqueue failure does not affect existing jobs
// ============================================================================

#[tokio::test]
async fn enqueue_failure_does_not_corrupt_existing_jobs() {
    let store = FailingMailboxStore::new();

    // Successfully enqueue first job
    let job1 = make_job("j-1", "mbox-1");
    store.enqueue(&job1).await.unwrap();

    // Fail second enqueue
    store.fail_enqueue.store(true, Ordering::SeqCst);
    let job2 = make_job("j-2", "mbox-1");
    assert!(store.enqueue(&job2).await.is_err());

    // First job is still intact
    let loaded = store.load_job("j-1").await.unwrap().unwrap();
    assert_eq!(loaded.job_id, "j-1");
    assert_eq!(loaded.status, MailboxJobStatus::Queued);

    // Second job was never persisted
    assert!(store.load_job("j-2").await.unwrap().is_none());
}

// ============================================================================
// Claim failure after enqueue — job remains claimable after recovery
// ============================================================================

#[tokio::test]
async fn job_remains_claimable_after_claim_failure_recovery() {
    let store = FailingMailboxStore::new();

    let job = make_job("j-1", "mbox-1");
    store.enqueue(&job).await.unwrap();

    // Fail claim
    store.fail_claim.store(true, Ordering::SeqCst);
    assert!(
        store
            .claim("mbox-1", "consumer-1", 30_000, 1000, 1)
            .await
            .is_err()
    );

    // Recover
    store.fail_claim.store(false, Ordering::SeqCst);
    let claimed = store
        .claim("mbox-1", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job_id, "j-1");
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
        result: None,
        termination: awaken_contract::contract::lifecycle::TerminationReason::NaturalEnd,
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
    use awaken_contract::contract::event_sink::VecEventSink;

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
