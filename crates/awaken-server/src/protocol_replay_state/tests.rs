use super::*;
use async_trait::async_trait;
use awaken_contract::contract::durable_event_sink::{
    AgentEventNormalizationContext, AgentEventNormalizer, ScopedAgentEventNormalizer,
};
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_store::{AppendOptions, EventWriter};
use awaken_contract::contract::outbox::{OutboxMessageDraft, OutboxStatus, OutboxStore};
use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayDraft, ProtocolReplayReader, ProtocolReplayWriter, ProtocolStreamKey,
};
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_runtime::{AgentRuntime, RuntimeError};
use awaken_stores::{
    InMemoryEventStore, InMemoryMailboxStore, InMemoryOutboxStore, InMemoryProtocolReplayLog,
    InMemoryStore,
};

use crate::app::{ServerConfig, ServerState};
use crate::mailbox::{Mailbox, MailboxConfig};
use crate::protocol_fanout::{
    ProtocolReplayFanoutError, ProtocolReplayFanoutMessage, ProtocolReplayFanoutPublisher,
};
use crate::protocol_projector::{AI_SDK_PROTOCOL, AI_SDK_PROTOCOL_VERSION};

struct StubResolver;

impl awaken_runtime::AgentResolver for StubResolver {
    fn resolve(&self, agent_id: &str) -> Result<awaken_runtime::ResolvedAgent, RuntimeError> {
        Err(RuntimeError::AgentNotFound {
            agent_id: agent_id.to_string(),
        })
    }
}

fn make_state() -> ServerState {
    let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
    let store = Arc::new(InMemoryStore::new());
    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "test".to_string(),
        MailboxConfig::default(),
    ));
    ServerState::new(
        runtime,
        mailbox,
        store as Arc<dyn ThreadRunStore>,
        Arc::new(StubResolver),
        ServerConfig::default(),
    )
}

async fn append_run_start(event_store: &InMemoryEventStore) -> String {
    let normalizer = ScopedAgentEventNormalizer::new(
        AgentEventNormalizationContext::new("thread-relay", "run-relay", "test").unwrap(),
    );
    let normalized = normalizer
        .normalize(&AgentEvent::RunStart {
            thread_id: "thread-relay".into(),
            run_id: "run-relay".into(),
            parent_run_id: None,
            identity: None,
        })
        .unwrap()
        .unwrap();
    event_store
        .append(normalized.draft, AppendOptions::default())
        .await
        .unwrap()
        .event
        .event_id
        .as_str()
        .to_string()
}

fn fast_config() -> ProtocolProjectorRelayConfig {
    ProtocolProjectorRelayConfig {
        idle_sleep: Duration::from_millis(1),
        error_sleep: Duration::from_millis(1),
        ..ProtocolProjectorRelayConfig::default()
    }
}

fn fast_fanout_config() -> ProtocolFanoutRelayConfig {
    ProtocolFanoutRelayConfig {
        idle_sleep: Duration::from_millis(1),
        error_sleep: Duration::from_millis(1),
        ..ProtocolFanoutRelayConfig::default()
    }
}

fn fast_a2a_push_config() -> A2aPushWebhookRelayConfig {
    A2aPushWebhookRelayConfig {
        idle_sleep: Duration::from_millis(1),
        error_sleep: Duration::from_millis(1),
        ..A2aPushWebhookRelayConfig::default()
    }
}

async fn replay_count(log: &InMemoryProtocolReplayLog) -> usize {
    log.list_replay(
        ProtocolStreamKey::new(
            "thread:thread-relay",
            AI_SDK_PROTOCOL,
            AI_SDK_PROTOCOL_VERSION,
        )
        .unwrap(),
        None,
        10,
    )
    .await
    .unwrap()
    .records
    .len()
}

#[derive(Default)]
struct RecordingFanoutPublisher {
    replay_ids: Mutex<Vec<String>>,
}

#[async_trait]
impl ProtocolReplayFanoutPublisher for RecordingFanoutPublisher {
    async fn publish(
        &self,
        message: ProtocolReplayFanoutMessage,
    ) -> Result<(), ProtocolReplayFanoutError> {
        self.replay_ids
            .lock()
            .push(message.record.protocol_replay_id.as_str().to_string());
        Ok(())
    }
}

#[tokio::test]
async fn protocol_projector_relay_projects_attached_outbox_in_background() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let event_id = append_run_start(&event_store).await;
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({ "event_id": event_id }),
    )
    .unwrap();
    draft.dedupe_key = Some(format!("canonical/{event_id}"));
    outbox.enqueue_outbox(draft).await.unwrap();
    let state = with_protocol_replay_log(make_state(), replay_log.clone());
    let state = with_protocol_projector_relay(
        state,
        outbox.clone(),
        event_store as Arc<dyn EventLookup>,
        replay_log.clone() as Arc<dyn ProtocolReplayWriter>,
        fast_config(),
    )
    .unwrap();
    assert!(protocol_replay_log(&state).is_some());

    let handle = start_protocol_projector_relay(&state).unwrap().unwrap();
    for _ in 0..50 {
        if replay_count(&replay_log).await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.shutdown().await;

    assert_eq!(replay_count(&replay_log).await, 2);
    let delivered = outbox
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
}

#[tokio::test]
async fn with_protocol_migrates_relay_attachments_to_new_buffers() {
    use crate::app::ProtocolModuleState;

    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());

    // Configure a projector relay *before* replacing the protocol module.
    let state = with_protocol_replay_log(make_state(), replay_log.clone());
    let state = with_protocol_projector_relay(
        state,
        outbox,
        event_store as Arc<dyn EventLookup>,
        replay_log as Arc<dyn ProtocolReplayWriter>,
        fast_config(),
    )
    .unwrap();

    // Replacing the protocol module swaps replay-buffer identity. The projector
    // attachment and replay log must migrate so the relay still starts instead
    // of being silently orphaned under the previous buffers.
    let state = state.with_protocol(ProtocolModuleState::new());

    assert!(
        protocol_replay_log(&state).is_some(),
        "replay log configured before with_protocol must survive the swap"
    );
    let handle = start_protocol_projector_relay(&state)
        .unwrap()
        .expect("projector relay configured before with_protocol must survive the swap");
    handle.shutdown().await;
}

#[tokio::test]
async fn protocol_fanout_relay_publishes_attached_outbox_in_background() {
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let publisher = Arc::new(RecordingFanoutPublisher::default());
    let record = replay_log
        .append_replay(
            ProtocolReplayDraft::new(
                "thread:thread-fanout-state",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
                "ai-sdk-projector-v1",
                "wire-fanout-state",
                "start",
                b"data: start\n\n".to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .record;
    outbox
        .enqueue_outbox(
            OutboxMessageDraft::new(
                OUTBOX_LANE_PROTOCOL_REPLAY,
                OUTBOX_TARGET_PROTOCOL_FANOUT,
                serde_json::json!({
                    "protocol_replay_id": record.protocol_replay_id.as_str(),
                    "protocol": record.protocol.as_str(),
                    "protocol_version": record.protocol_version.as_str(),
                    "wire_event_id": record.wire_event_id.as_str(),
                }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let state = with_protocol_fanout_relay(
        make_state(),
        outbox.clone(),
        replay_log,
        publisher.clone(),
        fast_fanout_config(),
    )
    .unwrap();

    let handles = start_protocol_relays(&state).await.unwrap();
    for _ in 0..50 {
        if publisher.replay_ids.lock().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handles.shutdown().await;

    assert_eq!(publisher.replay_ids.lock().len(), 1);
    let delivered = outbox
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
}

#[tokio::test]
async fn a2a_push_webhook_relay_attaches_outbox_and_starts() {
    let outbox: Arc<dyn OutboxStore> = Arc::new(InMemoryOutboxStore::new());
    let state =
        with_a2a_push_webhook_relay(make_state(), outbox.clone(), fast_a2a_push_config()).unwrap();

    let attached = a2a_push_webhook_outbox_for_buffers(&state.protocol.replay_buffers).unwrap();
    assert!(Arc::ptr_eq(&attached, &outbox));

    let handle = start_a2a_push_webhook_relay(&state).unwrap().unwrap();
    handle.shutdown_with_timeout(Duration::from_secs(1)).await;
}

// Regression: previously `run_outbox_relay` raced `relay.tick()` against
// `cancel.cancelled()` in the same `select!`, so a shutdown that fired
// after `claim_outbox` but before `ack`/`nack` would drop the tick
// future and leave the row claimed until lease expiry. The relay now
// only observes cancellation between ticks; verify a slow handler
// completes its delivery before shutdown returns.
#[tokio::test]
async fn shutdown_does_not_drop_in_flight_tick() {
    use awaken_contract::contract::outbox::OutboxStore;
    use tokio::sync::Notify;

    struct GatedHandler {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl crate::outbox_relay::OutboxRelayHandler for GatedHandler {
        async fn deliver(
            &self,
            _message: &awaken_contract::contract::outbox::OutboxMessage,
        ) -> Result<(), crate::outbox_relay::OutboxRelayError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    let outbox = Arc::new(InMemoryOutboxStore::new());
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({"event_id": "evt"}),
    )
    .unwrap();
    draft.dedupe_key = Some("dedupe".into());
    outbox.enqueue_outbox(draft).await.unwrap();

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handler = Arc::new(GatedHandler {
        entered: entered.clone(),
        release: release.clone(),
    });
    let relay = OutboxRelay::new(
        outbox.clone(),
        handler,
        OutboxRelayConfig {
            lane: OUTBOX_LANE_CANONICAL.to_string(),
            target: OUTBOX_TARGET_PROTOCOL_PROJECTOR.to_string(),
            consumer_id: "shutdown-test".into(),
            batch_limit: 10,
            lease_ms: 60_000,
            retry_delay_ms: 0,
            max_retry_delay_ms: 0,
        },
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let mut task = tokio::spawn(run_outbox_relay(
        relay,
        Duration::from_millis(1),
        Duration::from_millis(1),
        "shutdown-test",
        cancel.clone(),
    ));

    // Wait until the handler is mid-deliver, then request shutdown.
    entered.notified().await;
    cancel.cancel();

    // While the handler is blocked, the relay task must still be alive:
    // cancel-safe shutdown means the tick future is not dropped, so the
    // task cannot exit until the handler returns.
    let early = tokio::time::timeout(Duration::from_millis(25), &mut task).await;
    assert!(
        early.is_err(),
        "relay task exited mid-deliver, lost cancel-safety"
    );

    // Let the handler complete; the relay should ack then observe the
    // cancellation between ticks and shut down cleanly.
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("relay task did not shut down after handler released")
        .expect("relay task panicked");

    let delivered = outbox
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1, "row must be acked, not stuck claimed");
}

#[tokio::test]
async fn shutdown_timeout_bounds_stuck_in_flight_tick() {
    use awaken_contract::contract::outbox::OutboxStore;
    use tokio::sync::Notify;

    struct StuckHandler {
        entered: Arc<Notify>,
    }

    #[async_trait]
    impl crate::outbox_relay::OutboxRelayHandler for StuckHandler {
        async fn deliver(
            &self,
            _message: &awaken_contract::contract::outbox::OutboxMessage,
        ) -> Result<(), crate::outbox_relay::OutboxRelayError> {
            self.entered.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    let outbox = Arc::new(InMemoryOutboxStore::new());
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({"event_id": "evt-timeout"}),
    )
    .unwrap();
    draft.dedupe_key = Some("dedupe-timeout".into());
    outbox.enqueue_outbox(draft).await.unwrap();

    let entered = Arc::new(Notify::new());
    let relay = OutboxRelay::new(
        outbox.clone(),
        Arc::new(StuckHandler {
            entered: entered.clone(),
        }),
        OutboxRelayConfig {
            lane: OUTBOX_LANE_CANONICAL.to_string(),
            target: OUTBOX_TARGET_PROTOCOL_PROJECTOR.to_string(),
            consumer_id: "shutdown-timeout-test".into(),
            batch_limit: 10,
            lease_ms: 60_000,
            retry_delay_ms: 0,
            max_retry_delay_ms: 0,
        },
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let handle = ProtocolRelayHandle {
        task: tokio::spawn(run_outbox_relay(
            relay,
            Duration::from_millis(1),
            Duration::from_millis(1),
            "shutdown-timeout-test",
            cancel.clone(),
        )),
        cancel,
        name: "shutdown-timeout-test",
    };

    entered.notified().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        handle.shutdown_with_timeout(Duration::from_millis(25)),
    )
    .await
    .expect("shutdown timeout must bound a stuck handler");

    let claimed = outbox
        .list_outbox(Some(OutboxStatus::Claimed), 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1, "lease retry owns recovery after abort");
}
