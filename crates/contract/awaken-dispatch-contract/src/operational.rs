//! Durable operational facts emitted by the dispatch aggregate.
//!
//! This feed reports delivery authority changes, not a Run's agent outcome.
//! Consumers that need committed Run truth use `RunLifecycleFeed`; correlating
//! the two feeds is explicit and never implies a cross-aggregate transaction.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use serde::{Deserialize, Serialize};

use crate::dispatch::{DispatchError, DispatchOutcome, RunClaim};

/// Exclusive cursor in one dispatch-store operational partition.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct DispatchCursor(pub u64);

/// Why a previously issued lease ceased to authorize its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseLossReason {
    Expired,
    Cancelled,
    RetryExhausted,
}

/// One valid dispatch-authority transition.
///
/// The payload is an enum rather than a kind plus optional fields, so consumers
/// cannot observe a `settled` event without its outcome or a `reclaimed` event
/// without both the superseded and replacement claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DispatchOperation {
    Claimed {
        claim: RunClaim,
    },
    LeaseLost {
        claim: RunClaim,
        reason: LeaseLossReason,
    },
    Reclaimed {
        previous: RunClaim,
        claim: RunClaim,
    },
    Settled {
        claim: RunClaim,
        outcome: DispatchOutcome,
    },
    DeadLettered {
        claim: RunClaim,
        attempt_count: u64,
    },
}

impl DispatchOperation {
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        match self {
            Self::Claimed { claim }
            | Self::LeaseLost { claim, .. }
            | Self::Reclaimed { claim, .. }
            | Self::Settled { claim, .. }
            | Self::DeadLettered { claim, .. } => &claim.run_id,
        }
    }
}

/// One durable dispatch operation in store-assigned order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchOperationalEvent {
    pub cursor: DispatchCursor,
    /// Store-assigned wall-clock time of the atomic authority mutation.
    ///
    /// `None` is retained only for wire/schema compatibility with historical
    /// rows. Consumers that calculate elapsed time must not invent it.
    #[serde(default)]
    pub recorded_at_ms: Option<u64>,
    pub operation: DispatchOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchPage {
    pub events: Vec<DispatchOperationalEvent>,
    /// Last returned cursor, or the requested cursor when the page is empty.
    pub next_cursor: DispatchCursor,
}

/// Durable, replayable delivery-authority feed.
#[async_trait]
pub trait DispatchOperationalFeed: Send + Sync {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> Result<DispatchPage, DispatchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_round_trip_preserves_the_valid_product_type() {
        let previous = RunClaim {
            run_id: RunId("run".into()),
            owner: "worker-a".into(),
            epoch: 1,
        };
        let operation = DispatchOperation::Reclaimed {
            previous,
            claim: RunClaim {
                run_id: RunId("run".into()),
                owner: "worker-b".into(),
                epoch: 2,
            },
        };

        let encoded = serde_json::to_string(&operation).expect("serialize");
        let decoded: DispatchOperation = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(decoded, operation);
        assert_eq!(decoded.run_id(), &RunId("run".into()));
    }

    #[test]
    fn historical_event_without_store_time_remains_readable_but_explicitly_unknown() {
        // Cause-effect decision table:
        // R1 new payload + timestamp => Some(timestamp) (covered by store
        // conformance); R2 historical payload without timestamp => None. R2
        // must not fabricate elapsed time while preserving rolling upgrades.
        let decoded: DispatchOperationalEvent = serde_json::from_value(serde_json::json!({
            "cursor": 7,
            "operation": {
                "type": "claimed",
                "claim": { "run_id": "run", "owner": "worker", "epoch": 1 }
            }
        }))
        .expect("historical event remains readable");

        assert_eq!(decoded.recorded_at_ms, None);
    }
}
