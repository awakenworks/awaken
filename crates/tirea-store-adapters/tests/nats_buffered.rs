//! Integration tests for NatsBufferedThreadWriter using testcontainers.
//!
//! Requires Docker. Run with:
//! ```bash
//! cargo test --package tirea-store-adapters --features nats --test nats_buffered -- --nocapture
//! ```

#![cfg(feature = "nats")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use testcontainers::runners::AsyncRunner;
use testcontainers::ImageExt;
use testcontainers_modules::nats::Nats;
use tirea_contract::storage::VersionPrecondition;
use tirea_contract::thread::ThreadChangeSet;
use tirea_contract::{
    CheckpointReason, Committed, Message, MessageQuery, Thread, ThreadListPage, ThreadListQuery,
    ThreadReader, ThreadStoreError, ThreadWriter,
};
use tirea_store_adapters::{MemoryStore, NatsBufferedThreadWriter};

async fn start_nats_js() -> Option<(testcontainers::ContainerAsync<Nats>, String)> {
    let container = match Nats::default().with_cmd(["-js"]).start().await {
        Ok(container) => container,
        Err(err) => {
            eprintln!("skipping nats_buffered test: unable to start NATS container ({err})");
            return None;
        }
    };
    let host = container.get_host().await.expect("failed to get host");
    let port = container
        .get_host_port_ipv4(4222)
        .await
        .expect("failed to get port");
    let url = format!("{host}:{port}");
    Some((container, url))
}

async fn make_storage(nats_url: &str) -> (Arc<MemoryStore>, NatsBufferedThreadWriter) {
    let inner = Arc::new(MemoryStore::new());
    let nats_client = async_nats::connect(nats_url).await.unwrap();
    let js = async_nats::jetstream::new(nats_client);
    let storage = NatsBufferedThreadWriter::new(inner.clone(), js)
        .await
        .unwrap();
    (inner, storage)
}

#[derive(Clone)]
struct CountingDelayStore {
    inner: Arc<MemoryStore>,
    append_calls: Arc<AtomicUsize>,
    save_calls: Arc<AtomicUsize>,
    append_delay: Duration,
    save_delay: Duration,
}

impl CountingDelayStore {
    fn new(append_delay: Duration, save_delay: Duration) -> Self {
        Self {
            inner: Arc::new(MemoryStore::new()),
            append_calls: Arc::new(AtomicUsize::new(0)),
            save_calls: Arc::new(AtomicUsize::new(0)),
            append_delay,
            save_delay,
        }
    }

    fn append_calls(&self) -> usize {
        self.append_calls.load(Ordering::Relaxed)
    }

    fn save_calls(&self) -> usize {
        self.save_calls.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ThreadReader for CountingDelayStore {
    async fn load(
        &self,
        thread_id: &str,
    ) -> Result<Option<tirea_contract::storage::ThreadHead>, ThreadStoreError> {
        self.inner.load(thread_id).await
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        self.inner.list_threads(query).await
    }
}

#[async_trait::async_trait]
impl ThreadWriter for CountingDelayStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        self.inner.create(thread).await
    }

    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        self.append_calls.fetch_add(1, Ordering::Relaxed);
        if !self.append_delay.is_zero() {
            tokio::time::sleep(self.append_delay).await;
        }
        self.inner.append(thread_id, delta, precondition).await
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        self.inner.delete(thread_id).await
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        self.save_calls.fetch_add(1, Ordering::Relaxed);
        if !self.save_delay.is_zero() {
            tokio::time::sleep(self.save_delay).await;
        }
        self.inner.save(thread).await
    }
}

#[derive(Clone)]
struct FailFirstSaveStore {
    inner: Arc<MemoryStore>,
    fail_next_save: Arc<AtomicBool>,
    save_calls: Arc<AtomicUsize>,
}

impl FailFirstSaveStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemoryStore::new()),
            fail_next_save: Arc::new(AtomicBool::new(true)),
            save_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn save_calls(&self) -> usize {
        self.save_calls.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ThreadReader for FailFirstSaveStore {
    async fn load(
        &self,
        thread_id: &str,
    ) -> Result<Option<tirea_contract::storage::ThreadHead>, ThreadStoreError> {
        self.inner.load(thread_id).await
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        self.inner.list_threads(query).await
    }
}

#[async_trait::async_trait]
impl ThreadWriter for FailFirstSaveStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        self.inner.create(thread).await
    }

    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        self.inner.append(thread_id, delta, precondition).await
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        self.inner.delete(thread_id).await
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        self.save_calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_next_save.swap(false, Ordering::AcqRel) {
            return Err(ThreadStoreError::Io(std::io::Error::other(
                "injected save failure",
            )));
        }
        self.inner.save(thread).await
    }
}

#[tokio::test]
async fn test_create_delegates_to_inner() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t1");
    storage.create(&thread).await.unwrap();

    let loaded = inner.load("t1").await.unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().thread.id, "t1");
}

#[tokio::test]
async fn test_append_does_not_write_to_inner() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t1").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    let delta = ThreadChangeSet {
        run_id: "r1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("world"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };

    // append() publishes to NATS, not to inner storage
    storage
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // Inner should still have only the original message
    let loaded = inner.load("t1").await.unwrap().unwrap();
    assert_eq!(loaded.thread.messages.len(), 1);
    assert_eq!(loaded.thread.messages[0].content, "hello");
}

#[tokio::test]
async fn test_save_flushes_to_inner_and_purges_nats() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t1").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    let delta = ThreadChangeSet {
        run_id: "r1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("world"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // Build final thread with both messages
    let final_thread = Thread::new("t1")
        .with_message(Message::user("hello"))
        .with_message(Message::assistant("world"));

    // save() should write to inner storage
    storage.save(&final_thread).await.unwrap();

    let loaded = inner.load("t1").await.unwrap().unwrap();
    assert_eq!(loaded.thread.messages.len(), 2);
    assert_eq!(loaded.thread.messages[0].content, "hello");
    assert_eq!(loaded.thread.messages[1].content, "world");
}

#[tokio::test]
async fn test_load_delegates_to_inner() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    assert!(storage.load("nonexistent").await.unwrap().is_none());

    let thread = Thread::new("t1");
    inner.create(&thread).await.unwrap();

    let loaded = storage.load("t1").await.unwrap();
    assert!(loaded.is_some());
}

#[tokio::test]
async fn test_delete_delegates_to_inner() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t1");
    inner.create(&thread).await.unwrap();

    storage.delete("t1").await.unwrap();

    assert!(inner.load("t1").await.unwrap().is_none());
}

#[tokio::test]
async fn test_recover_replays_unacked_deltas() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    // Create thread in inner storage
    let thread = Thread::new("t1").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    // Publish deltas via append (these go to NATS)
    let delta1 = ThreadChangeSet {
        run_id: "r1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("response 1"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    let delta2 = ThreadChangeSet {
        run_id: "r1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::ToolResultsCommitted,
        messages: vec![Arc::new(Message::assistant("response 2"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t1", &delta1, VersionPrecondition::Any)
        .await
        .unwrap();
    storage
        .append("t1", &delta2, VersionPrecondition::Any)
        .await
        .unwrap();

    // Simulate crash — don't call save(), just recover
    let recovered = storage.recover().await.unwrap();

    assert_eq!(recovered, 2);

    // Inner storage should now have all messages
    let loaded = inner.load("t1").await.unwrap().unwrap();
    assert_eq!(loaded.thread.messages.len(), 3); // hello + response 1 + response 2
}

// ============================================================================
// CQRS consistency tests — queries read from last-flushed snapshot
// ============================================================================

/// During an active run, load() returns the pre-run snapshot without any
/// buffered deltas.  This is the designed CQRS behaviour: real-time data
/// is delivered through the SSE event stream, queries read durable storage.
#[tokio::test]
async fn test_query_returns_last_flush_snapshot_during_active_run() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    // Simulate a completed first run: thread with 1 user + 1 assistant msg.
    let thread = Thread::new("t1")
        .with_message(Message::user("hello"))
        .with_message(Message::assistant("first reply"));
    inner.create(&thread).await.unwrap();

    // Second run starts — new deltas go to NATS only.
    let delta = ThreadChangeSet {
        run_id: "r2".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("second reply"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // Query via NatsBufferedThreadWriter.load() — should see first-run snapshot.
    let head = storage.load("t1").await.unwrap().unwrap();
    assert_eq!(
        head.thread.messages.len(),
        2,
        "load() should return the pre-run snapshot (2 messages), not include buffered delta"
    );
    assert_eq!(head.thread.messages[0].content, "hello");
    assert_eq!(head.thread.messages[1].content, "first reply");
}

/// load_messages() (ThreadReader default) also reads from the inner storage,
/// so during a run it returns the last-flushed message list.
#[tokio::test]
async fn test_load_messages_returns_last_flush_snapshot_during_active_run() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t1")
        .with_message(Message::user("msg-0"))
        .with_message(Message::assistant("msg-1"));
    inner.create(&thread).await.unwrap();

    // Buffer 2 new deltas via NATS.
    for i in 2..4 {
        let delta = ThreadChangeSet {
            run_id: "r2".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason: CheckpointReason::AssistantTurnCommitted,
            messages: vec![Arc::new(Message::assistant(format!("msg-{i}")))],
            patches: vec![],
            actions: vec![],
            snapshot: None,
        };
        storage
            .append("t1", &delta, VersionPrecondition::Any)
            .await
            .unwrap();
    }

    // Query messages through the inner storage (which NatsBufferedThreadWriter delegates to).
    let page = ThreadReader::load_messages(inner.as_ref(), "t1", &MessageQuery::default())
        .await
        .unwrap();
    assert_eq!(
        page.messages.len(),
        2,
        "load_messages() should return 2 messages from last flush, not 4"
    );
}

/// After save() completes, queries immediately see the flushed data.
#[tokio::test]
async fn test_query_accurate_after_run_end_flush() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    // First run: create thread with 1 message.
    let thread = Thread::new("t1").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    // Run produces deltas buffered in NATS.
    let delta = ThreadChangeSet {
        run_id: "r1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("world"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t1", &delta, VersionPrecondition::Any)
        .await
        .unwrap();

    // Before flush: load sees 1 message.
    let pre = storage.load("t1").await.unwrap().unwrap();
    assert_eq!(pre.thread.messages.len(), 1);

    // Run-end flush.
    let final_thread = Thread::new("t1")
        .with_message(Message::user("hello"))
        .with_message(Message::assistant("world"));
    storage.save(&final_thread).await.unwrap();

    // After flush: load sees 2 messages.
    let post = storage.load("t1").await.unwrap().unwrap();
    assert_eq!(post.thread.messages.len(), 2);
    assert_eq!(post.thread.messages[1].content, "world");

    // load_messages also sees 2.
    let page = ThreadReader::load_messages(inner.as_ref(), "t1", &MessageQuery::default())
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 2);
}

/// Across two sequential runs: during the second run, queries return the
/// first run's fully flushed state.
#[tokio::test]
async fn test_multi_run_query_sees_previous_run_data() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    // === Run 1 ===
    let thread = Thread::new("t1").with_message(Message::user("q1"));
    inner.create(&thread).await.unwrap();

    let delta1 = ThreadChangeSet {
        run_id: "r1".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("a1"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t1", &delta1, VersionPrecondition::Any)
        .await
        .unwrap();

    // Flush run 1.
    let run1_thread = Thread::new("t1")
        .with_message(Message::user("q1"))
        .with_message(Message::assistant("a1"));
    storage.save(&run1_thread).await.unwrap();

    // === Run 2 (in progress) ===
    let delta2 = ThreadChangeSet {
        run_id: "r2".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("a2"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t1", &delta2, VersionPrecondition::Any)
        .await
        .unwrap();

    // Query during run 2: sees run 1's flushed state (2 messages), not run 2's delta.
    let head = storage.load("t1").await.unwrap().unwrap();
    assert_eq!(
        head.thread.messages.len(),
        2,
        "during run 2, query should see run 1's flushed state (q1 + a1)"
    );
    assert_eq!(head.thread.messages[0].content, "q1");
    assert_eq!(head.thread.messages[1].content, "a1");

    // Flush run 2.
    let run2_thread = Thread::new("t1")
        .with_message(Message::user("q1"))
        .with_message(Message::assistant("a1"))
        .with_message(Message::assistant("a2"));
    storage.save(&run2_thread).await.unwrap();

    // Now query sees all 3 messages.
    let head = storage.load("t1").await.unwrap().unwrap();
    assert_eq!(head.thread.messages.len(), 3);
}

#[tokio::test]
async fn test_run_finished_append_auto_flushes_to_inner() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t-auto").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    let delta1 = ThreadChangeSet {
        run_id: "r-auto".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("first"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t-auto", &delta1, VersionPrecondition::Any)
        .await
        .unwrap();

    let pre = inner.load("t-auto").await.unwrap().unwrap();
    assert_eq!(pre.thread.messages.len(), 1);

    let delta2 = ThreadChangeSet {
        run_id: "r-auto".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![Arc::new(Message::assistant("second"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t-auto", &delta2, VersionPrecondition::Any)
        .await
        .unwrap();

    let post = inner.load("t-auto").await.unwrap().unwrap();
    assert_eq!(post.thread.messages.len(), 3);
    assert_eq!(post.thread.messages[0].content, "hello");
    assert_eq!(post.thread.messages[1].content, "first");
    assert_eq!(post.thread.messages[2].content, "second");
}

#[tokio::test]
async fn test_buffered_vs_direct_write_latency_and_amplification() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };

    let rounds = 12usize;
    let write_delay = Duration::from_millis(25);

    let direct = Arc::new(CountingDelayStore::new(write_delay, write_delay));
    direct
        .create(&Thread::new("direct").with_message(Message::user("u0")))
        .await
        .unwrap();
    let t0 = Instant::now();
    for i in 0..rounds {
        let reason = if i + 1 == rounds {
            CheckpointReason::RunFinished
        } else {
            CheckpointReason::AssistantTurnCommitted
        };
        let delta = ThreadChangeSet {
            run_id: "r-compare".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason,
            messages: vec![Arc::new(Message::assistant(format!("d{i}")))],
            patches: vec![],
            actions: vec![],
            snapshot: None,
        };
        direct
            .append("direct", &delta, VersionPrecondition::Any)
            .await
            .unwrap();
    }
    let direct_elapsed = t0.elapsed();

    let nats_client = async_nats::connect(&url).await.unwrap();
    let jetstream = async_nats::jetstream::new(nats_client);
    let buffered_inner = Arc::new(CountingDelayStore::new(write_delay, write_delay));
    let buffered = NatsBufferedThreadWriter::new(buffered_inner.clone(), jetstream)
        .await
        .unwrap();
    buffered
        .create(&Thread::new("buffered").with_message(Message::user("u0")))
        .await
        .unwrap();

    let t1 = Instant::now();
    for i in 0..rounds {
        let reason = if i + 1 == rounds {
            CheckpointReason::RunFinished
        } else {
            CheckpointReason::AssistantTurnCommitted
        };
        let delta = ThreadChangeSet {
            run_id: "r-compare".to_string(),
            parent_run_id: None,
            run_meta: None,
            reason,
            messages: vec![Arc::new(Message::assistant(format!("b{i}")))],
            patches: vec![],
            actions: vec![],
            snapshot: None,
        };
        buffered
            .append("buffered", &delta, VersionPrecondition::Any)
            .await
            .unwrap();
    }
    let buffered_elapsed = t1.elapsed();
    println!(
        "direct_elapsed={direct_elapsed:?}, buffered_elapsed={buffered_elapsed:?}, rounds={rounds}"
    );

    let direct_loaded = direct.load("direct").await.unwrap().unwrap();
    let buffered_loaded = buffered_inner.load("buffered").await.unwrap().unwrap();
    assert_eq!(
        direct_loaded.thread.messages.len(),
        buffered_loaded.thread.messages.len()
    );

    assert_eq!(direct.append_calls(), rounds);
    assert_eq!(direct.save_calls(), 0);
    assert_eq!(buffered_inner.append_calls(), 0);
    assert_eq!(buffered_inner.save_calls(), 1);

    assert!(
        buffered_elapsed < direct_elapsed,
        "expected buffered write path to be faster with delayed inner storage; direct={direct_elapsed:?}, buffered={buffered_elapsed:?}"
    );
}

#[tokio::test]
async fn test_run_finished_flush_failure_can_be_recovered() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };

    let inner = Arc::new(FailFirstSaveStore::new());
    let nats_client = async_nats::connect(&url).await.unwrap();
    let jetstream = async_nats::jetstream::new(nats_client);
    let storage = NatsBufferedThreadWriter::new(inner.clone(), jetstream)
        .await
        .unwrap();

    let thread = Thread::new("t-fail").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    let delta1 = ThreadChangeSet {
        run_id: "r-fail".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::AssistantTurnCommitted,
        messages: vec![Arc::new(Message::assistant("step"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t-fail", &delta1, VersionPrecondition::Any)
        .await
        .unwrap();

    let delta2 = ThreadChangeSet {
        run_id: "r-fail".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![Arc::new(Message::assistant("finish"))],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    let err = storage
        .append("t-fail", &delta2, VersionPrecondition::Any)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ThreadStoreError::Io(_)),
        "expected injected save failure, got: {err:?}"
    );

    let before_recover = inner.load("t-fail").await.unwrap().unwrap();
    assert_eq!(before_recover.thread.messages.len(), 1);
    assert_eq!(inner.save_calls(), 1);

    let recovered = storage.recover().await.unwrap();
    assert_eq!(recovered, 2);

    let after_recover = inner.load("t-fail").await.unwrap().unwrap();
    assert_eq!(after_recover.thread.messages.len(), 3);
    assert_eq!(after_recover.thread.messages[0].content, "hello");
    assert_eq!(after_recover.thread.messages[1].content, "step");
    assert_eq!(after_recover.thread.messages[2].content, "finish");

    let recovered_again = storage.recover().await.unwrap();
    assert_eq!(recovered_again, 0);
}

#[tokio::test]
async fn test_duplicate_run_finished_delta_is_idempotent_by_message_id() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;

    let thread = Thread::new("t-idempotent").with_message(Message::user("hello"));
    inner.create(&thread).await.unwrap();

    let run_finished = ThreadChangeSet {
        run_id: "r-idempotent".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![Arc::new(
            Message::assistant("done").with_id("msg-run-finished".to_string()),
        )],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };

    storage
        .append("t-idempotent", &run_finished, VersionPrecondition::Any)
        .await
        .unwrap();
    storage
        .append("t-idempotent", &run_finished, VersionPrecondition::Any)
        .await
        .unwrap();

    let loaded = inner.load("t-idempotent").await.unwrap().unwrap();
    assert_eq!(loaded.thread.messages.len(), 2);
    assert_eq!(loaded.thread.messages[0].content, "hello");
    assert_eq!(loaded.thread.messages[1].content, "done");
}

#[tokio::test]
async fn test_concurrent_appends_flush_to_consistent_snapshot() {
    let Some((_container, url)) = start_nats_js().await else {
        return;
    };
    let (inner, storage) = make_storage(&url).await;
    let storage = Arc::new(storage);

    inner
        .create(&Thread::new("t-concurrent").with_message(Message::user("u0")))
        .await
        .unwrap();

    let rounds = 24usize;
    let mut handles = Vec::with_capacity(rounds);
    for i in 0..rounds {
        let storage = Arc::clone(&storage);
        handles.push(tokio::spawn(async move {
            let delta = ThreadChangeSet {
                run_id: "r-concurrent".to_string(),
                parent_run_id: None,
                run_meta: None,
                reason: CheckpointReason::AssistantTurnCommitted,
                messages: vec![Arc::new(
                    Message::assistant(format!("m{i}")).with_id(format!("m-{i}")),
                )],
                patches: vec![],
                actions: vec![],
                snapshot: None,
            };
            storage
                .append("t-concurrent", &delta, VersionPrecondition::Any)
                .await
                .unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let finish = ThreadChangeSet {
        run_id: "r-concurrent".to_string(),
        parent_run_id: None,
        run_meta: None,
        reason: CheckpointReason::RunFinished,
        messages: vec![],
        patches: vec![],
        actions: vec![],
        snapshot: None,
    };
    storage
        .append("t-concurrent", &finish, VersionPrecondition::Any)
        .await
        .unwrap();

    let loaded = inner.load("t-concurrent").await.unwrap().unwrap();
    assert_eq!(loaded.thread.messages.len(), rounds + 1);
}
