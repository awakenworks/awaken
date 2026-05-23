//! Protocol replay outbox fanout relay.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::outbox::{
    OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_PROTOCOL_FANOUT, OutboxMessage,
};
use awaken_contract::contract::protocol_replay_log::{
    ProtocolReplayId, ProtocolReplayLookup, ProtocolReplayRecord,
};
use serde::Deserialize;
use thiserror::Error;

use crate::outbox_relay::{OutboxRelayError, OutboxRelayHandler};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolReplayFanoutNotification {
    pub protocol_replay_id: ProtocolReplayId,
    pub protocol: String,
    pub protocol_version: String,
    pub wire_event_id: String,
}

impl ProtocolReplayFanoutNotification {
    pub fn from_outbox_message(message: &OutboxMessage) -> Result<Self, OutboxRelayError> {
        validate_protocol_fanout_route(message)?;
        let payload: ProtocolReplayFanoutPayload = serde_json::from_value(message.payload.clone())
            .map_err(|error| OutboxRelayError::Validation(error.to_string()))?;
        let notification = Self {
            protocol_replay_id: ProtocolReplayId::new(payload.protocol_replay_id)
                .map_err(|error| OutboxRelayError::Validation(error.to_string()))?,
            protocol: payload.protocol,
            protocol_version: payload.protocol_version,
            wire_event_id: payload.wire_event_id,
        };
        notification.validate()?;
        Ok(notification)
    }

    fn validate(&self) -> Result<(), OutboxRelayError> {
        reject_blank("protocol", &self.protocol)?;
        reject_blank("protocol_version", &self.protocol_version)?;
        reject_blank("wire_event_id", &self.wire_event_id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolReplayFanoutMessage {
    pub outbox_id: String,
    pub notification: ProtocolReplayFanoutNotification,
    pub record: ProtocolReplayRecord,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolReplayFanoutError {
    #[error("publish failed: {0}")]
    Publish(String),
}

#[async_trait]
pub trait ProtocolReplayFanoutPublisher: Send + Sync {
    async fn publish(
        &self,
        message: ProtocolReplayFanoutMessage,
    ) -> Result<(), ProtocolReplayFanoutError>;
}

pub struct ProtocolReplayFanoutRelayHandler {
    replay_lookup: Arc<dyn ProtocolReplayLookup>,
    publisher: Arc<dyn ProtocolReplayFanoutPublisher>,
}

impl ProtocolReplayFanoutRelayHandler {
    #[must_use]
    pub fn new(
        replay_lookup: Arc<dyn ProtocolReplayLookup>,
        publisher: Arc<dyn ProtocolReplayFanoutPublisher>,
    ) -> Self {
        Self {
            replay_lookup,
            publisher,
        }
    }
}

#[async_trait]
impl OutboxRelayHandler for ProtocolReplayFanoutRelayHandler {
    async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError> {
        let notification = ProtocolReplayFanoutNotification::from_outbox_message(message)?;
        let record = self
            .replay_lookup
            .load_replay(&notification.protocol_replay_id)
            .await
            .map_err(|error| {
                OutboxRelayError::Delivery(format!("protocol replay lookup failed: {error}"))
            })?
            .ok_or_else(|| {
                OutboxRelayError::Delivery(format!(
                    "protocol replay row not found: {}",
                    notification.protocol_replay_id.as_str()
                ))
            })?;
        validate_notification_matches_record(&notification, &record)?;
        self.publisher
            .publish(ProtocolReplayFanoutMessage {
                outbox_id: message.outbox_id.clone(),
                notification,
                record,
            })
            .await
            .map_err(|error| OutboxRelayError::Delivery(error.to_string()))
    }
}

#[derive(Deserialize)]
struct ProtocolReplayFanoutPayload {
    protocol_replay_id: String,
    protocol: String,
    protocol_version: String,
    wire_event_id: String,
}

fn validate_protocol_fanout_route(message: &OutboxMessage) -> Result<(), OutboxRelayError> {
    if message.lane == OUTBOX_LANE_PROTOCOL_REPLAY
        && message.target == OUTBOX_TARGET_PROTOCOL_FANOUT
    {
        return Ok(());
    }
    Err(OutboxRelayError::Validation(format!(
        "unexpected outbox message route: lane={}, target={}",
        message.lane, message.target
    )))
}

fn validate_notification_matches_record(
    notification: &ProtocolReplayFanoutNotification,
    record: &ProtocolReplayRecord,
) -> Result<(), OutboxRelayError> {
    if notification.protocol == record.protocol
        && notification.protocol_version == record.protocol_version
        && notification.wire_event_id == record.wire_event_id
    {
        return Ok(());
    }
    Err(OutboxRelayError::Validation(format!(
        "protocol replay fanout payload does not match row: {}",
        notification.protocol_replay_id.as_str()
    )))
}

fn reject_blank(field: &str, value: &str) -> Result<(), OutboxRelayError> {
    if value.trim().is_empty() {
        return Err(OutboxRelayError::Validation(format!("{field} is required")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::outbox::{OutboxMessageDraft, OutboxStatus, OutboxStore};
    use awaken_contract::contract::protocol_replay_log::{
        ProtocolReplayDraft, ProtocolReplayWriter,
    };
    use awaken_stores::{InMemoryOutboxStore, InMemoryProtocolReplayLog};
    use parking_lot::Mutex;
    use serde_json::json;

    use crate::outbox_relay::{OutboxRelay, OutboxRelayConfig};
    use crate::protocol_projector::{AI_SDK_PROTOCOL, AI_SDK_PROTOCOL_VERSION};

    #[derive(Default)]
    struct RecordingPublisher {
        messages: Mutex<Vec<ProtocolReplayFanoutMessage>>,
    }

    #[async_trait]
    impl ProtocolReplayFanoutPublisher for RecordingPublisher {
        async fn publish(
            &self,
            message: ProtocolReplayFanoutMessage,
        ) -> Result<(), ProtocolReplayFanoutError> {
            self.messages.lock().push(message);
            Ok(())
        }
    }

    struct FailingPublisher;

    #[async_trait]
    impl ProtocolReplayFanoutPublisher for FailingPublisher {
        async fn publish(
            &self,
            _message: ProtocolReplayFanoutMessage,
        ) -> Result<(), ProtocolReplayFanoutError> {
            Err(ProtocolReplayFanoutError::Publish(
                "bus unavailable".to_string(),
            ))
        }
    }

    fn relay_config() -> OutboxRelayConfig {
        OutboxRelayConfig {
            lane: OUTBOX_LANE_PROTOCOL_REPLAY.to_string(),
            target: OUTBOX_TARGET_PROTOCOL_FANOUT.to_string(),
            consumer_id: "fanout-test".to_string(),
            batch_limit: 10,
            lease_ms: 1_000,
            retry_delay_ms: 0,
            max_retry_delay_ms: 0,
        }
    }

    fn replay_draft(wire_event_id: &str) -> ProtocolReplayDraft {
        ProtocolReplayDraft::new(
            "thread:thread-fanout",
            AI_SDK_PROTOCOL,
            AI_SDK_PROTOCOL_VERSION,
            "ai-sdk-projector-v1",
            wire_event_id,
            "start",
            b"data: start\n\n".to_vec(),
        )
        .unwrap()
    }

    async fn append_replay(log: &InMemoryProtocolReplayLog) -> ProtocolReplayRecord {
        log.append_replay(replay_draft("wire-1"))
            .await
            .unwrap()
            .record
    }

    fn outbox_draft(record: &ProtocolReplayRecord) -> OutboxMessageDraft {
        OutboxMessageDraft::new(
            OUTBOX_LANE_PROTOCOL_REPLAY,
            OUTBOX_TARGET_PROTOCOL_FANOUT,
            json!({
                "protocol_replay_id": record.protocol_replay_id.as_str(),
                "protocol": record.protocol,
                "protocol_version": record.protocol_version,
                "wire_event_id": record.wire_event_id,
            }),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn relay_loads_protocol_replay_row_publishes_and_acks() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let record = append_replay(&replay_log).await;
        outbox.enqueue_outbox(outbox_draft(&record)).await.unwrap();
        let publisher = Arc::new(RecordingPublisher::default());
        let handler = Arc::new(ProtocolReplayFanoutRelayHandler::new(
            replay_log,
            publisher.clone(),
        ));
        let relay = OutboxRelay::new(outbox.clone(), handler, relay_config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.delivered, 1);
        let published = publisher.messages.lock().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].record.protocol_replay_id,
            record.protocol_replay_id
        );
        assert_eq!(published[0].record.wire_payload_bytes, b"data: start\n\n");
        let delivered = outbox
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
    }

    #[tokio::test]
    async fn relay_dead_letters_invalid_payload_without_publishing() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let mut draft = OutboxMessageDraft::new(
            OUTBOX_LANE_PROTOCOL_REPLAY,
            OUTBOX_TARGET_PROTOCOL_FANOUT,
            json!({ "protocol_replay_id": "pr-missing-fields" }),
        )
        .unwrap();
        draft.max_attempts = 1;
        outbox.enqueue_outbox(draft).await.unwrap();
        let publisher = Arc::new(RecordingPublisher::default());
        let handler = Arc::new(ProtocolReplayFanoutRelayHandler::new(
            replay_log,
            publisher.clone(),
        ));
        let relay = OutboxRelay::new(outbox.clone(), handler, relay_config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.dead_lettered, 1);
        assert!(publisher.messages.lock().is_empty());
        let dead = outbox
            .list_outbox(Some(OutboxStatus::DeadLetter), 10)
            .await
            .unwrap();
        assert!(dead[0].last_error.as_deref().unwrap().contains("protocol"));
    }

    #[tokio::test]
    async fn relay_dead_letters_mismatched_notification() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let record = append_replay(&replay_log).await;
        let mut draft = outbox_draft(&record);
        draft.payload["wire_event_id"] = json!("wire-other");
        draft.max_attempts = 1;
        outbox.enqueue_outbox(draft).await.unwrap();
        let publisher = Arc::new(RecordingPublisher::default());
        let handler = Arc::new(ProtocolReplayFanoutRelayHandler::new(
            replay_log,
            publisher.clone(),
        ));
        let relay = OutboxRelay::new(outbox.clone(), handler, relay_config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.dead_lettered, 1);
        assert!(publisher.messages.lock().is_empty());
        let dead = outbox
            .list_outbox(Some(OutboxStatus::DeadLetter), 10)
            .await
            .unwrap();
        assert!(
            dead[0]
                .last_error
                .as_deref()
                .unwrap()
                .contains("does not match")
        );
    }

    #[tokio::test]
    async fn relay_dead_letters_publisher_failures_after_max_attempts() {
        let replay_log = Arc::new(InMemoryProtocolReplayLog::new());
        let outbox = Arc::new(InMemoryOutboxStore::new());
        let record = append_replay(&replay_log).await;
        let mut draft = outbox_draft(&record);
        draft.max_attempts = 1;
        outbox.enqueue_outbox(draft).await.unwrap();
        let handler = Arc::new(ProtocolReplayFanoutRelayHandler::new(
            replay_log,
            Arc::new(FailingPublisher),
        ));
        let relay = OutboxRelay::new(outbox.clone(), handler, relay_config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.dead_lettered, 1);
        let dead = outbox
            .list_outbox(Some(OutboxStatus::DeadLetter), 10)
            .await
            .unwrap();
        assert!(
            dead[0]
                .last_error
                .as_deref()
                .unwrap()
                .contains("bus unavailable")
        );
    }
}
