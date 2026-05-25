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
#[path = "protocol_projector_tests.rs"]
mod tests;
