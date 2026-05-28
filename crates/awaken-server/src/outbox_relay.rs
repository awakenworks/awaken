//! Generic at-least-once outbox relay.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::outbox::{
    OutboxError, OutboxMessage, OutboxNackOutcome, OutboxStore,
};
use awaken_server_contract::now_ms;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OutboxRelayError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error("delivery error: {0}")]
    Delivery(String),
    #[error(transparent)]
    Store(#[from] OutboxError),
}

#[async_trait]
pub trait OutboxRelayHandler: Send + Sync {
    async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRelayConfig {
    pub lane: String,
    pub target: String,
    pub consumer_id: String,
    pub batch_limit: usize,
    pub lease_ms: u64,
    pub retry_delay_ms: u64,
    pub max_retry_delay_ms: u64,
}

impl OutboxRelayConfig {
    pub fn validate(&self) -> Result<(), OutboxRelayError> {
        reject_blank("lane", &self.lane)?;
        reject_blank("target", &self.target)?;
        reject_blank("consumer_id", &self.consumer_id)?;
        if self.batch_limit == 0 {
            return Err(OutboxRelayError::Validation(
                "batch_limit must be greater than zero".to_string(),
            ));
        }
        if self.lease_ms == 0 {
            return Err(OutboxRelayError::Validation(
                "lease_ms must be greater than zero".to_string(),
            ));
        }
        if self.max_retry_delay_ms < self.retry_delay_ms {
            return Err(OutboxRelayError::Validation(
                "max_retry_delay_ms must be >= retry_delay_ms".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutboxRelayTick {
    pub claimed: usize,
    pub delivered: usize,
    pub requeued: usize,
    pub dead_lettered: usize,
    pub lost_claims: usize,
}

pub struct OutboxRelay {
    store: Arc<dyn OutboxStore>,
    handler: Arc<dyn OutboxRelayHandler>,
    config: OutboxRelayConfig,
}

impl OutboxRelay {
    pub fn new(
        store: Arc<dyn OutboxStore>,
        handler: Arc<dyn OutboxRelayHandler>,
        config: OutboxRelayConfig,
    ) -> Result<Self, OutboxRelayError> {
        config.validate()?;
        Ok(Self {
            store,
            handler,
            config,
        })
    }

    pub async fn tick(&self) -> Result<OutboxRelayTick, OutboxRelayError> {
        let now = now_ms();
        let claimed = self
            .store
            .claim_outbox(
                &self.config.lane,
                &self.config.target,
                self.config.batch_limit,
                self.config.lease_ms,
                &self.config.consumer_id,
                now,
            )
            .await?;
        let mut stats = OutboxRelayTick {
            claimed: claimed.len(),
            ..OutboxRelayTick::default()
        };

        for message in claimed {
            let Some(claim_token) = message.claim_token.as_deref() else {
                stats.lost_claims += 1;
                continue;
            };
            match self.handler.deliver(&message).await {
                Ok(()) => {
                    if self
                        .store
                        .ack_outbox(&message.outbox_id, claim_token, now_ms())
                        .await?
                    {
                        stats.delivered += 1;
                    } else {
                        stats.lost_claims += 1;
                    }
                }
                Err(error) => {
                    let retry_at = retry_at_ms(
                        now_ms(),
                        message.attempt_count,
                        self.config.retry_delay_ms,
                        self.config.max_retry_delay_ms,
                    );
                    match self
                        .store
                        .nack_outbox(
                            &message.outbox_id,
                            claim_token,
                            &error.to_string(),
                            retry_at,
                            now_ms(),
                        )
                        .await?
                    {
                        OutboxNackOutcome::Requeued => stats.requeued += 1,
                        OutboxNackOutcome::DeadLettered => stats.dead_lettered += 1,
                        OutboxNackOutcome::LostClaim => stats.lost_claims += 1,
                    }
                }
            }
        }
        Ok(stats)
    }
}

fn retry_at_ms(now: u64, attempt_count: u32, base_delay_ms: u64, max_delay_ms: u64) -> u64 {
    let exponent = attempt_count.saturating_sub(1).min(31);
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let delay = base_delay_ms.saturating_mul(multiplier).min(max_delay_ms);
    now.saturating_add(delay)
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
    use awaken_server_contract::contract::outbox::{OutboxMessageDraft, OutboxStatus, OutboxStore};
    use awaken_stores::InMemoryOutboxStore;
    use parking_lot::Mutex;
    use serde_json::json;

    #[derive(Default)]
    struct RecordingHandler {
        delivered: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl OutboxRelayHandler for RecordingHandler {
        async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError> {
            self.delivered.lock().push(message.outbox_id.clone());
            Ok(())
        }
    }

    struct FailingHandler;

    #[async_trait]
    impl OutboxRelayHandler for FailingHandler {
        async fn deliver(&self, _message: &OutboxMessage) -> Result<(), OutboxRelayError> {
            Err(OutboxRelayError::Delivery("publish failed".to_string()))
        }
    }

    struct DelayedAckHandler {
        delivered: Arc<Mutex<Vec<String>>>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl OutboxRelayHandler for DelayedAckHandler {
        async fn deliver(&self, message: &OutboxMessage) -> Result<(), OutboxRelayError> {
            self.delivered.lock().push(message.outbox_id.clone());
            self.started.notify_waiters();
            self.release.notified().await;
            Ok(())
        }
    }

    fn config() -> OutboxRelayConfig {
        OutboxRelayConfig {
            lane: "canonical".to_string(),
            target: "projector".to_string(),
            consumer_id: "relay-a".to_string(),
            batch_limit: 10,
            lease_ms: 1_000,
            retry_delay_ms: 0,
            max_retry_delay_ms: 0,
        }
    }

    fn draft(value: i64) -> OutboxMessageDraft {
        OutboxMessageDraft::new("canonical", "projector", json!({ "value": value })).unwrap()
    }

    #[tokio::test]
    async fn tick_delivers_and_acks_claimed_messages() {
        let store = Arc::new(InMemoryOutboxStore::new());
        store.enqueue_outbox(draft(1)).await.unwrap();
        store.enqueue_outbox(draft(2)).await.unwrap();
        let handler = Arc::new(RecordingHandler::default());
        let relay = OutboxRelay::new(store.clone(), handler.clone(), config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.claimed, 2);
        assert_eq!(stats.delivered, 2);
        assert_eq!(handler.delivered.lock().len(), 2);
        let delivered = store
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 2);
    }

    #[tokio::test]
    async fn tick_nacks_failures_and_dead_letters_after_max_attempts() {
        let store = Arc::new(InMemoryOutboxStore::new());
        let mut message = draft(1);
        message.max_attempts = 2;
        store.enqueue_outbox(message).await.unwrap();
        let relay = OutboxRelay::new(store.clone(), Arc::new(FailingHandler), config()).unwrap();

        let first = relay.tick().await.unwrap();
        assert_eq!(first.requeued, 1);
        let second = relay.tick().await.unwrap();
        assert_eq!(second.dead_lettered, 1);

        let dead = store
            .list_outbox(Some(OutboxStatus::DeadLetter), 10)
            .await
            .unwrap();
        assert_eq!(dead.len(), 1);
        assert!(
            dead[0]
                .last_error
                .as_deref()
                .unwrap()
                .contains("publish failed")
        );
    }

    #[tokio::test]
    async fn tick_redelivers_after_crashed_relay_loses_ack() {
        let store = Arc::new(InMemoryOutboxStore::new());
        let message = store.enqueue_outbox(draft(1)).await.unwrap().message;
        let crashed_claim = store
            .claim_outbox("canonical", "projector", 1, 100, "crashed-relay", 1_000)
            .await
            .unwrap();
        assert_eq!(crashed_claim.len(), 1);
        assert_eq!(crashed_claim[0].outbox_id, message.outbox_id);

        // Simulate a process crash after publish but before ack/nack. A fresh
        // relay must redeliver once the old lease is expired.
        let handler = Arc::new(RecordingHandler::default());
        let relay = OutboxRelay::new(store.clone(), handler.clone(), config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.claimed, 1);
        assert_eq!(stats.delivered, 1);
        assert_eq!(handler.delivered.lock().as_slice(), &[message.outbox_id]);
        let delivered = store
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
    }

    #[tokio::test]
    async fn concurrent_relay_redelivery_rejects_late_ack_after_network_jitter() {
        let store = Arc::new(InMemoryOutboxStore::new());
        let message = store.enqueue_outbox(draft(1)).await.unwrap().message;
        let delayed_deliveries = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut short_lease = config();
        short_lease.consumer_id = "relay-a".to_string();
        short_lease.lease_ms = 10;
        let relay_a = Arc::new(
            OutboxRelay::new(
                store.clone(),
                Arc::new(DelayedAckHandler {
                    delivered: delayed_deliveries.clone(),
                    started: started.clone(),
                    release: release.clone(),
                }),
                short_lease,
            )
            .unwrap(),
        );

        let tick_a = {
            let relay = relay_a.clone();
            tokio::spawn(async move { relay.tick().await })
        };
        started.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let mut relay_b_config = config();
        relay_b_config.consumer_id = "relay-b".to_string();
        let relay_b_handler = Arc::new(RecordingHandler::default());
        let relay_b = OutboxRelay::new(store.clone(), relay_b_handler.clone(), relay_b_config)
            .expect("relay b");
        let stats_b = relay_b.tick().await.expect("relay b tick");
        assert_eq!(stats_b.delivered, 1);
        assert_eq!(
            relay_b_handler.delivered.lock().as_slice(),
            std::slice::from_ref(&message.outbox_id)
        );

        release.notify_waiters();
        let stats_a = tick_a.await.expect("relay a task").expect("relay a tick");
        assert_eq!(stats_a.lost_claims, 1);
        assert_eq!(
            delayed_deliveries.lock().as_slice(),
            std::slice::from_ref(&message.outbox_id)
        );

        let delivered = store
            .list_outbox(Some(OutboxStatus::Delivered), 10)
            .await
            .unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].claimed_by, None);
        assert_eq!(delivered[0].claim_token, None);
    }

    #[tokio::test]
    async fn tick_respects_configured_lane_and_target() {
        let store = Arc::new(InMemoryOutboxStore::new());
        store
            .enqueue_outbox(
                OutboxMessageDraft::new("other", "projector", json!({ "value": 1 })).unwrap(),
            )
            .await
            .unwrap();
        let handler = Arc::new(RecordingHandler::default());
        let relay = OutboxRelay::new(store, handler.clone(), config()).unwrap();

        let stats = relay.tick().await.unwrap();

        assert_eq!(stats.claimed, 0);
        assert!(handler.delivered.lock().is_empty());
    }

    #[test]
    fn relay_config_rejects_invalid_claim_parameters() {
        let mut invalid = config();
        invalid.batch_limit = 0;
        let err = invalid.validate().unwrap_err();
        assert!(
            matches!(err, OutboxRelayError::Validation(message) if message.contains("batch_limit"))
        );
    }

    #[test]
    fn retry_delay_backs_off_and_caps() {
        assert_eq!(retry_at_ms(10, 1, 5, 20), 15);
        assert_eq!(retry_at_ms(10, 2, 5, 20), 20);
        assert_eq!(retry_at_ms(10, 4, 5, 20), 30);
    }
}
