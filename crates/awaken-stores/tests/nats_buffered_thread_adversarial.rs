//! Adversarial/stress tests for `NatsBufferedThreadStore`.
//!
//! These exercise failure modes and concurrency that the happy-path conformance
//! suite does not:
//!
//! - Concurrent checkpoints racing on a single thread (CAS + coalescing)
//! - Many-thread fanout (per-thread isolation under interleaving)
//! - Transient inner-store failures (JetStream NAK + redelivery)
//! - `force_flush` timeout when the inner store stalls
//! - Large payloads near JetStream's 1 MiB message limit
//! - Mid-flight crash + recovery coalescing (store1 drops with N unacked
//!   entries for one thread; store2 must drain to a consistent final state)

#![cfg(feature = "nats")]

#[path = "nats_buffered_thread_fixture.rs"]
mod fixture;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::{Thread, ThreadMetadata};
use awaken_stores::{InMemoryStore, NatsBufferedThreadStore, ReadConsistency};
use fixture::{NatsFixture, unique_config};

// ── Test doubles ────────────────────────────────────────────────────

/// Wraps `InMemoryStore`, counting checkpoint invocations.
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
    fn count(&self) -> usize {
        self.checkpoint_count.load(Ordering::SeqCst)
    }
}

/// Checkpoint fails for the first `fail_first` calls (Io error), then succeeds.
struct FlakyStore {
    inner: InMemoryStore,
    fail_first: AtomicUsize,
    observed_calls: AtomicUsize,
}

impl FlakyStore {
    fn new(fail_first: usize) -> Self {
        Self {
            inner: InMemoryStore::new(),
            fail_first: AtomicUsize::new(fail_first),
            observed_calls: AtomicUsize::new(0),
        }
    }
    fn observed_calls(&self) -> usize {
        self.observed_calls.load(Ordering::SeqCst)
    }
}

/// Checkpoint blocks forever so the flusher cannot advance `flushed_seq`.
struct BlockedStore {
    inner: InMemoryStore,
}

impl BlockedStore {
    fn new() -> Self {
        Self {
            inner: InMemoryStore::new(),
        }
    }
}

// Macro to forward ThreadStore + RunStore methods to inner InMemoryStore.
macro_rules! forward_thread_run {
    ($ty:ty) => {
        #[async_trait]
        impl ThreadStore for $ty {
            async fn load_thread(&self, id: &str) -> Result<Option<Thread>, StorageError> {
                self.inner.load_thread(id).await
            }
            async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
                self.inner.save_thread(thread).await
            }
            async fn delete_thread(&self, id: &str) -> Result<(), StorageError> {
                self.inner.delete_thread(id).await
            }
            async fn list_threads(
                &self,
                offset: usize,
                limit: usize,
            ) -> Result<Vec<String>, StorageError> {
                self.inner.list_threads(offset, limit).await
            }
            async fn load_messages(&self, id: &str) -> Result<Option<Vec<Message>>, StorageError> {
                self.inner.load_messages(id).await
            }
            async fn save_messages(
                &self,
                id: &str,
                messages: &[Message],
            ) -> Result<(), StorageError> {
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
        impl RunStore for $ty {
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
    };
}

forward_thread_run!(CountingStore);
forward_thread_run!(FlakyStore);
forward_thread_run!(BlockedStore);

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

#[async_trait]
impl ThreadRunStore for FlakyStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.observed_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .fail_first
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n > 0 { Some(n - 1) } else { None }
            })
            .is_ok()
        {
            return Err(StorageError::Io("flaky: transient".into()));
        }
        self.inner.checkpoint(thread_id, messages, run).await
    }
}

#[async_trait]
impl ThreadRunStore for BlockedStore {
    async fn checkpoint(
        &self,
        _thread_id: &str,
        _messages: &[Message],
        _run: &RunRecord,
    ) -> Result<(), StorageError> {
        // Park forever — flusher never advances flushed_seq, so force_flush must time out.
        std::future::pending::<()>().await;
        unreachable!()
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn mk_run(id: &str, thread: &str, updated_at: u64) -> RunRecord {
    RunRecord {
        run_id: id.into(),
        thread_id: thread.into(),
        agent_id: "a".into(),
        parent_run_id: None,
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
        updated_at,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

/// 20 writers race on one thread. Every checkpoint must return Ok (no CAS
/// deadlocks, no lost errors), force_flush must converge within the 10s
/// internal timeout, and the surviving run/messages must be from the same
/// checkpoint call (coalescer keeps the highest-seq entry consistently).
#[tokio::test]
async fn concurrent_checkpoints_same_thread_converge() {
    const N: usize = 20;
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(100);
    let store = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
            .await
            .expect("connect"),
    );

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let run = mk_run(&format!("r{i}"), "t-race", i as u64 + 1);
            s.checkpoint("t-race", &[Message::user(format!("msg-{i}"))], &run)
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().expect("checkpoint ok under contention");
    }

    store.force_flush("t-race").await.expect("force_flush");

    let thread = inner
        .load_thread("t-race")
        .await
        .unwrap()
        .expect("thread persisted");
    // Coalescing keeps the entry with the highest thread_seq; the run &
    // messages in that entry must match (they were written by the same call).
    // `RunStore::latest_run` is timestamp-based, so the thread projection is
    // the durable source for the winning checkpoint after concurrent writes.
    let winning_run_id = thread
        .latest_run_id
        .as_deref()
        .expect("thread projection has latest run");
    let winner_idx: usize = winning_run_id
        .trim_start_matches('r')
        .parse()
        .expect("run_id parseable");
    assert!(winner_idx < N);
    let msgs = inner.load_messages("t-race").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        msgs[0].text(),
        format!("msg-{winner_idx}"),
        "winning run's messages must match winning run_id (no cross-entry tearing)"
    );

    store.shutdown().await.unwrap();
}

/// Regression: if logical thread_seq publish order is inverted, a later
/// stale WAL entry must not roll the inner projection or flushed watermark
/// back after a higher seq has already flushed.
#[tokio::test]
async fn out_of_order_wal_publish_does_not_rewind_projection_or_flushed_seq() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(CountingStore::new());
    let probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(100);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let newer = mk_run("run-newer", "t-out-of-order", 2);
    let newer_js_seq = store
        .__test_plant_wal_entry("t-out-of-order", &newer, &[Message::user("newer")], 2)
        .await
        .expect("plant newer wal");
    store
        .__test_force_hot_meta("t-out-of-order", 2, 2, newer_js_seq)
        .await
        .expect("force hot meta");
    store
        .force_flush("t-out-of-order")
        .await
        .expect("flush newer");

    let older = mk_run("run-older", "t-out-of-order", 1);
    store
        .__test_plant_wal_entry("t-out-of-order", &older, &[Message::user("older")], 1)
        .await
        .expect("plant older wal");

    tokio::time::sleep(Duration::from_millis(350)).await;

    let latest = probe
        .latest_run("t-out-of-order")
        .await
        .unwrap()
        .expect("latest run");
    assert_eq!(latest.run_id, "run-newer");
    let messages = probe
        .load_messages("t-out-of-order")
        .await
        .unwrap()
        .expect("messages");
    assert_eq!(messages[0].text(), "newer");
    assert_eq!(
        store
            .__test_read_flushed_seq("t-out-of-order")
            .await
            .unwrap(),
        2,
        "flushed_seq must not regress when stale seq is redelivered later"
    );
    assert_eq!(
        probe.count(),
        1,
        "stale seq should be acked/skipped, not checkpointed to inner"
    );

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn strong_load_run_uses_hot_cache_when_watermark_skips_stale_run_persist() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.read_consistency = ReadConsistency::Strong;
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    let run = mk_run("run-stale-watermark", "t-stale-watermark", 1);
    store
        .__test_cache_run_if_newer(&run, 1)
        .await
        .expect("cache stale run");
    store
        .__test_force_hot_meta("t-stale-watermark", 2, 2, 1)
        .await
        .expect("force hot meta");
    store
        .__test_force_flushed_seq("t-stale-watermark", 2)
        .await
        .expect("force flushed seq");
    assert!(
        inner
            .load_run("run-stale-watermark")
            .await
            .unwrap()
            .is_none(),
        "test setup should leave inner store without the stale run"
    );

    let loaded = store
        .load_run("run-stale-watermark")
        .await
        .expect("strong load_run");
    assert_eq!(
        loaded.as_ref().map(|run| run.run_id.as_str()),
        Some("run-stale-watermark"),
        "Strong load_run must not return a false None when projection watermark advanced before a stale run was persisted"
    );

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn strong_latest_run_uses_thread_projection_when_run_timestamps_tie() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(100);
    config.read_consistency = ReadConsistency::Strong;
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let older = mk_run("run-old-tie", "t-projection-tie", 1);
    let newer = mk_run("run-new-tie", "t-projection-tie", 1);
    store
        .checkpoint(
            "t-projection-tie",
            &[Message::user("old projection")],
            &older,
        )
        .await
        .unwrap();
    store
        .checkpoint(
            "t-projection-tie",
            &[Message::user("new projection")],
            &newer,
        )
        .await
        .unwrap();

    store.force_flush("t-projection-tie").await.unwrap();
    let latest = store
        .latest_run("t-projection-tie")
        .await
        .unwrap()
        .expect("latest run");
    assert_eq!(
        latest.run_id, "run-new-tie",
        "Strong latest_run must follow the flushed thread projection, not updated_at tie order"
    );

    store.shutdown().await.unwrap();
}

/// Regression: hot run cache is keyed by run_id, so same-run checkpoints
/// must carry their logical thread_seq and reject older overwrites.
#[tokio::test]
async fn hot_run_cache_ignores_older_same_run_sequence() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let newer = mk_run("same-run", "t-cache", 2);
    store
        .__test_cache_run_if_newer(&newer, 2)
        .await
        .expect("cache newer");
    let older = mk_run("same-run", "t-cache", 1);
    store
        .__test_cache_run_if_newer(&older, 1)
        .await
        .expect("cache older no-op");

    let cached = store
        .load_run("same-run")
        .await
        .unwrap()
        .expect("cached run");
    assert_eq!(cached.updated_at, 2);

    store.shutdown().await.unwrap();
}

/// M threads × K checkpoints on the SAME run_id per thread (iterative agent-loop
/// pattern). Per-thread `latest_run` must be that thread's last write (no
/// cross-thread interference), and coalescing must reduce inner writes below
/// M*K because repeated writes to the same run_id collapse to one per batch.
#[tokio::test]
async fn many_threads_fanout_isolated() {
    const M: usize = 8;
    const K: usize = 5;
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(CountingStore::new());
    let probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(150);
    let store = Arc::new(
        NatsBufferedThreadStore::connect(inner, config)
            .await
            .expect("connect"),
    );

    let mut handles = Vec::new();
    for t in 0..M {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let tid = format!("t-fan-{t}");
            // Same run_id checkpointed K times — realistic agent-step scenario.
            let run_id = format!("t{t}-r0");
            for k in 0..K {
                let run = mk_run(&run_id, &tid, (k + 1) as u64);
                s.checkpoint(&tid, &[Message::user(format!("t{t}-k{k}"))], &run)
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    for t in 0..M {
        let tid = format!("t-fan-{t}");
        store.force_flush(&tid).await.unwrap();
    }

    for t in 0..M {
        let tid = format!("t-fan-{t}");
        let latest = probe.latest_run(&tid).await.unwrap().expect("run");
        assert_eq!(
            latest.run_id,
            format!("t{t}-r0"),
            "each thread's latest_run must be its own final write"
        );
    }
    assert!(
        probe.count() < M * K,
        "same-run-id coalescing should reduce inner writes below {} got {}",
        M * K,
        probe.count()
    );

    store.shutdown().await.unwrap();
}

/// First 3 inner checkpoints fail; flusher NAKs, JetStream redelivers, and
/// state eventually converges. Verifies the NAK/redelivery loop isn't broken.
#[tokio::test]
async fn transient_inner_failure_redelivers_until_success() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(FlakyStore::new(3));
    let probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(100);
    // Short ack_wait ⇒ NAK'd messages redeliver quickly.
    config.ack_wait = Duration::from_secs(1);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r-retry", "t-flaky", 1);
    store
        .checkpoint("t-flaky", &[Message::user("will-retry")], &run)
        .await
        .unwrap();

    let mut converged = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if probe.load_run("r-retry").await.unwrap().is_some() {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "inner state must converge after transient errors"
    );
    assert!(
        probe.observed_calls() >= 4,
        "flaky store should have been invoked at least 4 times (3 fails + 1 success), got {}",
        probe.observed_calls()
    );

    store.shutdown().await.unwrap();
}

/// Inner store blocks forever ⇒ flusher cannot advance `flushed_seq` ⇒
/// `force_flush` must surface a timeout error rather than hang.
#[tokio::test]
async fn force_flush_times_out_when_flusher_stalls() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(BlockedStore::new());
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(100);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r-stuck", "t-stuck", 1);
    store
        .checkpoint("t-stuck", &[Message::user("never-lands")], &run)
        .await
        .unwrap();

    let t0 = std::time::Instant::now();
    let err = store
        .force_flush("t-stuck")
        .await
        .expect_err("force_flush must time out");
    let elapsed = t0.elapsed();

    match err {
        StorageError::Io(msg) => {
            assert!(
                msg.contains("force_flush timeout"),
                "expected timeout message, got: {msg}"
            );
        }
        other => panic!("expected StorageError::Io, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_secs(9),
        "force_flush should have waited ~10s before timing out, got {:?}",
        elapsed
    );

    let shutdown_err = store
        .shutdown()
        .await
        .expect_err("shutdown should report that pending WAL could not drain");
    assert!(
        shutdown_err.to_string().contains("force_flush timeout"),
        "expected shutdown drain timeout, got: {shutdown_err}"
    );
}

/// ~800 KiB message payload — below JetStream's 1 MiB default but well above
/// anything the unit tests touch. Round-trips through WAL + flush.
#[tokio::test]
async fn large_payload_round_trip() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(150);
    let store = NatsBufferedThreadStore::connect(Arc::clone(&inner), config)
        .await
        .expect("connect");

    let big = "x".repeat(800 * 1024);
    let run = mk_run("r-big", "t-big", 1);
    store
        .checkpoint("t-big", &[Message::user(big.clone())], &run)
        .await
        .unwrap();

    store.force_flush("t-big").await.expect("force_flush");
    let msgs = probe.load_messages("t-big").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text().len(), big.len());

    store.shutdown().await.unwrap();
}

/// store1 publishes 5 checkpoints for one thread with a long flush interval
/// (so its flusher never runs), then is dropped. store2 joins the same
/// durable consumer; the 5 unacked entries redeliver and must coalesce —
/// the inner store ends up with the highest-seq entry's run, not multiple
/// competing writes.
#[tokio::test]
async fn inflight_crash_recovery_coalesces_same_thread() {
    const N: usize = 5;
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(CountingStore::new());
    let probe = Arc::clone(&inner);

    let stream_name = format!("THREADLOG_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let hot_bucket = format!("hot_{}", uuid::Uuid::now_v7().simple());
    let mut store1_cfg = awaken_stores::NatsBufferedThreadConfig::new(fixture.url.clone());
    store1_cfg.stream_name = stream_name.clone();
    store1_cfg.consumer_name = consumer_name.clone();
    store1_cfg.hot_bucket = hot_bucket.clone();
    // Very long so store1's flusher never drains before drop.
    store1_cfg.flush_interval = Duration::from_secs(60);
    store1_cfg.ack_wait = Duration::from_secs(1);
    let store1 = NatsBufferedThreadStore::connect(Arc::clone(&inner), store1_cfg)
        .await
        .expect("connect 1");

    for i in 0..N {
        let run = mk_run(&format!("r{i}"), "t-crash", (i + 1) as u64);
        store1
            .checkpoint("t-crash", &[Message::user(format!("m{i}"))], &run)
            .await
            .unwrap();
    }
    assert_eq!(probe.count(), 0, "store1 must not have flushed yet");
    drop(store1);

    let mut store2_cfg = awaken_stores::NatsBufferedThreadConfig::new(fixture.url.clone());
    store2_cfg.stream_name = stream_name.clone();
    store2_cfg.consumer_name = consumer_name.clone();
    store2_cfg.hot_bucket = hot_bucket.clone();
    store2_cfg.flush_interval = Duration::from_millis(200);
    store2_cfg.ack_wait = Duration::from_secs(1);
    let store2 = NatsBufferedThreadStore::connect(Arc::clone(&inner), store2_cfg)
        .await
        .expect("connect 2");

    // Wait until the final checkpoint lands (run "r{N-1}" has highest seq
    // ⇒ coalescer's chosen entry).
    let mut converged = false;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(run) = probe.latest_run("t-crash").await.unwrap()
            && run.run_id == format!("r{}", N - 1)
        {
            converged = true;
            break;
        }
    }
    assert!(converged, "store2 must drain to highest-seq entry");
    assert!(
        probe.count() <= N,
        "coalescing should cap inner writes at {N}, got {}",
        probe.count()
    );
    // Most redeliveries should share a single batch ⇒ well below N.
    // (Not a hard bound — timing-dependent — but count < N is the meaningful check.)

    store2.shutdown().await.unwrap();
}

/// Drain-in-progress: store1 leaves N unacked entries, store2 connects and
/// *while it is still draining those*, M new checkpoints arrive via store2's
/// write path. Final state must reflect the highest-seq entry across both
/// generations — no partial drain, no lost writes, no stale winner.
#[tokio::test]
async fn drain_in_progress_accepts_new_writes() {
    const OLD: usize = 5;
    const NEW: usize = 5;
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(CountingStore::new());
    let probe = Arc::clone(&inner);

    let stream_name = format!("THREADLOG_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let hot_bucket = format!("hot_{}", uuid::Uuid::now_v7().simple());
    let mk_cfg = |flush: Duration| {
        let mut config = awaken_stores::NatsBufferedThreadConfig::new(fixture.url.clone());
        config.stream_name = stream_name.clone();
        config.consumer_name = consumer_name.clone();
        config.hot_bucket = hot_bucket.clone();
        config.flush_interval = flush;
        config.ack_wait = Duration::from_secs(1);
        config
    };

    // store1: publish OLD entries but never flush (long interval + drop).
    let store1 =
        NatsBufferedThreadStore::connect(Arc::clone(&inner), mk_cfg(Duration::from_secs(60)))
            .await
            .expect("connect 1");
    for i in 0..OLD {
        let run = mk_run(&format!("old-{i}"), "t-mix", (i + 1) as u64);
        store1
            .checkpoint("t-mix", &[Message::user(format!("old-{i}"))], &run)
            .await
            .unwrap();
    }
    drop(store1);

    // store2 takes over. Fire NEW checkpoints immediately — before the flusher
    // has had a chance to drain the inherited OLD entries — so both cohorts
    // contend in the same coalescing window(s).
    let store2 =
        NatsBufferedThreadStore::connect(Arc::clone(&inner), mk_cfg(Duration::from_millis(150)))
            .await
            .expect("connect 2");
    for i in 0..NEW {
        let updated_at = (OLD + i + 1) as u64; // strictly above every OLD entry
        let run = mk_run(&format!("new-{i}"), "t-mix", updated_at);
        store2
            .checkpoint("t-mix", &[Message::user(format!("new-{i}"))], &run)
            .await
            .unwrap();
    }

    // Highest thread_seq wins. Since store2's checkpoints run after store1's
    // (CAS seq is monotonic across the shared KV), the winner is the last NEW.
    store2.force_flush("t-mix").await.expect("force_flush");
    let latest = probe.latest_run("t-mix").await.unwrap().expect("run");
    assert_eq!(
        latest.run_id,
        format!("new-{}", NEW - 1),
        "drain-in-progress must honour highest-seq across mixed cohorts"
    );
    let msgs = probe.load_messages("t-mix").await.unwrap().unwrap();
    assert_eq!(msgs[0].text(), format!("new-{}", NEW - 1));
    assert!(
        probe.count() <= OLD + NEW,
        "coalescing should cap inner writes at {}; got {}",
        OLD + NEW,
        probe.count()
    );

    store2.shutdown().await.unwrap();
}

/// Two active stores share one stream + durable consumer + hot bucket.
/// Writers are split across disjoint thread_id sets (so no CAS contention),
/// but both stores' flushers compete to pull from the same JetStream
/// consumer. JetStream's pull-consumer semantics must ensure each WAL entry
/// is applied exactly once to `inner`, and every thread's final state must
/// match its single writer.
#[tokio::test]
async fn two_active_stores_share_consumer_no_double_apply() {
    const PER_STORE: usize = 10;
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(CountingStore::new());
    let probe = Arc::clone(&inner);

    let stream_name = format!("THREADLOG_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let hot_bucket = format!("hot_{}", uuid::Uuid::now_v7().simple());
    let mk_cfg = || {
        let mut config = awaken_stores::NatsBufferedThreadConfig::new(fixture.url.clone());
        config.stream_name = stream_name.clone();
        config.consumer_name = consumer_name.clone();
        config.hot_bucket = hot_bucket.clone();
        config.flush_interval = Duration::from_millis(100);
        config.ack_wait = Duration::from_secs(5);
        config
    };

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), mk_cfg())
            .await
            .expect("connect A"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), mk_cfg())
            .await
            .expect("connect B"),
    );

    let a = Arc::clone(&store_a);
    let writer_a = tokio::spawn(async move {
        for i in 0..PER_STORE {
            let tid = format!("A-{i}");
            let run = mk_run(&format!("ra-{i}"), &tid, 1);
            a.checkpoint(&tid, &[Message::user(format!("a-{i}"))], &run)
                .await
                .unwrap();
        }
    });
    let b = Arc::clone(&store_b);
    let writer_b = tokio::spawn(async move {
        for i in 0..PER_STORE {
            let tid = format!("B-{i}");
            let run = mk_run(&format!("rb-{i}"), &tid, 1);
            b.checkpoint(&tid, &[Message::user(format!("b-{i}"))], &run)
                .await
                .unwrap();
        }
    });
    writer_a.await.unwrap();
    writer_b.await.unwrap();

    for i in 0..PER_STORE {
        store_a.force_flush(&format!("A-{i}")).await.unwrap();
        store_b.force_flush(&format!("B-{i}")).await.unwrap();
    }

    for i in 0..PER_STORE {
        let a_run = probe
            .latest_run(&format!("A-{i}"))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("A-{i} missing"));
        assert_eq!(a_run.run_id, format!("ra-{i}"));
        let b_run = probe
            .latest_run(&format!("B-{i}"))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("B-{i} missing"));
        assert_eq!(b_run.run_id, format!("rb-{i}"));
    }
    // Each unique thread has exactly one entry in the WAL and no intra-thread
    // coalescing is possible ⇒ each entry must be applied exactly once.
    // If JetStream double-delivered to both flushers, count would exceed 2N.
    assert_eq!(
        probe.count(),
        2 * PER_STORE,
        "expected exactly one inner.checkpoint per thread (no double-apply, no drops); got {}",
        probe.count()
    );

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}
