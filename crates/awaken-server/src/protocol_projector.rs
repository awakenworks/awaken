//! Protocol replay projectors backed by `ProtocolReplayLog`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use async_trait::async_trait;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_store::{
    CanonicalEvent, CanonicalEventId, EventLookup, EventStoreError,
};
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxMessage,
};
use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayDraft, ProtocolReplayError, ProtocolReplayRecord, ProtocolReplayWriter,
    SourceEventCursor,
};
use awaken_contract::contract::transport::Transcoder;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::outbox_relay::{OutboxRelayError, OutboxRelayHandler};
use crate::protocols::ai_sdk_v6::encoder::AiSdkEncoder;

pub const AI_SDK_PROTOCOL: &str = "ai-sdk";
pub const AI_SDK_PROTOCOL_VERSION: &str = "v6-ui-message-stream";
pub const AI_SDK_PROJECTOR_VERSION: &str = "awaken-ai-sdk-v1";
const PROJECTION_CACHE_LIMIT: usize = 4_096;

#[derive(Debug, Error)]
pub enum ProtocolProjectorError {
    #[error("event payload is not a runtime AgentEvent: {0}")]
    EventPayload(String),
    #[error("event cannot be projected into a thread protocol stream: {0}")]
    MissingThreadScope(String),
    #[error("outbox message payload is invalid: {0}")]
    OutboxPayload(String),
    #[error("unexpected outbox message route: lane={lane}, target={target}")]
    UnexpectedOutboxRoute { lane: String, target: String },
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error(transparent)]
    EventStore(#[from] EventStoreError),
    #[error(transparent)]
    Replay(#[from] ProtocolReplayError),
}

pub struct AiSdkProtocolProjector {
    writer: Arc<dyn ProtocolReplayWriter>,
    state: Mutex<AiSdkProjectionState>,
}

impl AiSdkProtocolProjector {
    #[must_use]
    pub fn new(writer: Arc<dyn ProtocolReplayWriter>) -> Self {
        Self {
            writer,
            state: Mutex::new(AiSdkProjectionState::default()),
        }
    }

    pub async fn project_event(
        &self,
        event: &CanonicalEvent,
    ) -> Result<Vec<ProtocolReplayRecord>, ProtocolProjectorError> {
        if let Some(records) = self.state.lock().cache.records(&event.event_id) {
            return Ok(records);
        }
        let Some(agent_event) = decode_agent_event(event)? else {
            return Ok(Vec::new());
        };
        let stream_id = thread_stream_id(event)?;
        let drafts = {
            let mut state = self.state.lock();
            if let Some(records) = state.cache.records(&event.event_id) {
                return Ok(records);
            }
            if let Some(drafts) = state.cache.drafts(&event.event_id) {
                drafts
            } else {
                let outputs = state
                    .encoders
                    .entry(stream_id.clone())
                    .or_default()
                    .transcode(&agent_event);
                let drafts = outputs
                    .iter()
                    .enumerate()
                    .map(|(index, output)| replay_draft(event, &stream_id, index, output))
                    .collect::<Result<Vec<_>, _>>()?;
                state
                    .cache
                    .remember_drafts(event.event_id.clone(), drafts.clone());
                drafts
            }
        };
        let mut records = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let record = self.writer.append_replay(draft).await?.record;
            records.push(record);
        }
        self.state
            .lock()
            .cache
            .remember_records(event.event_id.clone(), records.clone());
        Ok(records)
    }
}

pub struct CanonicalOutboxProtocolProjector {
    event_lookup: Arc<dyn EventLookup>,
    projector: Arc<AiSdkProtocolProjector>,
}

impl CanonicalOutboxProtocolProjector {
    #[must_use]
    pub fn new(
        event_lookup: Arc<dyn EventLookup>,
        replay_writer: Arc<dyn ProtocolReplayWriter>,
    ) -> Self {
        Self::from_projector(
            event_lookup,
            Arc::new(AiSdkProtocolProjector::new(replay_writer)),
        )
    }

    #[must_use]
    pub fn from_projector(
        event_lookup: Arc<dyn EventLookup>,
        projector: Arc<AiSdkProtocolProjector>,
    ) -> Self {
        Self {
            event_lookup,
            projector,
        }
    }

    pub async fn project_outbox_message(
        &self,
        message: &OutboxMessage,
    ) -> Result<Vec<ProtocolReplayRecord>, ProtocolProjectorError> {
        validate_canonical_projector_message(message)?;
        let event_id = outbox_event_id(message)?;
        let event = self.event_lookup.load_event(&event_id).await?;
        self.projector.project_event(&event).await
    }
}

#[async_trait]
impl OutboxRelayHandler for CanonicalOutboxProtocolProjector {
    async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError> {
        self.project_outbox_message(message)
            .await
            .map(|_| ())
            .map_err(|error| OutboxRelayError::Delivery(error.to_string()))
    }
}

#[derive(Debug, Default)]
struct AiSdkProjectionState {
    encoders: BTreeMap<String, AiSdkEncoder>,
    cache: ProjectionCache,
}

#[derive(Debug, Default)]
struct ProjectionCache {
    drafts: BTreeMap<CanonicalEventId, Vec<ProtocolReplayDraft>>,
    records: BTreeMap<CanonicalEventId, Vec<ProtocolReplayRecord>>,
    order: VecDeque<CanonicalEventId>,
}

impl ProjectionCache {
    fn records(&self, event_id: &CanonicalEventId) -> Option<Vec<ProtocolReplayRecord>> {
        self.records.get(event_id).cloned()
    }

    fn drafts(&self, event_id: &CanonicalEventId) -> Option<Vec<ProtocolReplayDraft>> {
        self.drafts.get(event_id).cloned()
    }

    fn remember_drafts(&mut self, event_id: CanonicalEventId, drafts: Vec<ProtocolReplayDraft>) {
        self.track(event_id.clone());
        self.drafts.insert(event_id, drafts);
        self.evict();
    }

    fn remember_records(&mut self, event_id: CanonicalEventId, records: Vec<ProtocolReplayRecord>) {
        self.track(event_id.clone());
        self.records.insert(event_id, records);
        self.evict();
    }

    fn track(&mut self, event_id: CanonicalEventId) {
        if !self.drafts.contains_key(&event_id) && !self.records.contains_key(&event_id) {
            self.order.push_back(event_id);
        }
    }

    fn evict(&mut self) {
        while self.order.len() > PROJECTION_CACHE_LIMIT {
            if let Some(event_id) = self.order.pop_front() {
                self.drafts.remove(&event_id);
                self.records.remove(&event_id);
            }
        }
    }
}

fn validate_canonical_projector_message(
    message: &OutboxMessage,
) -> Result<(), ProtocolProjectorError> {
    if message.lane == OUTBOX_LANE_CANONICAL && message.target == OUTBOX_TARGET_PROTOCOL_PROJECTOR {
        return Ok(());
    }
    Err(ProtocolProjectorError::UnexpectedOutboxRoute {
        lane: message.lane.clone(),
        target: message.target.clone(),
    })
}

fn outbox_event_id(message: &OutboxMessage) -> Result<CanonicalEventId, ProtocolProjectorError> {
    let event_id = message
        .payload
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolProjectorError::OutboxPayload("event_id is required".into()))?;
    CanonicalEventId::new(event_id)
        .map_err(|error| ProtocolProjectorError::OutboxPayload(error.to_string()))
}

fn decode_agent_event(
    event: &CanonicalEvent,
) -> Result<Option<AgentEvent>, ProtocolProjectorError> {
    if event.payload.get("event_type").is_none() {
        return Ok(None);
    }
    serde_json::from_value(event.payload.clone())
        .map(Some)
        .map_err(|error| ProtocolProjectorError::EventPayload(error.to_string()))
}

fn thread_stream_id(event: &CanonicalEvent) -> Result<String, ProtocolProjectorError> {
    event
        .thread_id
        .as_ref()
        .map(|thread_id| format!("thread:{thread_id}"))
        .ok_or_else(|| {
            ProtocolProjectorError::MissingThreadScope(event.event_id.as_str().to_string())
        })
}

fn replay_draft<T: Serialize>(
    event: &CanonicalEvent,
    stream_id: &str,
    index: usize,
    output: &T,
) -> Result<ProtocolReplayDraft, ProtocolProjectorError> {
    let wire_payload_json = serde_json::to_value(output)
        .map_err(|error| ProtocolProjectorError::Serialization(error.to_string()))?;
    let wire_payload_bytes = serde_json::to_vec(output)
        .map_err(|error| ProtocolProjectorError::Serialization(error.to_string()))?;
    let wire_event_type = wire_payload_json
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut draft = ProtocolReplayDraft::new(
        stream_id.to_string(),
        AI_SDK_PROTOCOL,
        AI_SDK_PROTOCOL_VERSION,
        AI_SDK_PROJECTOR_VERSION,
        format!("{}:{index}", event.event_id.as_str()),
        wire_event_type,
        wire_payload_bytes,
    )?;
    draft.wire_payload_json = Some(wire_payload_json);
    draft.source_event_ids = vec![event.event_id.clone()];
    draft.source_event_cursors = event
        .cursors_by_scope
        .iter()
        .map(|(scope, cursor)| {
            SourceEventCursor::new(event.event_id.clone(), scope.clone(), cursor.clone())
        })
        .collect();
    Ok(draft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::durable_event_sink::{
        AgentEventNormalizationContext, AgentEventNormalizer, ScopedAgentEventNormalizer,
    };
    use awaken_contract::contract::event_store::{AppendOptions, EventScope, EventWriter};
    use awaken_contract::contract::lifecycle::TerminationReason;
    use awaken_contract::contract::outbox::{
        OUTBOX_LANE_CANONICAL, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxMessage, OutboxMessageDraft,
        OutboxStatus, OutboxStore,
    };
    use awaken_contract::contract::protocol_replay_log::{ProtocolReplayReader, ProtocolStreamKey};
    use awaken_stores::{InMemoryEventStore, InMemoryOutboxStore, InMemoryProtocolReplayLog};

    use crate::outbox_relay::{OutboxRelay, OutboxRelayConfig};

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
}
