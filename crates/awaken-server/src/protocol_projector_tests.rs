use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use async_trait::async_trait;
use awaken_contract::contract::durable_event_sink::{
    AgentEventNormalizationContext, AgentEventNormalizer, ScopedAgentEventNormalizer,
};
use awaken_contract::contract::event_store::{AppendOptions, EventScope, EventWriter};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxMessage, OutboxMessageDraft,
    OutboxStatus, OutboxStore,
};
use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayAppendResult, ProtocolReplayReader, ProtocolReplayWriter, ProtocolStreamKey,
};
use awaken_stores::{InMemoryEventStore, InMemoryOutboxStore, InMemoryProtocolReplayLog};

use crate::outbox_relay::{OutboxRelay, OutboxRelayConfig, OutboxRelayError, OutboxRelayHandler};

struct FailsAfterProjectOnce {
    inner: Arc<CanonicalOutboxProtocolProjector>,
    fail_next: AtomicBool,
}

struct FailsAfterAppend {
    inner: Arc<InMemoryProtocolReplayLog>,
    successful_appends_before_failure: AtomicUsize,
}

impl FailsAfterProjectOnce {
    fn new(inner: Arc<CanonicalOutboxProtocolProjector>) -> Self {
        Self {
            inner,
            fail_next: AtomicBool::new(true),
        }
    }
}

impl FailsAfterAppend {
    fn new(
        inner: Arc<InMemoryProtocolReplayLog>,
        successful_appends_before_failure: usize,
    ) -> Self {
        Self {
            inner,
            successful_appends_before_failure: AtomicUsize::new(successful_appends_before_failure),
        }
    }
}

#[async_trait]
impl ProtocolReplayWriter for FailsAfterAppend {
    async fn append_replay(
        &self,
        draft: ProtocolReplayDraft,
    ) -> Result<ProtocolReplayAppendResult, ProtocolReplayError> {
        let result = self.inner.append_replay(draft).await?;
        let remaining = self
            .successful_appends_before_failure
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            })
            .expect("fetch_update closure always returns Some");
        if remaining == 1 {
            return Err(ProtocolReplayError::Io(
                "injected crash after partial replay append".to_string(),
            ));
        }
        Ok(result)
    }
}

#[async_trait]
impl OutboxRelayHandler for FailsAfterProjectOnce {
    async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError> {
        self.inner
            .project_outbox_message(message)
            .await
            .map_err(|error| OutboxRelayError::Delivery(error.to_string()))?;
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(OutboxRelayError::Delivery(
                "injected crash after replay projection before ack".to_string(),
            ));
        }
        Ok(())
    }
}

async fn canonical_event(agent_event: &AgentEvent) -> CanonicalEvent {
    append_canonical_event(&InMemoryEventStore::new(), agent_event).await
}

async fn append_canonical_event(
    event_store: &InMemoryEventStore,
    agent_event: &AgentEvent,
) -> CanonicalEvent {
    append_canonical_event_for(event_store, "thread-proto", "run-proto", agent_event).await
}

async fn append_canonical_event_for(
    event_store: &InMemoryEventStore,
    thread_id: &str,
    run_id: &str,
    agent_event: &AgentEvent,
) -> CanonicalEvent {
    let normalizer = ScopedAgentEventNormalizer::new(
        AgentEventNormalizationContext::new(thread_id, run_id, "test").unwrap(),
    );
    let normalized = normalizer.normalize(agent_event).unwrap().unwrap();
    event_store
        .append(normalized.draft, AppendOptions::default())
        .await
        .unwrap()
        .event
}

fn outbox_message_for(event: &CanonicalEvent) -> OutboxMessage {
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({
            "event_id": event.event_id.as_str(),
            "event_kind": event.event_kind.as_str(),
            "created_at": event.created_at,
        }),
    )
    .unwrap();
    draft.dedupe_key = Some(format!("canonical/{}", event.event_id.as_str()));
    OutboxMessage::from_enqueue("out-test".into(), draft, 1).unwrap()
}

fn relay_config() -> OutboxRelayConfig {
    OutboxRelayConfig {
        lane: OUTBOX_LANE_CANONICAL.to_string(),
        target: OUTBOX_TARGET_PROTOCOL_PROJECTOR.to_string(),
        consumer_id: "projector-test".to_string(),
        batch_limit: 10,
        lease_ms: 1_000,
        retry_delay_ms: 0,
        max_retry_delay_ms: 0,
    }
}

fn relay_config_for(consumer_id: &str) -> OutboxRelayConfig {
    OutboxRelayConfig {
        consumer_id: consumer_id.to_string(),
        ..relay_config()
    }
}

#[tokio::test]
async fn ai_sdk_projector_writes_byte_stable_replay_rows() {
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let projector = AiSdkProtocolProjector::new(replay_log.clone());
    let event = canonical_event(&AgentEvent::RunStart {
        thread_id: "thread-proto".into(),
        run_id: "run-proto".into(),
        parent_run_id: None,
        identity: None,
    })
    .await;

    let records = projector.project_event(&event).await.unwrap();

    assert_eq!(records.len(), 2);
    assert_eq!(records[0].protocol, AI_SDK_PROTOCOL);
    assert_eq!(records[0].protocol_version, AI_SDK_PROTOCOL_VERSION);
    assert_eq!(records[0].source_event_ids[0], event.event_id);
    assert_eq!(records[0].wire_event_type, "start");
    assert_eq!(
        records[0].wire_payload_bytes,
        br#"{"type":"start","messageId":"run-proto"}"#
    );

    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(page.records, records);
}

#[tokio::test]
async fn ag_ui_projector_writes_protocol_replay_rows() {
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let projector = AgUiProtocolProjector::new(replay_log.clone());
    let event = canonical_event(&AgentEvent::RunStart {
        thread_id: "thread-proto".into(),
        run_id: "run-proto".into(),
        parent_run_id: None,
        identity: None,
    })
    .await;

    let records = projector.project_event(&event).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].protocol, AG_UI_PROTOCOL);
    assert_eq!(records[0].protocol_version, AG_UI_PROTOCOL_VERSION);
    assert_eq!(records[0].source_event_ids[0], event.event_id);
    assert_eq!(records[0].wire_event_type, "RUN_STARTED");
    assert!(
        std::str::from_utf8(&records[0].wire_payload_bytes)
            .unwrap()
            .contains(r#""type":"RUN_STARTED""#)
    );

    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AG_UI_PROTOCOL,
                AG_UI_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(page.records, records);
}

#[tokio::test]
async fn ai_sdk_projector_is_idempotent_for_same_wire_event_ids() {
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let projector = AiSdkProtocolProjector::new(replay_log);
    let event = canonical_event(&AgentEvent::RunFinish {
        thread_id: "thread-proto".into(),
        run_id: "run-proto".into(),
        identity: None,
        result: None,
        termination: TerminationReason::NaturalEnd,
    })
    .await;

    let first = projector.project_event(&event).await.unwrap();
    let second = projector.project_event(&event).await.unwrap();

    assert_eq!(first, second);
}

#[tokio::test]
async fn ai_sdk_projector_isolates_encoder_state_by_thread_stream() {
    let event_store = InMemoryEventStore::new();
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let projector = AiSdkProtocolProjector::new(replay_log.clone());
    let start_a = append_canonical_event_for(
        &event_store,
        "thread-a",
        "run-a",
        &AgentEvent::RunStart {
            thread_id: "thread-a".into(),
            run_id: "run-a".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    let text_a = append_canonical_event_for(
        &event_store,
        "thread-a",
        "run-a",
        &AgentEvent::TextDelta {
            delta: "hello".into(),
        },
    )
    .await;
    let start_b = append_canonical_event_for(
        &event_store,
        "thread-b",
        "run-b",
        &AgentEvent::RunStart {
            thread_id: "thread-b".into(),
            run_id: "run-b".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    let text_b = append_canonical_event_for(
        &event_store,
        "thread-b",
        "run-b",
        &AgentEvent::TextDelta {
            delta: "world".into(),
        },
    )
    .await;

    projector.project_event(&start_a).await.unwrap();
    projector.project_event(&text_a).await.unwrap();
    projector.project_event(&start_b).await.unwrap();
    projector.project_event(&text_b).await.unwrap();

    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new("thread:thread-b", AI_SDK_PROTOCOL, AI_SDK_PROTOCOL_VERSION)
                .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    let wire_types = page
        .records
        .iter()
        .map(|record| record.wire_event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        wire_types,
        ["start", "data-run-info", "text-start", "text-delta"]
    );
    assert_eq!(
        page.records[2].wire_payload_bytes,
        br#"{"type":"text-start","id":"txt_0"}"#
    );
}

#[tokio::test]
async fn ai_sdk_projector_skips_non_runtime_domain_events() {
    let event_store = InMemoryEventStore::new();
    let draft = awaken_contract::contract::event_store::CanonicalEventDraft::new(
        vec![EventScope::thread("thread-proto")],
        awaken_contract::contract::event_store::CanonicalEventKind::new("RunQueued").unwrap(),
        serde_json::json!({ "dispatch_id": "dispatch-1" }),
        "test",
    )
    .unwrap();
    let event = event_store
        .append(draft, AppendOptions::default())
        .await
        .unwrap()
        .event;
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let projector = AiSdkProtocolProjector::new(replay_log);

    let records = projector.project_event(&event).await.unwrap();

    assert!(records.is_empty());
}

#[tokio::test]
async fn canonical_outbox_relay_projects_agent_events_and_acks() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let event = append_canonical_event(
        &event_store,
        &AgentEvent::RunStart {
            thread_id: "thread-proto".into(),
            run_id: "run-proto".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    outbox
        .enqueue_outbox({
            let mut draft = OutboxMessageDraft::new(
                OUTBOX_LANE_CANONICAL,
                OUTBOX_TARGET_PROTOCOL_PROJECTOR,
                serde_json::json!({
                    "event_id": event.event_id.as_str(),
                    "event_kind": event.event_kind.as_str(),
                    "created_at": event.created_at,
                }),
            )
            .unwrap();
            draft.dedupe_key = Some(format!("canonical/{}", event.event_id.as_str()));
            draft
        })
        .await
        .unwrap();
    let handler = Arc::new(CanonicalOutboxProtocolProjector::new(
        event_store.clone(),
        replay_log.clone(),
    ));
    let relay = OutboxRelay::new(outbox.clone(), handler, relay_config()).unwrap();

    let stats = relay.tick().await.unwrap();

    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.delivered, 1);
    let delivered = outbox
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.records[0].source_event_ids[0], event.event_id);
}

#[tokio::test]
async fn canonical_outbox_projector_can_project_all_protocols() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let event = append_canonical_event(
        &event_store,
        &AgentEvent::RunStart {
            thread_id: "thread-proto".into(),
            run_id: "run-proto".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    let handler =
        CanonicalOutboxProtocolProjector::new_all_protocols(event_store, replay_log.clone());

    let records = handler
        .project_outbox_message(&outbox_message_for(&event))
        .await
        .unwrap();

    assert_eq!(records.len(), 3);
    let ai_sdk = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    let ag_ui = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AG_UI_PROTOCOL,
                AG_UI_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(ai_sdk.records.len(), 2);
    assert_eq!(ag_ui.records.len(), 1);
}

#[tokio::test]
async fn canonical_outbox_multi_relay_claims_project_once_under_contention() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let event = append_canonical_event(
        &event_store,
        &AgentEvent::RunStart {
            thread_id: "thread-proto".into(),
            run_id: "run-proto".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    outbox
        .enqueue_outbox({
            let mut draft = OutboxMessageDraft::new(
                OUTBOX_LANE_CANONICAL,
                OUTBOX_TARGET_PROTOCOL_PROJECTOR,
                serde_json::json!({
                    "event_id": event.event_id.as_str(),
                    "event_kind": event.event_kind.as_str(),
                    "created_at": event.created_at,
                }),
            )
            .unwrap();
            draft.dedupe_key = Some(format!("canonical/{}", event.event_id.as_str()));
            draft
        })
        .await
        .unwrap();
    let handler = Arc::new(CanonicalOutboxProtocolProjector::new(
        event_store.clone(),
        replay_log.clone(),
    ));
    let relay_a =
        OutboxRelay::new(outbox.clone(), handler.clone(), relay_config_for("relay-a")).unwrap();
    let relay_b =
        OutboxRelay::new(outbox.clone(), handler.clone(), relay_config_for("relay-b")).unwrap();

    let (stats_a, stats_b) = tokio::join!(relay_a.tick(), relay_b.tick());
    let stats_a = stats_a.expect("relay a");
    let stats_b = stats_b.expect("relay b");

    assert_eq!(
        stats_a.claimed + stats_b.claimed,
        1,
        "only one relay instance may claim the outbox message"
    );
    assert_eq!(stats_a.delivered + stats_b.delivered, 1);
    let delivered = outbox
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        page.records.len(),
        2,
        "contended relays must not duplicate replay rows"
    );
}

#[tokio::test]
async fn canonical_outbox_projector_is_idempotent_for_duplicate_delivery() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let event = append_canonical_event(
        &event_store,
        &AgentEvent::RunStart {
            thread_id: "thread-proto".into(),
            run_id: "run-proto".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    let handler = CanonicalOutboxProtocolProjector::new(event_store, replay_log.clone());
    let message = outbox_message_for(&event);

    let first = handler.project_outbox_message(&message).await.unwrap();
    let second = handler.project_outbox_message(&message).await.unwrap();

    assert_eq!(first, second);
    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(page.records.len(), first.len());
}

#[tokio::test]
async fn canonical_outbox_relay_restart_after_post_projection_failure_is_idempotent() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let event = append_canonical_event(
        &event_store,
        &AgentEvent::RunStart {
            thread_id: "thread-proto".into(),
            run_id: "run-proto".into(),
            parent_run_id: None,
            identity: None,
        },
    )
    .await;
    outbox
        .enqueue_outbox({
            let mut draft = OutboxMessageDraft::new(
                OUTBOX_LANE_CANONICAL,
                OUTBOX_TARGET_PROTOCOL_PROJECTOR,
                serde_json::json!({
                    "event_id": event.event_id.as_str(),
                    "event_kind": event.event_kind.as_str(),
                    "created_at": event.created_at,
                }),
            )
            .unwrap();
            draft.dedupe_key = Some(format!("canonical/{}", event.event_id.as_str()));
            draft
        })
        .await
        .unwrap();
    let projector = Arc::new(CanonicalOutboxProtocolProjector::new(
        event_store.clone(),
        replay_log.clone(),
    ));
    let first_relay = OutboxRelay::new(
        outbox.clone(),
        Arc::new(FailsAfterProjectOnce::new(projector.clone())),
        relay_config(),
    )
    .unwrap();

    let first = first_relay.tick().await.unwrap();
    assert_eq!(first.claimed, 1);
    assert_eq!(first.delivered, 0);
    assert_eq!(first.requeued, 1);
    let projected_after_failure = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap()
        .records;
    assert_eq!(
        projected_after_failure.len(),
        2,
        "first delivery projected replay rows before the simulated crash"
    );

    let restarted_relay =
        OutboxRelay::new(outbox.clone(), projector.clone(), relay_config()).unwrap();
    let second = restarted_relay.tick().await.unwrap();
    assert_eq!(second.claimed, 1);
    assert_eq!(second.delivered, 1);
    let delivered = outbox
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);

    let projected_after_restart = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap()
        .records;
    assert_eq!(
        projected_after_restart, projected_after_failure,
        "restart must ack the duplicate delivery without appending duplicate replay rows"
    );
}

#[tokio::test]
async fn ai_sdk_projector_restart_after_partial_replay_append_completes_missing_rows() {
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let failing_writer = Arc::new(FailsAfterAppend::new(replay_log.clone(), 1));
    let failing_projector = AiSdkProtocolProjector::new(failing_writer);
    let event = canonical_event(&AgentEvent::RunStart {
        thread_id: "thread-proto".into(),
        run_id: "run-proto".into(),
        parent_run_id: None,
        identity: None,
    })
    .await;

    let error = failing_projector
        .project_event(&event)
        .await
        .expect_err("partial append fault must fail the first projection");
    assert!(
        error.to_string().contains("partial replay append"),
        "unexpected error: {error}"
    );
    let partial = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap()
        .records;
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].wire_event_type, "start");

    let restarted_projector = AiSdkProtocolProjector::new(replay_log.clone());
    let completed = restarted_projector.project_event(&event).await.unwrap();

    assert_eq!(
        completed.len(),
        2,
        "restart must return the complete projection for the event"
    );
    assert_eq!(
        completed[0], partial[0],
        "restart must reuse the already-appended replay row idempotently"
    );
    let page = replay_log
        .list_replay(
            ProtocolStreamKey::new(
                "thread:thread-proto",
                AI_SDK_PROTOCOL,
                AI_SDK_PROTOCOL_VERSION,
            )
            .unwrap(),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        page.records, completed,
        "partial replay append recovery must not leave duplicate rows"
    );
}

#[tokio::test]
async fn canonical_outbox_relay_dead_letters_invalid_payload() {
    let event_store = Arc::new(InMemoryEventStore::new());
    let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
    let outbox = Arc::new(InMemoryOutboxStore::new());
    let mut draft = OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({ "event_kind": "RunStarted" }),
    )
    .unwrap();
    draft.max_attempts = 1;
    outbox.enqueue_outbox(draft).await.unwrap();
    let handler = Arc::new(CanonicalOutboxProtocolProjector::new(
        event_store,
        replay_log,
    ));
    let relay = OutboxRelay::new(outbox.clone(), handler, relay_config()).unwrap();

    let stats = relay.tick().await.unwrap();

    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.dead_lettered, 1);
    let dead = outbox
        .list_outbox(Some(OutboxStatus::DeadLetter), 10)
        .await
        .unwrap();
    assert_eq!(dead.len(), 1);
    assert!(dead[0].last_error.as_deref().unwrap().contains("event_id"));
}
