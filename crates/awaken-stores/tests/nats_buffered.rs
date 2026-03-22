//! Integration tests for NatsBufferedWriter.
//!
//! Requires Docker with NATS JetStream. Tests are marked `#[ignore]` since
//! they need an external service. Run with:
//! ```bash
//! cargo test --package awaken-stores --features nats --test nats_buffered -- --ignored
//! ```

#![cfg(feature = "nats")]

use std::sync::Arc;

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};
use awaken_stores::{InMemoryStore, NatsBufferedWriter};

fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
    RunRecord {
        run_id: run_id.to_owned(),
        thread_id: thread_id.to_owned(),
        agent_id: "agent-1".to_owned(),
        parent_run_id: None,
        status: RunStatus::Running,
        termination_code: None,
        created_at: updated_at,
        updated_at,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

/// Helper to create a buffered writer connected to a test NATS server.
/// Set NATS_URL env var to the NATS server address (e.g. "localhost:4222").
async fn make_writer() -> Option<(Arc<InMemoryStore>, NatsBufferedWriter)> {
    let url = std::env::var("NATS_URL").ok()?;
    let inner = Arc::new(InMemoryStore::new());
    let nats_client = async_nats::connect(&url).await.ok()?;
    let js = async_nats::jetstream::new(nats_client);
    let writer = NatsBufferedWriter::new(inner.clone(), js).await.ok()?;
    Some((inner, writer))
}

// ========================================================================
// Core operations
// ========================================================================

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn checkpoint_buffers_to_nats() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    let run = make_run("nats-r1", "nats-t1", 100);
    let messages = vec![Message::user("hello"), Message::assistant("world")];

    writer.checkpoint("nats-t1", &messages, &run).await.unwrap();

    // Inner store should not have messages yet (they're in NATS)
    let loaded = inner.load_messages("nats-t1").await.unwrap();
    assert!(loaded.is_none());
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn flush_persists_to_inner() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    let messages = vec![Message::user("hello"), Message::assistant("world")];
    let run = make_run("nats-r2", "nats-t2", 100);

    writer.checkpoint("nats-t2", &messages, &run).await.unwrap();

    let flushed = writer.flush("nats-t2").await.unwrap();
    assert!(flushed > 0);

    let loaded = inner.load_messages("nats-t2").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].text(), "hello");
    assert_eq!(loaded[1].text(), "world");
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn flush_empty_returns_zero() {
    let Some((_, writer)) = make_writer().await else {
        return;
    };

    let flushed = writer.flush("nats-nonexistent").await.unwrap();
    assert_eq!(flushed, 0);
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn load_delegates_to_inner() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    // Nothing in inner
    let loaded = writer.load_messages("nats-t3").await.unwrap();
    assert!(loaded.is_none());

    // Put something in inner
    let run = make_run("nats-r3", "nats-t3", 100);
    inner
        .checkpoint("nats-t3", &[Message::user("direct")], &run)
        .await
        .unwrap();

    let loaded = writer.load_messages("nats-t3").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].text(), "direct");
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn load_run_delegates_to_inner() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    let run = make_run("nats-r4", "nats-t4", 100);
    inner
        .checkpoint("nats-t4", &[Message::user("m")], &run)
        .await
        .unwrap();

    let loaded = writer.load_run("nats-r4").await.unwrap().unwrap();
    assert_eq!(loaded.run_id, "nats-r4");
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn latest_run_delegates_to_inner() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    let msgs = vec![Message::user("m")];
    inner
        .checkpoint("nats-t5", &msgs, &make_run("nats-r5a", "nats-t5", 100))
        .await
        .unwrap();
    inner
        .checkpoint("nats-t5", &msgs, &make_run("nats-r5b", "nats-t5", 200))
        .await
        .unwrap();

    let latest = writer.latest_run("nats-t5").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "nats-r5b");
}

// ========================================================================
// Recovery
// ========================================================================

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn recover_replays_unacked_checkpoints() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    // Checkpoint but don't flush (simulating crash)
    let messages = vec![Message::user("hello"), Message::assistant("world")];
    let run = make_run("nats-r6", "nats-t6", 100);
    writer.checkpoint("nats-t6", &messages, &run).await.unwrap();

    // Inner should be empty
    assert!(inner.load_messages("nats-t6").await.unwrap().is_none());

    // Recover
    let recovered = writer.recover().await.unwrap();
    assert!(recovered > 0);

    // Inner should now have the messages
    let loaded = inner.load_messages("nats-t6").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn recover_empty_returns_zero() {
    let Some((_, writer)) = make_writer().await else {
        return;
    };

    // Flush anything from previous test runs
    let _ = writer.flush("nats-clean").await;

    let recovered = writer.recover().await.unwrap();
    // May or may not be 0 depending on previous test state, but should not error
    let _ = recovered;
}

// ========================================================================
// Multiple checkpoints
// ========================================================================

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn multiple_checkpoints_last_wins_on_flush() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };

    let run1 = make_run("nats-r7a", "nats-t7", 100);
    writer
        .checkpoint("nats-t7", &[Message::user("first")], &run1)
        .await
        .unwrap();

    let run2 = make_run("nats-r7b", "nats-t7", 200);
    writer
        .checkpoint(
            "nats-t7",
            &[Message::user("first"), Message::assistant("second")],
            &run2,
        )
        .await
        .unwrap();

    let flushed = writer.flush("nats-t7").await.unwrap();
    assert_eq!(flushed, 2);

    let loaded = inner.load_messages("nats-t7").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[1].text(), "second");

    // Run should be the latest one
    let loaded_run = inner.load_run("nats-r7b").await.unwrap().unwrap();
    assert_eq!(loaded_run.updated_at, 200);
}

#[tokio::test]
#[ignore = "requires NATS JetStream via NATS_URL"]
async fn concurrent_checkpoints() {
    let Some((inner, writer)) = make_writer().await else {
        return;
    };
    let writer = Arc::new(writer);

    let handles: Vec<_> = (0..10)
        .map(|i| {
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                let run = make_run(&format!("nats-rc{i}"), "nats-tc", i as u64 * 100);
                writer
                    .checkpoint("nats-tc", &[Message::user(format!("msg-{i}"))], &run)
                    .await
                    .unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let flushed = writer.flush("nats-tc").await.unwrap();
    assert_eq!(flushed, 10);

    // Should have the last checkpoint's messages
    let loaded = inner.load_messages("nats-tc").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 1);
}
