//! Cross-node live-channel integration tests for `NatsMailboxStore`.
//!
//! Two independent store instances share a single NATS server — the closest
//! mirror of a multi-node deployment where different processes own a
//! thread's active run at different times. Verifies that:
//!
//! - A publish from store A is observed by a subscriber on store B.
//! - All `LiveRunCommand` variants round-trip without truncation.
//! - Disjoint threads are isolated (no cross-talk at the subject level).
//! - Publishing with no subscriber is a silent no-op (best-effort).
//! - Multiple subscribers to the same subject all receive each publish
//!   (core NATS pub-sub semantics).

#![cfg(feature = "nats")]

mod nats_fixture;

use std::time::Duration;

use awaken_contract::contract::mailbox::{LiveRunCommand, MailboxStore};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};
use futures::StreamExt;
use nats_fixture::NatsFixture;
use serde_json::Value;
use tokio::time::timeout;

/// Config that is unique per test (so tests can run in parallel on the
/// same fixture) but shared between the two store instances in a single
/// test — they simulate two nodes attached to the same production NATS
/// deployment, so they must reuse the same stream and KV buckets.
fn shared_config(fixture: &NatsFixture) -> NatsMailboxConfig {
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{tag}");
    config.consumer_name = format!("c_{tag}");
    config.dispatch_bucket = format!("d_{tag}");
    config.epoch_bucket = format!("e_{tag}");
    config.thread_index_bucket = format!("ti_{tag}");
    config
}

fn mk_resume() -> ToolCallResume {
    ToolCallResume {
        decision_id: "d1".into(),
        action: ResumeDecisionAction::Resume,
        result: Value::Null,
        reason: None,
        updated_at: 0,
    }
}

/// Publish on one store, subscribe on another, verify cross-instance delivery.
/// This is the minimum "multi-node" scenario: two `NatsMailboxStore` instances
/// connected to the same NATS URL act as two different server processes.
#[tokio::test]
async fn cross_node_messages_delivery() {
    let fixture = NatsFixture::start().await;
    let config = shared_config(&fixture);
    let publisher = NatsMailboxStore::connect(config.clone())
        .await
        .expect("publisher connect");
    let subscriber_store = NatsMailboxStore::connect(config)
        .await
        .expect("subscriber connect");

    let stream = subscriber_store
        .open_live_channel("thread-x")
        .await
        .expect("open channel");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_c = captured.clone();
    let _consumer = tokio::spawn(async move {
        let mut stream = stream;
        while let Some(entry) = stream.next().await {
            captured_c.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });

    publisher
        .deliver_live(
            "thread-x",
            LiveRunCommand::Messages(vec![Message::user("cross-node")]),
        )
        .await
        .expect("publish");

    // Allow forwarder to capture + ack.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cmds = captured.lock().await;
    match &cmds[0] {
        LiveRunCommand::Messages(msgs) => {
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].text(), "cross-node");
        }
        other => panic!("expected Messages, got {other:?}"),
    }
    drop(cmds);

    publisher.shutdown().await.unwrap();
    subscriber_store.shutdown().await.unwrap();
}

/// All three `LiveRunCommand` variants round-trip across stores.
#[tokio::test]
async fn cross_node_all_variants() {
    let fixture = NatsFixture::start().await;
    let config = shared_config(&fixture);
    let publisher = NatsMailboxStore::connect(config.clone())
        .await
        .expect("publisher connect");
    let subscriber_store = NatsMailboxStore::connect(config)
        .await
        .expect("subscriber connect");

    let stream = subscriber_store
        .open_live_channel("t-all")
        .await
        .expect("open channel");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_c = captured.clone();
    let _consumer = tokio::spawn(async move {
        let mut stream = stream;
        while let Some(entry) = stream.next().await {
            captured_c.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });

    publisher
        .deliver_live("t-all", LiveRunCommand::Messages(vec![Message::user("m")]))
        .await
        .unwrap();
    publisher
        .deliver_live("t-all", LiveRunCommand::Cancel)
        .await
        .unwrap();
    publisher
        .deliver_live(
            "t-all",
            LiveRunCommand::Decision(vec![("tc-1".into(), mk_resume())]),
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let cmds = captured.lock().await;
    assert!(matches!(cmds[0], LiveRunCommand::Messages(_)));
    assert!(matches!(cmds[1], LiveRunCommand::Cancel));
    match &cmds[2] {
        LiveRunCommand::Decision(d) => {
            assert_eq!(d.len(), 1);
            assert_eq!(d[0].0, "tc-1");
        }
        other => panic!("expected Decision, got {other:?}"),
    }
    drop(cmds);

    publisher.shutdown().await.unwrap();
    subscriber_store.shutdown().await.unwrap();
}

/// Disjoint threads must not cross-talk — the subject carries `thread_id`
/// so publishes on one subject never reach subscribers of another.
#[tokio::test]
async fn cross_node_thread_isolation() {
    let fixture = NatsFixture::start().await;
    let config = shared_config(&fixture);
    let publisher = NatsMailboxStore::connect(config.clone())
        .await
        .expect("publisher connect");
    let subscriber_store = NatsMailboxStore::connect(config)
        .await
        .expect("subscriber connect");

    let stream_a = subscriber_store
        .open_live_channel("t-a")
        .await
        .expect("open a");
    let mut stream_b = subscriber_store
        .open_live_channel("t-b")
        .await
        .expect("open b");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let captured_a = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_a_c = captured_a.clone();
    let _consumer_a = tokio::spawn(async move {
        let mut stream_a = stream_a;
        while let Some(entry) = stream_a.next().await {
            captured_a_c.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });

    publisher
        .deliver_live("t-a", LiveRunCommand::Cancel)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let cmds = captured_a.lock().await;
    assert!(matches!(cmds[0], LiveRunCommand::Cancel));
    drop(cmds);

    let got_b = timeout(Duration::from_millis(200), stream_b.next()).await;
    assert!(got_b.is_err(), "t-b must not receive t-a's command");

    publisher.shutdown().await.unwrap();
    subscriber_store.shutdown().await.unwrap();
}

/// Publishing with no subscriber is a silent no-op — messages drop, no
/// error surfaces. This is the contract for ephemeral `LiveRunCommand` delivery.
#[tokio::test]
async fn cross_node_publish_without_subscriber_silently_drops() {
    let fixture = NatsFixture::start().await;
    let publisher = NatsMailboxStore::connect(shared_config(&fixture))
        .await
        .expect("publisher connect");

    let outcome = publisher
        .deliver_live("ghost-thread", LiveRunCommand::Cancel)
        .await
        .expect("publish without subscriber must return Ok");
    assert_eq!(
        outcome,
        awaken_contract::contract::mailbox::LiveDeliveryOutcome::NoSubscriber,
        "no-subscriber ⇒ NoSubscriber so the caller falls back to durable queue"
    );

    publisher.shutdown().await.unwrap();
}

/// Two subscribers on the same subject both receive each publish
/// (core NATS pub-sub fan-out semantics).
#[tokio::test]
async fn cross_node_multiple_subscribers_fanout() {
    let fixture = NatsFixture::start().await;
    let config = shared_config(&fixture);
    let publisher = NatsMailboxStore::connect(config.clone())
        .await
        .expect("publisher connect");
    let sub_a = NatsMailboxStore::connect(config.clone())
        .await
        .expect("sub a connect");
    let sub_b = NatsMailboxStore::connect(config)
        .await
        .expect("sub b connect");

    let stream_a = sub_a.open_live_channel("fanout").await.expect("open a");
    let stream_b = sub_b.open_live_channel("fanout").await.expect("open b");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let captured_a = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_b = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let a_c = captured_a.clone();
    let b_c = captured_b.clone();
    let _ca = tokio::spawn(async move {
        let mut stream_a = stream_a;
        while let Some(entry) = stream_a.next().await {
            a_c.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });
    let _cb = tokio::spawn(async move {
        let mut stream_b = stream_b;
        while let Some(entry) = stream_b.next().await {
            b_c.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });

    publisher
        .deliver_live("fanout", LiveRunCommand::Cancel)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(captured_a.lock().await[0], LiveRunCommand::Cancel));
    assert!(matches!(captured_b.lock().await[0], LiveRunCommand::Cancel));

    publisher.shutdown().await.unwrap();
    sub_a.shutdown().await.unwrap();
    sub_b.shutdown().await.unwrap();
}
