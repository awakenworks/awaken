//! Fault injection tests for storage backends.
//!
//! Verifies that the system degrades gracefully when storage operations fail:
//! - FailingStore wraps a real store and injects failures via atomic flags
//! - Concurrent checkpoint failures do not corrupt state

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::{Thread, ThreadMetadata};
use awaken_stores::InMemoryStore;

mod support;
use support::make_run;

// ============================================================================
// FailingStore — wraps InMemoryStore with injectable failures
// ============================================================================

struct FailingStore {
    inner: Arc<InMemoryStore>,
    fail_checkpoint: AtomicBool,
    fail_save_messages: AtomicBool,
    fail_create_run: AtomicBool,
}

impl FailingStore {
    fn new(inner: Arc<InMemoryStore>) -> Self {
        Self {
            inner,
            fail_checkpoint: AtomicBool::new(false),
            fail_save_messages: AtomicBool::new(false),
            fail_create_run: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ThreadStore for FailingStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        self.inner.load_thread(thread_id).await
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(thread).await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(thread_id).await
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.inner.list_threads(offset, limit).await
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(thread_id).await
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        if self.fail_save_messages.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected save_messages failure".into()));
        }
        self.inner.save_messages(thread_id, messages).await
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(thread_id).await
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner.update_thread_metadata(id, metadata).await
    }
}

#[async_trait]
impl RunStore for FailingStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        if self.fail_create_run.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected create_run failure".into()));
        }
        self.inner.create_run(record).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.load_run(run_id).await
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.latest_run(thread_id).await
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.inner.list_runs(query).await
    }
}

#[async_trait]
impl ThreadRunStore for FailingStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        if self.fail_checkpoint.load(Ordering::SeqCst) {
            return Err(StorageError::Io("injected checkpoint failure".into()));
        }
        self.inner.checkpoint(thread_id, messages, run).await
    }
}

// ============================================================================
// Checkpoint failure returns error (does not panic)
// ============================================================================

#[tokio::test]
async fn checkpoint_failure_returns_storage_error() {
    let inner = Arc::new(InMemoryStore::new());
    let store = FailingStore::new(inner);
    store.fail_checkpoint.store(true, Ordering::SeqCst);

    let msgs = vec![Message::user("hello")];
    let run = make_run("r-1", "t-1", 100);

    let result = store.checkpoint("t-1", &msgs, &run).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected checkpoint failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ============================================================================
// Checkpoint failure does not corrupt previously saved data
// ============================================================================

#[tokio::test]
async fn checkpoint_failure_preserves_existing_data() {
    let inner = Arc::new(InMemoryStore::new());
    let store = FailingStore::new(Arc::clone(&inner));

    // Successful checkpoint
    let msgs = vec![Message::user("first")];
    let run = make_run("r-1", "t-1", 100);
    store.checkpoint("t-1", &msgs, &run).await.unwrap();

    // Enable failure
    store.fail_checkpoint.store(true, Ordering::SeqCst);

    // Failed checkpoint
    let msgs2 = vec![Message::user("second")];
    let run2 = make_run("r-2", "t-1", 200);
    let result = store.checkpoint("t-1", &msgs2, &run2).await;
    assert!(result.is_err());

    // Original data is intact (read from inner store)
    let loaded_msgs = inner.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(loaded_msgs.len(), 1);
    assert_eq!(loaded_msgs[0].text(), "first");

    let loaded_run = inner.load_run("r-1").await.unwrap().unwrap();
    assert_eq!(loaded_run.run_id, "r-1");

    // r-2 was never persisted
    assert!(inner.load_run("r-2").await.unwrap().is_none());
}

// ============================================================================
// Toggling failure on/off — recovery after transient failure
// ============================================================================

#[tokio::test]
async fn store_recovers_after_transient_failure() {
    let inner = Arc::new(InMemoryStore::new());
    let store = FailingStore::new(Arc::clone(&inner));

    // Fail first attempt
    store.fail_checkpoint.store(true, Ordering::SeqCst);
    let msgs = vec![Message::user("attempt-1")];
    let run = make_run("r-1", "t-1", 100);
    assert!(store.checkpoint("t-1", &msgs, &run).await.is_err());

    // Recover — disable failure
    store.fail_checkpoint.store(false, Ordering::SeqCst);
    let msgs2 = vec![Message::user("attempt-2")];
    let run2 = make_run("r-2", "t-1", 200);
    store.checkpoint("t-1", &msgs2, &run2).await.unwrap();

    // Data from second attempt is persisted
    let loaded_msgs = inner.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(loaded_msgs.len(), 1);
    assert_eq!(loaded_msgs[0].text(), "attempt-2");
}

// ============================================================================
// save_messages failure propagates correctly
// ============================================================================

#[tokio::test]
async fn save_messages_failure_propagates() {
    let inner = Arc::new(InMemoryStore::new());
    let store = FailingStore::new(inner);
    store.fail_save_messages.store(true, Ordering::SeqCst);

    let result = store.save_messages("t-1", &[Message::user("hello")]).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected save_messages failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ============================================================================
// create_run failure propagates correctly
// ============================================================================

#[tokio::test]
async fn create_run_failure_propagates() {
    let inner = Arc::new(InMemoryStore::new());
    let store = FailingStore::new(inner);
    store.fail_create_run.store(true, Ordering::SeqCst);

    let run = make_run("r-1", "t-1", 100);
    let result = store.create_run(&run).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::Io(msg) => assert!(msg.contains("injected create_run failure")),
        other => panic!("expected Io error, got: {other:?}"),
    }
}

// ============================================================================
// Concurrent checkpoint failures do not corrupt state
// ============================================================================

#[tokio::test]
async fn concurrent_checkpoint_failures_dont_corrupt_state() {
    let inner = Arc::new(InMemoryStore::new());

    // Seed the store with initial data
    let initial_msgs = vec![Message::user("seed")];
    let initial_run = make_run("r-seed", "t-1", 50);
    inner
        .checkpoint("t-1", &initial_msgs, &initial_run)
        .await
        .unwrap();

    let store = Arc::new(FailingStore::new(Arc::clone(&inner)));

    // Spawn 20 concurrent tasks, half of which will fail
    let mut handles = Vec::new();
    for i in 0..20 {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            if i % 2 == 0 {
                // These will fail
                s.fail_checkpoint.store(true, Ordering::SeqCst);
                let msgs = vec![Message::user(format!("fail-{i}"))];
                let run = make_run(&format!("r-fail-{i}"), "t-1", 100 + i);
                let _ = s.checkpoint("t-1", &msgs, &run).await;
                s.fail_checkpoint.store(false, Ordering::SeqCst);
            } else {
                // These will succeed
                let msgs = vec![Message::user(format!("ok-{i}"))];
                let run = make_run(&format!("r-ok-{i}"), "t-1", 100 + i);
                let _ = s.checkpoint("t-1", &msgs, &run).await;
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Messages should exist and be consistent (not corrupted)
    let loaded_msgs = inner.load_messages("t-1").await.unwrap().unwrap();
    assert!(
        !loaded_msgs.is_empty(),
        "messages should not be empty after concurrent ops"
    );

    // All successful runs should be loadable
    for i in (1..20).step_by(2) {
        let run = inner.load_run(&format!("r-ok-{i}")).await.unwrap();
        assert!(run.is_some(), "successful run r-ok-{i} should be persisted");
    }

    // Thread should still be loadable
    let thread = inner.load_thread("t-1").await.unwrap();
    assert!(thread.is_some(), "thread t-1 should still exist");
}

// ============================================================================
// Concurrent mixed operations under failure
// ============================================================================

#[tokio::test]
async fn concurrent_reads_during_write_failures_are_safe() {
    let inner = Arc::new(InMemoryStore::new());

    // Pre-populate
    let thread = Thread::with_id("t-read");
    inner.save_thread(&thread).await.unwrap();
    inner
        .save_messages("t-read", &[Message::user("existing")])
        .await
        .unwrap();

    let store = Arc::new(FailingStore::new(Arc::clone(&inner)));
    store.fail_save_messages.store(true, Ordering::SeqCst);

    let mut handles = Vec::new();

    // Writers that will fail
    for i in 0..5 {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let _ = s
                .save_messages("t-read", &[Message::user(format!("write-{i}"))])
                .await;
        }));
    }

    // Readers that should always succeed
    for _ in 0..5 {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let msgs = s.load_messages("t-read").await.unwrap().unwrap();
            assert_eq!(msgs[0].text(), "existing");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Data unchanged after failed writes
    let msgs = inner.load_messages("t-read").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "existing");
}
