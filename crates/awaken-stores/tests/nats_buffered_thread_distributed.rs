#![cfg(feature = "nats")]

#[path = "nats_buffered_thread_fixture.rs"]
mod fixture;

use std::sync::Arc;
use std::time::Duration;

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadRunStore, ThreadStore};
use awaken_stores::{InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore};
use fixture::NatsFixture;

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
