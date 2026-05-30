#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
#![cfg(feature = "nats")]

#[path = "nats_buffered_thread_fixture.rs"]
mod fixture;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunRequestSnapshot, RunStore, StorageError, ThreadParentFilter,
    ThreadQuery, ThreadRunStore, ThreadStore,
};
use awaken_server_contract::thread::{Thread, ThreadMetadata};
use awaken_stores::{InMemoryStore, NatsBufferedThreadStore, ReadConsistency};
use fixture::{NatsFixture, unique_config};
use tokio::sync::Barrier;

/// A ThreadRunStore that wraps InMemoryStore and counts checkpoint calls.
struct CountingStore {
    inner: InMemoryStore,
    checkpoint_count: AtomicUsize,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: InMemoryStore::new(),
            checkpoint_count: AtomicUsize::new(0),
        }
    }

    fn checkpoint_count(&self) -> usize {
        self.checkpoint_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ThreadStore for CountingStore {
    async fn load_thread(&self, id: &str) -> Result<Option<Thread>, StorageError> {
        self.inner.load_thread(id).await
    }
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(thread).await
    }
    async fn delete_thread(&self, id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(id).await
    }
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.inner.list_threads(offset, limit).await
    }
    async fn load_messages(&self, id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(id).await
    }
    async fn save_messages(&self, id: &str, messages: &[Message]) -> Result<(), StorageError> {
        self.inner.save_messages(id, messages).await
    }
    async fn delete_messages(&self, id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(id).await
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
impl RunStore for CountingStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
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
impl ThreadRunStore for CountingStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.checkpoint_count.fetch_add(1, Ordering::SeqCst);
        self.inner.checkpoint(thread_id, messages, run).await
    }
}

fn mk_run(id: &str, thread: &str) -> RunRecord {
    RunRecord {
        run_id: id.into(),
        thread_id: thread.into(),
        agent_id: "a".into(),
        parent_run_id: None,
        resolution_id: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Created,
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

fn mk_child_run(id: &str, thread: &str, parent_thread_id: &str) -> RunRecord {
    let mut run = mk_run(id, thread);
    run.request = Some(RunRequestSnapshot {
        parent_thread_id: Some(parent_thread_id.to_string()),
        ..RunRequestSnapshot::default()
    });
    run
}

#[tokio::test]
async fn coalescing_reduces_inner_checkpoint_count() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(CountingStore::new());
    let inner_probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(300);
    config.flush_batch_size = 64;
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    // Simulate an agent loop: SAME run_id checkpointed across 10 steps.
    let run = mk_run("r1", "t-coalesce");
    for _ in 0..10 {
        store
            .checkpoint("t-coalesce", &[Message::user("msg")], &run)
            .await
            .unwrap();
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    store.force_flush("t-coalesce").await.unwrap();

    let count = inner_probe.checkpoint_count();
    assert!(
        count < 10,
        "expected coalescing to reduce DB writes below 10, got {}",
        count
    );
    assert!(count >= 1, "at least one DB write expected");
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_your_writes_without_waiting() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r1", "t1");
    store
        .checkpoint("t1", &[Message::user("fresh")], &run)
        .await
        .unwrap();

    let msgs = store.load_messages("t1").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn strong_load_run_does_not_return_unflushed_hot_cache() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.read_consistency = ReadConsistency::Strong;
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r-hot-only", "t-hot-only");
    store
        .__test_cache_run_if_newer(&run, 1)
        .await
        .expect("cache hot run");

    let loaded = store.load_run("r-hot-only").await.unwrap();
    assert!(
        loaded.is_none(),
        "Strong load_run must not expose a run that only exists in hot cache"
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_drains_wal_on_reconnect() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let inner_probe = Arc::clone(&inner);

    let stream_name = format!("THREADLOG_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let hot_bucket = format!("hot_{}", uuid::Uuid::now_v7().simple());
    let mk_cfg = || {
        let mut config = awaken_stores::NatsBufferedThreadConfig::new(fixture.url.clone());
        config.stream_name = stream_name.clone();
        config.consumer_name = consumer_name.clone();
        config.hot_bucket = hot_bucket.clone();
        config.flush_interval = Duration::from_millis(100);
        // Short ack_wait so an unacked in-flight message from the dropped store1
        // is redelivered to store2 quickly.
        config.ack_wait = Duration::from_secs(1);
        config
    };

    // store1: connect, publish a checkpoint, then drop *immediately* — with no
    // explicit shutdown — to simulate a crashed instance. The WAL entry is durable
    // in the JetStream stream; whether store1's flusher acked it before dropping
    // is a race, but in either case the data must end up in the inner store.
    let store1 = NatsBufferedThreadStore::connect(Arc::clone(&inner), mk_cfg())
        .await
        .expect("connect 1");
    let run = mk_run("r1", "t-recover");
    store1
        .checkpoint("t-recover", &[Message::user("durable")], &run)
        .await
        .unwrap();
    drop(store1);

    let store2 = NatsBufferedThreadStore::connect(Arc::clone(&inner), mk_cfg())
        .await
        .expect("connect 2");
    // Same durable consumer + unacked message → JetStream redelivers to store2's
    // flusher once ack_wait expires.
    let mut recovered = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if inner_probe.load_run("r1").await.unwrap().is_some() {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "second instance should drain WAL within 5s");
    store2.shutdown().await.unwrap();
}

#[tokio::test]
async fn poison_wal_is_quarantined_without_repeated_redelivery() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(50);
    config.ack_wait = Duration::from_millis(250);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    let stream_seq = store
        .__test_publish_raw_wal("t-poison", br#"not-json"#)
        .await
        .expect("publish raw poison WAL");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let records = store
        .__test_list_poison_wal_records()
        .await
        .expect("load poison WAL quarantine");
    assert_eq!(
        records.len(),
        1,
        "poison WAL should be quarantined exactly once"
    );
    let (key, record) = &records[0];
    assert_eq!(key, &format!("poison.seq.{stream_seq}"));
    assert_eq!(record["reason"], "decode_failed");
    assert_eq!(record["stream_seq"], stream_seq);
    assert_eq!(record["thread_id"], "t-poison");
    assert_eq!(record["payload_len"], 8);
    assert!(
        record["payload_preview_hex"]
            .as_str()
            .is_some_and(|preview| !preview.is_empty()),
        "quarantine record should retain a payload preview"
    );
    assert!(
        inner.load_thread("t-poison").await.unwrap().is_none(),
        "poison WAL must not materialize any inner thread state"
    );

    tokio::time::sleep(Duration::from_millis(400)).await;
    let records_after_ack_wait = store
        .__test_list_poison_wal_records()
        .await
        .expect("reload poison WAL quarantine");
    assert_eq!(
        records_after_ack_wait.len(),
        1,
        "quarantined poison WAL should not keep redelivering after ack"
    );

    store.shutdown().await.unwrap();
}

// Documents the current quarantine-key design: keys include the JetStream
// stream sequence, so two separate publishes of identical malformed bytes
// are stored as TWO distinct quarantine records. Same-message redelivery is
// deduplicated (covered above), but payload-level dedupe is not. If the
// design changes to hash-by-content, this test must change in lockstep.
#[tokio::test]
async fn duplicate_poison_payloads_create_separate_quarantine_records_per_stream_seq() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(50);
    config.ack_wait = Duration::from_millis(250);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    let payload = br#"broken-payload"#;
    let seq1 = store
        .__test_publish_raw_wal("t-dup-poison", payload)
        .await
        .expect("publish 1");
    let seq2 = store
        .__test_publish_raw_wal("t-dup-poison", payload)
        .await
        .expect("publish 2");
    assert_ne!(seq1, seq2, "stream should hand out distinct sequences");

    tokio::time::sleep(Duration::from_millis(500)).await;

    let records = store
        .__test_list_poison_wal_records()
        .await
        .expect("list poison WAL");
    assert_eq!(
        records.len(),
        2,
        "identical payloads across different stream sequences are quarantined separately"
    );
    let keys: Vec<String> = records.iter().map(|(k, _)| k.clone()).collect();
    assert!(
        keys.contains(&format!("poison.seq.{seq1}"))
            && keys.contains(&format!("poison.seq.{seq2}")),
        "both quarantine slots must be keyed by stream seq, got {keys:?}"
    );
    for (_, record) in &records {
        assert_eq!(record["reason"], "decode_failed");
        assert_eq!(record["thread_id"], "t-dup-poison");
        assert_eq!(record["payload_len"], payload.len());
    }

    store.shutdown().await.unwrap();
}

// Regression: a persistent KV outage must NOT trap the consumer in an
// unbounded Nak loop on a poison message. After the bound kicks in
// (`should_drop_poison_on_quarantine_failure`), the flusher acks the
// message so the stream can make progress.
#[tokio::test]
async fn poison_quarantine_nak_loop_terminates_when_kv_bucket_is_deleted() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(50);
    config.ack_wait = Duration::from_millis(200);
    let bucket_name = config.hot_bucket.clone();
    let stream_name = config.stream_name.clone();
    let consumer_name = config.consumer_name.clone();
    let url = fixture.url.clone();

    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    // Nuke the KV bucket so every quarantine `put` will fail from the
    // flusher's side while the stream/consumer remain healthy.
    let admin_client = async_nats::connect(&url).await.expect("admin client");
    let admin_js = async_nats::jetstream::new(admin_client);
    admin_js
        .delete_key_value(&bucket_name)
        .await
        .expect("delete KV bucket");

    // Publish a single poison message. The flusher will try to quarantine,
    // fail repeatedly, and after the bound kicks in it must ack-drop.
    store
        .__test_publish_raw_wal("t-nak-bound", b"not-json-and-kv-is-gone")
        .await
        .expect("publish poison");

    // Wait for 5 * ack_wait (1s) + ample slack so the bound has fired.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Attach a fresh admin handle and verify the consumer is no longer
    // holding the message in its ack-pending set. If the Nak loop were
    // unbounded, this would stay at 1 indefinitely.
    let probe_client = async_nats::connect(&url).await.expect("probe client");
    let probe_js = async_nats::jetstream::new(probe_client);
    let stream = probe_js.get_stream(&stream_name).await.expect("get stream");
    let mut consumer = stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&consumer_name)
        .await
        .expect("get consumer");
    let info = consumer.info().await.expect("consumer info");
    assert_eq!(
        info.num_ack_pending, 0,
        "poison message must be acked after max NAKs; consumer still shows {} pending: {info:?}",
        info.num_ack_pending
    );

    // KV is gone, so internal shutdown housekeeping will fail — tolerate it.
    let _ = store.shutdown().await;
}

#[tokio::test]
async fn checkpoint_reports_commit_unknown_after_durable_wal_commit() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    store
        .__test_fail_checkpoint_after_mark_committed("injected post-commit failure")
        .await;

    let error = store
        .checkpoint(
            "t-commit-unknown",
            &[Message::user("durable")],
            &mk_run("r-durable", "t-commit-unknown"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, StorageError::CommitUnknown(_)),
        "post-commit failures must surface as CommitUnknown: {error:?}"
    );

    store.force_flush("t-commit-unknown").await.unwrap();

    let thread = inner
        .load_thread("t-commit-unknown")
        .await
        .unwrap()
        .expect("thread recovered from committed WAL");
    assert_eq!(thread.latest_run_id.as_deref(), Some("r-durable"));
    let messages = inner
        .load_messages("t-commit-unknown")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(messages[0].text(), "durable");

    store.shutdown().await.unwrap();
}

/// Regression for the concurrent-writer WAL race: when two writers
/// reserve seqs in order (A=20, B=10) but their JetStream publishes
/// arrive in the opposite order (B lands first, A lands second), a
/// reader relying on `get_last_raw_message_by_subject` would return B's
/// content while `latest_seq` is 20 — off-by-one content for the
/// watermark. The fix binds `latest_seq` to the JS stream seq of its
/// own WAL entry; the reader fetches by that JS seq and verifies
/// `thread_seq` matches.
#[tokio::test]
async fn read_your_writes_binds_to_committed_js_seq_not_subject_tail() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let cfg = unique_config(&fixture);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
        .await
        .expect("connect");

    let thread_id = "t-race";
    let run_a = mk_run("run-a", thread_id);
    let run_b = mk_run("run-b", thread_id);

    // Plant entry A (committed thread_seq=20) FIRST so its JS seq is
    // lower, then plant entry B (thread_seq=10) AFTER so it becomes the
    // JS subject-latest. Reader must NOT follow subject-latest.
    let js_seq_a = store
        .__test_plant_wal_entry(thread_id, &run_a, &[Message::user("A-content")], 20)
        .await
        .unwrap();
    let js_seq_b = store
        .__test_plant_wal_entry(thread_id, &run_b, &[Message::user("B-content")], 10)
        .await
        .unwrap();
    assert!(
        js_seq_b > js_seq_a,
        "precondition: B is JS-latest (got A={js_seq_a}, B={js_seq_b})"
    );

    // Force hot_meta to point at A's commit (latest_seq=20 bound to
    // A's JS stream sequence). This is what `promote_latest_seq` would
    // write when A committed "latest".
    store
        .__test_force_hot_meta(thread_id, 20, 20, js_seq_a)
        .await
        .unwrap();

    let messages = store
        .load_messages(thread_id)
        .await
        .unwrap()
        .expect("read-your-writes must return overlay content");
    assert_eq!(
        messages[0].text(),
        "A-content",
        "reader must return the WAL entry bound to latest_seq's JS seq, \
         not the subject-latest entry"
    );
    let latest = store
        .latest_run(thread_id)
        .await
        .unwrap()
        .expect("latest_run must return overlay run");
    assert_eq!(latest.run_id, "run-a");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn checkpoint_serializes_concurrent_cycle_updates() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner.save_thread(&Thread::with_id("a")).await.unwrap();
    inner.save_thread(&Thread::with_id("b")).await.unwrap();

    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_secs(60);
    let store = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
            .await
            .expect("connect"),
    );

    let barrier = Arc::new(Barrier::new(3));
    let spawn_checkpoint = |thread_id: &'static str, parent_thread_id: &'static str| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            store
                .checkpoint(
                    thread_id,
                    &[Message::user("buffered")],
                    &mk_child_run(
                        &format!("run-{thread_id}-to-{parent_thread_id}"),
                        thread_id,
                        parent_thread_id,
                    ),
                )
                .await
        })
    };

    let left = spawn_checkpoint("a", "b");
    let right = spawn_checkpoint("b", "a");
    barrier.wait().await;

    let left = left.await.unwrap();
    let right = right.await.unwrap();
    assert_ne!(left.is_ok(), right.is_ok());

    for thread_id in ["a", "b"] {
        let thread = store.load_thread(thread_id).await.unwrap().unwrap();
        store
            .validate_thread_hierarchy(thread_id, thread.parent_thread_id.as_deref())
            .await
            .unwrap();
    }

    store.force_flush_all_pending().await.unwrap();

    for thread_id in ["a", "b"] {
        let thread = inner.load_thread(thread_id).await.unwrap().unwrap();
        inner
            .validate_thread_hierarchy(thread_id, thread.parent_thread_id.as_deref())
            .await
            .unwrap();
    }

    Arc::into_inner(store)
        .expect("single store owner for shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn checkpoint_validation_ignores_eventual_read_consistency() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner.save_thread(&Thread::with_id("a")).await.unwrap();
    inner.save_thread(&Thread::with_id("b")).await.unwrap();

    let mut config = unique_config(&fixture);
    config.read_consistency = ReadConsistency::Eventual;
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    store
        .checkpoint(
            "a",
            &[Message::user("buffered-a")],
            &mk_child_run("run-a-to-b", "a", "b"),
        )
        .await
        .unwrap();

    let error = store
        .checkpoint(
            "b",
            &[Message::user("buffered-b")],
            &mk_child_run("run-b-to-a", "b", "a"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Validation(_)));

    store.force_flush_all_pending().await.unwrap();

    for thread_id in ["a", "b"] {
        let thread = inner.load_thread(thread_id).await.unwrap().unwrap();
        inner
            .validate_thread_hierarchy(thread_id, thread.parent_thread_id.as_deref())
            .await
            .unwrap();
    }

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn latest_wal_projection_preserves_sticky_parent_across_pending_checkpoints() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner.save_thread(&Thread::with_id("root")).await.unwrap();

    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    store
        .checkpoint(
            "child",
            &[Message::user("attach-child")],
            &mk_child_run("run-child-parented", "child", "root"),
        )
        .await
        .unwrap();
    store
        .checkpoint(
            "child",
            &[Message::user("sticky-parent")],
            &mk_run("run-child-latest", "child"),
        )
        .await
        .unwrap();

    let child = store.load_thread("child").await.unwrap().unwrap();
    assert_eq!(child.parent_thread_id.as_deref(), Some("root"));

    let page = store
        .list_threads_query(&ThreadQuery {
            parent_filter: ThreadParentFilter::Parent("root".to_string()),
            ..ThreadQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0], "child");

    let error = store
        .checkpoint(
            "root",
            &[Message::user("introduce-cycle")],
            &mk_child_run("run-root-to-child", "root", "child"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Validation(_)));

    store.force_flush_all_pending().await.unwrap();

    let flushed_child = inner.load_thread("child").await.unwrap().unwrap();
    assert_eq!(flushed_child.parent_thread_id.as_deref(), Some("root"));

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn same_run_coalescing_flush_preserves_materialized_parent_projection() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner.save_thread(&Thread::with_id("root")).await.unwrap();

    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    store
        .checkpoint(
            "child",
            &[Message::user("same-run-parented")],
            &mk_child_run("same-run", "child", "root"),
        )
        .await
        .unwrap();
    store
        .checkpoint(
            "child",
            &[Message::user("same-run-latest")],
            &mk_run("same-run", "child"),
        )
        .await
        .unwrap();

    store.force_flush("child").await.unwrap();

    let flushed_child = inner.load_thread("child").await.unwrap().unwrap();
    assert_eq!(flushed_child.parent_thread_id.as_deref(), Some("root"));

    let error = store
        .checkpoint(
            "root",
            &[Message::user("introduce-cycle")],
            &mk_child_run("run-root-to-child-after-coalesce", "root", "child"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Validation(_)));

    store.shutdown().await.unwrap();
}

/// Thread state is not buffered through the WAL; the buffered store must
/// delegate `save_thread_state`/`load_thread_state` to the inner durable
/// store so reads observe writes.
#[tokio::test]
async fn thread_state_delegates_to_inner_store() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let inner_probe = Arc::clone(&inner);
    let config = unique_config(&fixture);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let state = awaken_server_contract::PersistedState {
        revision: 5,
        extensions: Default::default(),
    };
    store
        .save_thread_state("t-state", &state)
        .await
        .expect("save thread_state");

    // Visible through the buffered store and persisted to the inner store.
    assert_eq!(
        store.load_thread_state("t-state").await.unwrap(),
        Some(state.clone())
    );
    assert_eq!(
        inner_probe.load_thread_state("t-state").await.unwrap(),
        Some(state)
    );
    assert!(store.load_thread_state("absent").await.unwrap().is_none());

    store.shutdown().await.unwrap();
}
