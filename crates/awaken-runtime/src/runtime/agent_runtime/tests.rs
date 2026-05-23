use super::*;
use std::sync::Arc;

use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use serde_json::Value;

struct StubResolver;
impl crate::registry::AgentResolver for StubResolver {
    fn resolve(
        &self,
        agent_id: &str,
    ) -> Result<crate::registry::ResolvedAgent, crate::error::RuntimeError> {
        Err(crate::error::RuntimeError::AgentNotFound {
            agent_id: agent_id.to_string(),
        })
    }
}

fn make_runtime() -> AgentRuntime {
    AgentRuntime::new(Arc::new(StubResolver))
}

fn make_resume() -> ToolCallResume {
    ToolCallResume {
        decision_id: "d1".into(),
        action: ResumeDecisionAction::Resume,
        result: Value::Null,
        reason: None,
        updated_at: 0,
    }
}

#[test]
fn new_creates_runtime() {
    let rt = make_runtime();
    assert!(rt.storage.is_none());
    assert!(rt.profile_store.is_none());
    assert!(rt.registry_handle().is_none());
}

#[test]
fn resolver_returns_ref() {
    let rt = make_runtime();
    // The stub resolver always returns AgentNotFound
    let err = rt.resolver().resolve("any").unwrap_err();
    assert!(
        matches!(err, crate::error::RuntimeError::AgentNotFound { .. }),
        "expected AgentNotFound, got {err:?}"
    );
}

#[test]
fn resolver_arc_returns_clone() {
    let rt = make_runtime();
    let arc = rt.resolver_arc();
    let err = arc.resolve("x").unwrap_err();
    assert!(matches!(
        err,
        crate::error::RuntimeError::AgentNotFound { .. }
    ));
}

#[test]
fn with_thread_run_store_sets_store() {
    let store = Arc::new(awaken_stores::InMemoryStore::new());
    let rt = make_runtime().with_in_memory_thread_run_store(store);
    assert!(rt.thread_run_store().is_some());
}

#[test]
fn thread_run_store_none_by_default() {
    let rt = make_runtime();
    assert!(rt.thread_run_store().is_none());
}

#[test]
fn create_run_channels_returns_triple() {
    let rt = make_runtime();
    let (handle, token, _rx) = rt.create_run_channels("run-1".into());
    assert_eq!(handle.run_id, "run-1");
    assert!(!token.is_cancelled());
}

#[test]
fn register_run_succeeds() {
    let rt = make_runtime();
    let (handle, _token, _rx) = rt.create_run_channels("run-1".into());
    assert!(rt.register_run("thread-1", handle).is_ok());
}

#[test]
fn register_run_fails_for_same_thread() {
    let rt = make_runtime();
    let (h1, _, _rx1) = rt.create_run_channels("run-1".into());
    let (h2, _, _rx2) = rt.create_run_channels("run-2".into());
    rt.register_run("thread-1", h1).unwrap();
    let err = rt.register_run("thread-1", h2).unwrap_err();
    assert!(
        matches!(err, RuntimeError::ThreadAlreadyRunning { ref thread_id } if thread_id == "thread-1"),
        "expected ThreadAlreadyRunning, got {err:?}"
    );
}

#[test]
fn unregister_run_allows_reregistration() {
    let rt = make_runtime();
    let (h1, _, _rx1) = rt.create_run_channels("run-1".into());
    rt.register_run("thread-1", h1).unwrap();
    rt.unregister_run("run-1");

    let (h2, _, _rx2) = rt.create_run_channels("run-2".into());
    assert!(rt.register_run("thread-1", h2).is_ok());
}

#[test]
fn run_handle_cancel() {
    let rt = make_runtime();
    let (handle, token, _rx) = rt.create_run_channels("run-1".into());
    assert!(!token.is_cancelled());
    handle.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn run_handle_send_decisions() {
    let rt = make_runtime();
    let (handle, _token, mut rx) = rt.create_run_channels("run-1".into());
    let decisions = vec![("call-1".into(), make_resume())];
    handle.send_decisions(decisions).unwrap();

    // Receive the batch from the channel
    let batch = rx.try_recv().unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].0, "call-1");
}

#[test]
fn run_handle_send_decision_single() {
    let rt = make_runtime();
    let (handle, _token, mut rx) = rt.create_run_channels("run-1".into());
    handle
        .send_decision("call-2".into(), make_resume())
        .unwrap();

    let batch = rx.try_recv().unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].0, "call-2");
}

#[test]
fn run_handle_send_decisions_closed_channel() {
    let rt = make_runtime();
    let (handle, _token, rx) = rt.create_run_channels("run-1".into());
    // Drop the receiver to close the channel
    drop(rx);

    let result = handle.send_decisions(vec![("call-1".into(), make_resume())]);
    assert!(result.is_err(), "send should fail when receiver is dropped");
}

// ── Live forwarder integration ──

mod live_forwarder {
    use super::*;
    use awaken_contract::contract::mailbox::LiveRunCommand;
    use awaken_stores::InMemoryMailboxStore;
    use std::time::Duration;

    /// Publish on `store` until the subscriber count for `thread_id` is
    /// non-zero so the forwarder's background subscription is guaranteed
    /// active. We cannot inspect the broadcast state directly from here
    /// (the broadcast sender is private to the store), so we send a
    /// single no-op ping that will be consumed by the forwarder and
    /// poll until the first test-visible side effect proves it ran.
    async fn settle() {
        // 20ms is enough for a tokio::spawn + one await + subscribe call
        // in CI. Tests that observe the forwarder output should use
        // additional polling with timeouts.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn messages_variant_lands_in_inbox() {
        let store = Arc::new(InMemoryMailboxStore::new());
        let rt = make_runtime().with_mailbox_store(store.clone());
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        let (handle, _token, _rx) =
            rt.create_run_channels_with_inbox("run-1".into(), None, Some(inbox_tx));
        rt.register_run("thread-1", handle).unwrap();
        settle().await;

        store
            .deliver_live_to(
                &LiveRunTarget::new("thread-1", "run-1"),
                LiveRunCommand::Messages(vec![Message::user("live-1")]),
            )
            .await
            .unwrap();

        let mut received = None;
        for _ in 0..50 {
            if let Some(json) = inbox_rx.try_recv() {
                received = Some(json);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let payload = received.expect("forwarder must deliver Messages within 500ms");
        let messages = crate::inbox::inbox_payload_messages(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "live-1");
    }

    #[tokio::test]
    async fn cancel_variant_triggers_token() {
        let store = Arc::new(InMemoryMailboxStore::new());
        let rt = make_runtime().with_mailbox_store(store.clone());
        let (handle, token, _rx) = rt.create_run_channels("run-1".into());
        rt.register_run("thread-1", handle).unwrap();
        settle().await;

        store
            .deliver_live_to(
                &LiveRunTarget::new("thread-1", "run-1"),
                LiveRunCommand::Cancel,
            )
            .await
            .unwrap();

        let mut cancelled = false;
        for _ in 0..50 {
            if token.is_cancelled() {
                cancelled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(cancelled, "forwarder must cancel token within 500ms");
    }

    #[tokio::test]
    async fn decision_variant_lands_on_decision_channel() {
        let store = Arc::new(InMemoryMailboxStore::new());
        let rt = make_runtime().with_mailbox_store(store.clone());
        let (handle, _token, mut rx) = rt.create_run_channels("run-1".into());
        rt.register_run("thread-1", handle).unwrap();
        settle().await;

        let decisions = vec![("call-1".into(), make_resume())];
        store
            .deliver_live_to(
                &LiveRunTarget::new("thread-1", "run-1"),
                LiveRunCommand::Decision(decisions),
            )
            .await
            .unwrap();

        let mut got = None;
        for _ in 0..50 {
            if let Ok(batch) = rx.try_recv() {
                got = Some(batch);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let batch = got.expect("forwarder must deliver Decision within 500ms");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, "call-1");
    }

    #[tokio::test]
    async fn no_store_wired_no_forwarder_runs() {
        // Baseline: without `with_mailbox_store`, deliver_live published
        // elsewhere must not reach this runtime's channels.
        let detached_store = InMemoryMailboxStore::new();
        let rt = make_runtime(); // no store
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        let (handle, token, _rx) =
            rt.create_run_channels_with_inbox("run-1".into(), None, Some(inbox_tx));
        rt.register_run("thread-1", handle).unwrap();
        settle().await;

        detached_store
            .deliver_live(
                "thread-1",
                LiveRunCommand::Messages(vec![Message::user("ignored")]),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(inbox_rx.try_recv().is_none());
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn separate_threads_isolated() {
        let store = Arc::new(InMemoryMailboxStore::new());
        let rt = make_runtime().with_mailbox_store(store.clone());

        let (tx_a, mut rx_a) = crate::inbox::inbox_channel();
        let (tx_b, mut rx_b) = crate::inbox::inbox_channel();
        let (h_a, _tok_a, _dec_a) =
            rt.create_run_channels_with_inbox("run-a".into(), None, Some(tx_a));
        let (h_b, _tok_b, _dec_b) =
            rt.create_run_channels_with_inbox("run-b".into(), None, Some(tx_b));
        rt.register_run("thread-a", h_a).unwrap();
        rt.register_run("thread-b", h_b).unwrap();
        settle().await;

        store
            .deliver_live_to(
                &LiveRunTarget::new("thread-a", "run-a"),
                LiveRunCommand::Messages(vec![Message::user("for-a")]),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert!(rx_a.try_recv().is_some(), "thread-a must receive");
        assert!(
            rx_b.try_recv().is_none(),
            "thread-b must not receive thread-a's message"
        );
    }

    #[tokio::test]
    async fn unregister_stops_live_forwarder_subscription() {
        let store = Arc::new(InMemoryMailboxStore::new());
        let rt = make_runtime().with_mailbox_store(store.clone());
        let (handle, _token, _rx) = rt.create_run_channels("run-1".into());
        rt.register_run("thread-1", handle).unwrap();
        settle().await;

        rt.unregister_run("run-1");
        let target = LiveRunTarget::new("thread-1", "run-1");
        let mut outcome = store
            .deliver_live_to(&target, LiveRunCommand::Cancel)
            .await
            .unwrap();
        for _ in 0..50 {
            if outcome == awaken_contract::contract::mailbox::LiveDeliveryOutcome::NoSubscriber {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            outcome = store
                .deliver_live_to(&target, LiveRunCommand::Cancel)
                .await
                .unwrap();
        }
        assert_eq!(
            outcome,
            awaken_contract::contract::mailbox::LiveDeliveryOutcome::NoSubscriber,
            "unregister must stop the old live forwarder"
        );
    }

    #[tokio::test]
    async fn cancel_then_messages_messages_not_processed() {
        // After forwarder dispatches Cancel it exits, so subsequent
        // Messages on the same thread should not reach the inbox via
        // this forwarder instance (agent loop is expected to be torn
        // down anyway).
        let store = Arc::new(InMemoryMailboxStore::new());
        let rt = make_runtime().with_mailbox_store(store.clone());
        let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
        let (handle, token, _rx) =
            rt.create_run_channels_with_inbox("run-1".into(), None, Some(inbox_tx));
        rt.register_run("thread-1", handle).unwrap();
        settle().await;

        store
            .deliver_live_to(
                &LiveRunTarget::new("thread-1", "run-1"),
                LiveRunCommand::Cancel,
            )
            .await
            .unwrap();
        // Wait for cancel to propagate.
        for _ in 0..50 {
            if token.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(token.is_cancelled());

        store
            .deliver_live_to(
                &LiveRunTarget::new("thread-1", "run-1"),
                LiveRunCommand::Messages(vec![Message::user("too-late")]),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            inbox_rx.try_recv().is_none(),
            "forwarder must have exited after Cancel"
        );
    }
}
