#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
#![cfg(feature = "nats")]

#[path = "nats_buffered_thread_fixture.rs"]
mod fixture;

use std::sync::Arc;
use std::time::Duration;

use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{
    ChildThreadDeleteStrategy, RunRecord, RunRequestSnapshot, RunStore, StorageError,
    ThreadRunStore, ThreadStore,
};
use awaken_stores::{InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore};
use fixture::NatsFixture;
use tokio::sync::{Barrier, Notify};

fn shared_config(fixture: &NatsFixture) -> NatsBufferedThreadConfig {
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let mut config = NatsBufferedThreadConfig::new(fixture.url.clone());
    config.stream_name = format!("THREADLOG_{suffix}");
    config.consumer_name = format!("c_{suffix}");
    config.hot_bucket = format!("hot_{suffix}");
    config.flush_interval = Duration::from_millis(100);
    config
}

fn mk_run(id: &str, thread: &str) -> RunRecord {
    RunRecord {
        run_id: id.into(),
        thread_id: thread.into(),
        agent_id: "a".into(),
        parent_run_id: None,
        registry_manifest: None,
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

/// Write from instance A visible to instance B via shared JetStream WAL overlay
/// even before the inner DB is flushed.
#[tokio::test]
async fn read_your_writes_across_instances() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60); // effectively disable flusher

    let store_a = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
        .await
        .expect("a");
    let store_b = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
        .await
        .expect("b");

    let run = mk_run("r1", "t1");
    store_a
        .checkpoint("t1", &[Message::user("from A")], &run)
        .await
        .unwrap();

    // B reads via WAL overlay (latest_seq > flushed_seq).
    let msgs = store_b.load_messages("t1").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text(), "from A");

    let latest = store_b.latest_run("t1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r1");

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Shared inner DB: exactly one instance's flusher drains each WAL entry; both
/// instances eventually see data in the shared inner store.
#[tokio::test]
async fn shared_inner_store_observes_flushed_writes_from_either_instance() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let cfg = shared_config(&fixture);

    let store_a = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
        .await
        .expect("a");
    let store_b = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
        .await
        .expect("b");

    // A writes multiple runs.
    for i in 0..5 {
        store_a
            .checkpoint(
                "t1",
                &[Message::user(format!("m{i}"))],
                &mk_run(&format!("r{i}"), "t1"),
            )
            .await
            .unwrap();
    }

    // Eventually all writes land in the shared inner DB.
    let start = std::time::Instant::now();
    let mut seen = 0;
    while start.elapsed() < Duration::from_secs(5) {
        seen = 0;
        for i in 0..5 {
            if inner.load_run(&format!("r{i}")).await.unwrap().is_some() {
                seen += 1;
            }
        }
        if seen == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(seen, 5, "all 5 runs should be in shared inner DB within 5s");

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Concurrent writers to the same thread produce monotonic unique thread_seq via
/// KV CAS on latest_seq.
#[tokio::test]
async fn concurrent_writes_same_thread_produce_unique_monotonic_seq() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let cfg = shared_config(&fixture);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    // Fire 10 concurrent checkpoints from each instance on the same thread.
    let mut handles = Vec::new();
    for i in 0..10 {
        let s = Arc::clone(&store_a);
        handles.push(tokio::spawn(async move {
            s.checkpoint(
                "t-concurrent",
                &[Message::user(format!("a{i}"))],
                &mk_run(&format!("ra{i}"), "t-concurrent"),
            )
            .await
            .unwrap();
        }));
    }
    for i in 0..10 {
        let s = Arc::clone(&store_b);
        handles.push(tokio::spawn(async move {
            s.checkpoint(
                "t-concurrent",
                &[Message::user(format!("b{i}"))],
                &mk_run(&format!("rb{i}"), "t-concurrent"),
            )
            .await
            .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // All 20 writes eventually reach the shared DB.
    store_a.force_flush("t-concurrent").await.unwrap();

    let mut total = 0;
    for prefix in ["ra", "rb"] {
        for i in 0..10 {
            if inner
                .load_run(&format!("{prefix}{i}"))
                .await
                .unwrap()
                .is_some()
            {
                total += 1;
            }
        }
    }
    assert_eq!(total, 20, "all 20 concurrent writes should land in DB");

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Hot KV run cache visible across instances — A writes, B's `load_run` sees it
/// immediately (before DB flush).
#[tokio::test]
async fn hot_run_cache_shared_across_instances() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60); // block flusher so only KV is fresh

    let store_a = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
        .await
        .expect("a");
    let store_b = NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
        .await
        .expect("b");

    store_a
        .checkpoint("t1", &[], &mk_run("r1", "t1"))
        .await
        .unwrap();

    // B reads from shared KV hot cache.
    let loaded = store_b.load_run("r1").await.unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().run_id, "r1");

    // Inner DB should still be empty (flush is blocked).
    assert!(inner.load_run("r1").await.unwrap().is_none());

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

#[tokio::test]
async fn hierarchy_changing_checkpoints_are_exclusive_across_instances() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("a"))
        .await
        .unwrap();
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("b"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);
    cfg.read_consistency = awaken_stores::ReadConsistency::Eventual;

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    let barrier = Arc::new(Barrier::new(3));
    let spawn_checkpoint = |store: Arc<NatsBufferedThreadStore<InMemoryStore>>,
                            thread_id: &'static str,
                            parent_thread_id: &'static str| {
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

    let left = spawn_checkpoint(Arc::clone(&store_a), "a", "b");
    let right = spawn_checkpoint(Arc::clone(&store_b), "b", "a");
    barrier.wait().await;

    let left = left.await.unwrap();
    let right = right.await.unwrap();
    assert_ne!(left.is_ok(), right.is_ok());
    assert!(
        matches!(left, Ok(())) || matches!(left, Err(StorageError::Validation(_))),
        "unexpected result from store_a: {left:?}"
    );
    assert!(
        matches!(right, Ok(())) || matches!(right, Err(StorageError::Validation(_))),
        "unexpected result from store_b: {right:?}"
    );

    store_a.force_flush_all_pending().await.unwrap();

    for thread_id in ["a", "b"] {
        let thread = inner.load_thread(thread_id).await.unwrap().unwrap();
        inner
            .validate_thread_hierarchy(thread_id, thread.parent_thread_id.as_deref())
            .await
            .unwrap();
    }

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_claim_after_wal_publish_is_aborted_and_does_not_block_conflicting_parent() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("a"))
        .await
        .unwrap();
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("b"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);
    cfg.ack_wait = Duration::from_millis(200);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    store_a.__test_set_hierarchy_claim_timing(150, None);

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_wait = reached.notified();
    store_a
        .__test_pause_checkpoint_after_wal_publish(Arc::clone(&reached), Arc::clone(&release))
        .await;

    let paused_writer = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .checkpoint(
                    "a",
                    &[Message::user("a->b")],
                    &mk_child_run("run-a-to-b", "a", "b"),
                )
                .await
        })
    };

    reached_wait.await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    store_b
        .checkpoint(
            "b",
            &[Message::user("b->a")],
            &mk_child_run("run-b-to-a", "b", "a"),
        )
        .await
        .unwrap();

    let overlaid_a = store_b.load_thread("a").await.unwrap().unwrap();
    assert_eq!(overlaid_a.parent_thread_id, None);

    release.notify_waiters();

    let paused_writer = paused_writer.await.unwrap().unwrap_err();
    assert!(
        matches!(paused_writer, StorageError::Io(_)),
        "expired writer must fail ownership check before promote: {paused_writer:?}"
    );

    store_b.force_flush_all_pending().await.unwrap();

    let flushed_a = inner.load_thread("a").await.unwrap().unwrap();
    let flushed_b = inner.load_thread("b").await.unwrap().unwrap();
    assert_eq!(flushed_a.parent_thread_id, None);
    assert_eq!(flushed_b.parent_thread_id.as_deref(), Some("a"));
    inner
        .validate_thread_hierarchy("a", flushed_a.parent_thread_id.as_deref())
        .await
        .unwrap();
    inner
        .validate_thread_hierarchy("b", flushed_b.parent_thread_id.as_deref())
        .await
        .unwrap();

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn aborted_wal_state_cannot_be_recommitted_after_claim_expiry() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("a"))
        .await
        .unwrap();
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("b"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);
    cfg.ack_wait = Duration::from_millis(200);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    store_a.__test_set_hierarchy_claim_timing(150, None);

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_wait = reached.notified();
    store_a
        .__test_pause_checkpoint_after_post_publish_claim_check(
            Arc::clone(&reached),
            Arc::clone(&release),
        )
        .await;

    let paused_writer = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .checkpoint(
                    "a",
                    &[Message::user("a->b-after-ensure")],
                    &mk_child_run("run-a-to-b-after-ensure", "a", "b"),
                )
                .await
        })
    };

    reached_wait.await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    store_b
        .checkpoint(
            "b",
            &[Message::user("b->a-after-abort")],
            &mk_child_run("run-b-to-a-after-abort", "b", "a"),
        )
        .await
        .unwrap();

    let wal_state = store_b
        .__test_read_wal_state("a", 1)
        .await
        .unwrap()
        .expect("aborted wal state");
    assert_eq!(wal_state.0, "aborted");

    release.notify_waiters();

    let paused_writer = paused_writer.await.unwrap().unwrap_err();
    match paused_writer {
        StorageError::Io(message) => {
            assert!(
                message.contains("aborted WAL state") || message.contains("hierarchy claim expiry"),
                "expected aborted/expired commit failure, got: {message}"
            );
        }
        other => panic!("expected Io error, got {other:?}"),
    }

    let wal_state_after = store_b
        .__test_read_wal_state("a", 1)
        .await
        .unwrap()
        .expect("wal state should remain settled");
    assert_eq!(wal_state_after.0, "aborted");

    store_b.force_flush_all_pending().await.unwrap();

    let overlaid_a = store_b.load_thread("a").await.unwrap().unwrap();
    let overlaid_b = store_b.load_thread("b").await.unwrap().unwrap();
    assert_eq!(overlaid_a.parent_thread_id, None);
    assert_eq!(overlaid_b.parent_thread_id.as_deref(), Some("a"));

    let flushed_a = inner.load_thread("a").await.unwrap().unwrap();
    let flushed_b = inner.load_thread("b").await.unwrap().unwrap();
    assert_eq!(flushed_a.parent_thread_id, None);
    assert_eq!(flushed_b.parent_thread_id.as_deref(), Some("a"));
    inner
        .validate_thread_hierarchy("a", flushed_a.parent_thread_id.as_deref())
        .await
        .unwrap();
    inner
        .validate_thread_hierarchy("b", flushed_b.parent_thread_id.as_deref())
        .await
        .unwrap();

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_claim_before_wal_publish_cannot_recover_late_conflicting_wal() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("a"))
        .await
        .unwrap();
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("b"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);
    cfg.ack_wait = Duration::from_millis(200);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    store_a.__test_set_hierarchy_claim_timing(150, None);

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_wait = reached.notified();
    store_a
        .__test_pause_checkpoint_before_wal_publish(Arc::clone(&reached), Arc::clone(&release))
        .await;

    let paused_writer = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .checkpoint(
                    "a",
                    &[Message::user("a->b-late-publish")],
                    &mk_child_run("run-a-to-b-late", "a", "b"),
                )
                .await
        })
    };

    reached_wait.await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    store_b
        .checkpoint(
            "b",
            &[Message::user("b->a")],
            &mk_child_run("run-b-to-a-late", "b", "a"),
        )
        .await
        .unwrap();

    release.notify_waiters();

    let paused_writer = paused_writer.await.unwrap().unwrap_err();
    assert!(
        matches!(paused_writer, StorageError::Io(_)),
        "expired writer must fail ownership check after late publish: {paused_writer:?}"
    );

    store_b.force_flush_all_pending().await.unwrap();

    let overlaid_a = store_b.load_thread("a").await.unwrap().unwrap();
    let overlaid_b = store_b.load_thread("b").await.unwrap().unwrap();
    assert_eq!(overlaid_a.parent_thread_id, None);
    assert_eq!(overlaid_b.parent_thread_id.as_deref(), Some("a"));

    let flushed_a = inner.load_thread("a").await.unwrap().unwrap();
    let flushed_b = inner.load_thread("b").await.unwrap().unwrap();
    assert_eq!(flushed_a.parent_thread_id, None);
    assert_eq!(flushed_b.parent_thread_id.as_deref(), Some("a"));
    inner
        .validate_thread_hierarchy("a", flushed_a.parent_thread_id.as_deref())
        .await
        .unwrap();
    inner
        .validate_thread_hierarchy("b", flushed_b.parent_thread_id.as_deref())
        .await
        .unwrap();

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_clears_hot_state_before_releasing_hierarchy_claim() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("t"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_wait = reached.notified();
    store_a
        .__test_pause_delete_after_inner_delete("t", Arc::clone(&reached), Arc::clone(&release))
        .await;

    let delete_task = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .delete_thread_with_strategy("t", ChildThreadDeleteStrategy::Detach)
                .await
        })
    };

    reached_wait.await;
    assert!(
        inner.load_thread("t").await.unwrap().is_none(),
        "inner delete should already have applied before hot-state cleanup pause"
    );

    let checkpoint_task = {
        let store_b = Arc::clone(&store_b);
        tokio::spawn(async move {
            store_b
                .checkpoint("t", &[Message::user("reborn")], &mk_run("run-reborn", "t"))
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !checkpoint_task.is_finished(),
        "checkpoint must wait until delete clears hot state and releases the hierarchy claim"
    );

    release.notify_waiters();

    delete_task.await.unwrap().unwrap();
    checkpoint_task.await.unwrap().unwrap();

    let overlaid = store_b.load_thread("t").await.unwrap().unwrap();
    assert_eq!(overlaid.latest_run_id.as_deref(), Some("run-reborn"));

    store_b
        .__test_flush_committed_thread_seqs("t", &[1])
        .await
        .unwrap();
    assert_eq!(store_b.__test_read_flushed_seq("t").await.unwrap(), 1);

    let flushed = inner.load_thread("t").await.unwrap().unwrap();
    assert_eq!(flushed.latest_run_id.as_deref(), Some("run-reborn"));
    let messages = inner.load_messages("t").await.unwrap().unwrap();
    assert_eq!(messages[0].text(), "reborn");

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_hierarchy_claim_before_save_thread_validated_prevents_stale_cycle_write() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("a"))
        .await
        .unwrap();
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("b"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    store_a.__test_set_hierarchy_claim_timing(150, None);

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_wait = reached.notified();
    store_a
        .__test_pause_save_thread_validated_before_inner_save(
            "a",
            Arc::clone(&reached),
            Arc::clone(&release),
        )
        .await;

    let paused_save = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .save_thread_validated(
                    &awaken_server_contract::thread::Thread::with_id("a")
                        .with_parent_thread_id("b"),
                )
                .await
        })
    };

    reached_wait.await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    store_b
        .save_thread_validated(
            &awaken_server_contract::thread::Thread::with_id("b").with_parent_thread_id("a"),
        )
        .await
        .unwrap();

    release.notify_waiters();

    let paused_save = paused_save.await.unwrap().unwrap_err();
    assert!(
        matches!(paused_save, StorageError::Io(_)),
        "expired hierarchy saver must fail before inner write: {paused_save:?}"
    );

    let thread_a = inner.load_thread("a").await.unwrap().unwrap();
    let thread_b = inner.load_thread("b").await.unwrap().unwrap();
    assert_eq!(thread_a.parent_thread_id, None);
    assert_eq!(thread_b.parent_thread_id.as_deref(), Some("a"));
    inner
        .validate_thread_hierarchy("a", thread_a.parent_thread_id.as_deref())
        .await
        .unwrap();
    inner
        .validate_thread_hierarchy("b", thread_b.parent_thread_id.as_deref())
        .await
        .unwrap();

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_hierarchy_claim_during_delete_does_not_clear_recreated_hot_state() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    inner
        .save_thread(&awaken_server_contract::thread::Thread::with_id("t"))
        .await
        .unwrap();

    let mut cfg = shared_config(&fixture);
    cfg.flush_interval = Duration::from_secs(60);

    let store_a = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg.clone())
            .await
            .expect("a"),
    );
    let store_b = Arc::new(
        NatsBufferedThreadStore::connect(Arc::clone(&inner), cfg)
            .await
            .expect("b"),
    );

    store_a.__test_set_hierarchy_claim_timing(150, None);

    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reached_wait = reached.notified();
    store_a
        .__test_pause_delete_after_inner_delete("t", Arc::clone(&reached), Arc::clone(&release))
        .await;

    let delete_task = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .delete_thread_with_strategy("t", ChildThreadDeleteStrategy::Detach)
                .await
        })
    };

    reached_wait.await;
    assert!(inner.load_thread("t").await.unwrap().is_none());
    tokio::time::sleep(Duration::from_millis(250)).await;

    store_b
        .checkpoint(
            "t",
            &[Message::user("reborn-after-expiry")],
            &mk_run("run-reborn-expiry", "t"),
        )
        .await
        .unwrap();

    release.notify_waiters();

    let delete_task = delete_task.await.unwrap().unwrap_err();
    assert!(
        matches!(delete_task, StorageError::Io(_)),
        "expired deleter must fail before clearing hot state: {delete_task:?}"
    );

    let overlaid = store_b.load_thread("t").await.unwrap().unwrap();
    assert_eq!(overlaid.latest_run_id.as_deref(), Some("run-reborn-expiry"));

    store_b
        .__test_flush_committed_thread_seqs("t", &[1])
        .await
        .unwrap();
    assert_eq!(store_b.__test_read_flushed_seq("t").await.unwrap(), 1);

    let flushed = inner.load_thread("t").await.unwrap().unwrap();
    assert_eq!(flushed.latest_run_id.as_deref(), Some("run-reborn-expiry"));
    let messages = inner.load_messages("t").await.unwrap().unwrap();
    assert_eq!(messages[0].text(), "reborn-after-expiry");

    Arc::into_inner(store_a)
        .expect("single owner for store_a shutdown")
        .shutdown()
        .await
        .unwrap();
    Arc::into_inner(store_b)
        .expect("single owner for store_b shutdown")
        .shutdown()
        .await
        .unwrap();
}
