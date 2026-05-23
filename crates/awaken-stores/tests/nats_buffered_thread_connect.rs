#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
#![cfg(feature = "nats")]

#[path = "nats_buffered_thread_fixture.rs"]
mod fixture;

use std::sync::Arc;

use awaken_stores::{InMemoryStore, NatsBufferedThreadStore};
use fixture::{NatsFixture, unique_config};

#[tokio::test]
async fn connect_and_shutdown() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let store = NatsBufferedThreadStore::connect(inner, unique_config(&fixture))
        .await
        .expect("connect");
    store.shutdown().await.expect("shutdown");
}

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunQuery, RunRecord, RunStore, ThreadRunStore};

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

#[tokio::test]
async fn checkpoint_writes_wal_without_db_write() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let inner_probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    // Effectively disable the flusher so the inner store stays empty.
    config.flush_interval = std::time::Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    store
        .checkpoint("t1", &[Message::user("hi")], &mk_run("r1", "t1"))
        .await
        .unwrap();

    // Flush interval is long, so DB should stay empty until an explicit drain.
    let loaded = inner_probe.load_run("r1").await.unwrap();
    assert!(loaded.is_none(), "inner store should not have the run yet");
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn read_your_writes_via_wal_overlay() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let inner_probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    // Effectively disable the flusher so the read must hit the WAL overlay.
    config.flush_interval = std::time::Duration::from_secs(60);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r1", "t1");
    store
        .checkpoint("t1", &[Message::user("hello")], &run)
        .await
        .unwrap();

    // flush_interval is 60s so WAL overlay must serve the read without waiting for DB.
    use awaken_contract::contract::storage::ThreadStore;
    let msgs = store.load_messages("t1").await.unwrap().unwrap();
    assert_eq!(msgs.len(), 1);

    let thread = store.load_thread("t1").await.unwrap().unwrap();
    assert_eq!(thread.latest_run_id.as_deref(), Some("r1"));
    assert_eq!(thread.open_run_id.as_deref(), Some("r1"));

    let threads = store.list_threads(0, 10).await.unwrap();
    assert_eq!(threads, vec!["t1".to_string()]);

    let loaded_run = store.load_run("r1").await.unwrap().unwrap();
    assert_eq!(loaded_run.run_id, "r1");

    let latest = store.latest_run("t1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r1");

    let page = store
        .list_runs(&RunQuery {
            thread_id: Some("t1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].run_id, "r1");

    assert!(
        inner_probe.load_thread("t1").await.unwrap().is_none(),
        "inner store should still be stale before shutdown drain"
    );
    store.shutdown().await.unwrap();
    assert_eq!(
        inner_probe
            .load_thread("t1")
            .await
            .unwrap()
            .unwrap()
            .latest_run_id
            .as_deref(),
        Some("r1"),
        "shutdown must drain pending WAL entries into the inner store"
    );
}

use std::time::Duration;

#[tokio::test]
async fn flusher_writes_to_inner_db() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let inner_probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(100);
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r1", "t-flush");
    store
        .checkpoint("t-flush", &[Message::user("hi")], &run)
        .await
        .unwrap();

    // Wait for flusher to catch up.
    let mut flushed = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if inner_probe.load_run("r1").await.unwrap().is_some() {
            flushed = true;
            break;
        }
    }
    assert!(flushed, "flusher should have written to inner DB within 3s");
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn force_flush_blocks_until_drained() {
    let fixture = NatsFixture::start().await;
    let inner = Arc::new(InMemoryStore::new());
    let inner_probe = Arc::clone(&inner);
    let mut config = unique_config(&fixture);
    config.flush_interval = Duration::from_millis(500); // slow flush
    let store = NatsBufferedThreadStore::connect(inner, config)
        .await
        .expect("connect");

    let run = mk_run("r1", "t-force");
    store
        .checkpoint("t-force", &[Message::user("hi")], &run)
        .await
        .unwrap();

    // Without waiting the full interval, inner DB may not have it.
    // (Flusher may or may not have run yet; that's ok — force_flush will wait.)
    store.force_flush("t-force").await.expect("force flush");
    assert!(
        inner_probe.load_run("r1").await.unwrap().is_some(),
        "flushed after force_flush"
    );
    store.shutdown().await.unwrap();
}
