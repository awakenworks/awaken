//! Managed lifecycle facts and the Session outbox notification port (ADR-0048).
//! The generic fact is shared by Session, Deployment, and DeploymentRun aggregates;
//! The notifier carries no fact payload: the transactional outbox is the only
//! source of event data. Delivery machinery lives outside this contract.

/// Durable start marker retained inside the Session aggregate while one
/// customer-visible Running interval is open. Overlapping driving events share
/// this marker; only the transition back out of Running closes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRuntimeIntervalStart {
    pub interval_id: String,
    pub activity_epoch: u64,
    pub started_at_unix_ms: u64,
    /// Prospective root revision committed by the mutation that opened this
    /// interval. The Session root and retained Event batches share this ordering
    /// fence; process-local observation order is never a replay coordinate.
    #[serde(default)]
    pub opened_revision: crate::SessionRevision,
    /// Exact Runtime boundaries observed while this aggregate interval remains
    /// open. Multiple overlapping activities and a child-to-primary handoff share
    /// one interval, so one Run id or one terminal cursor is not sufficient.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<SessionRuntimeIntervalObservation>,
}

/// One exact committed Runtime boundary observed by an admitted Session
/// activity. It is retained only as ordering/projection provenance: Run state,
/// transcript, and awaiting truth remain owned by Runtime's committed facts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRuntimeIntervalObservation {
    pub activity_epoch: u64,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor,
    pub source_commit_cursor: u64,
}

/// Exact, secret-free Session Running interval emitted through the existing
/// lifecycle outbox. Infrastructure-only restore, checkpoint, queue and drain
/// work never create this value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRuntimeInterval {
    pub interval_id: String,
    pub activity_epoch: u64,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    #[serde(default)]
    pub opened_revision: crate::SessionRevision,
    #[serde(default)]
    pub closed_revision: crate::SessionRevision,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<SessionRuntimeIntervalObservation>,
    /// Exact cumulative neutral usage at the closing boundary. This is retained
    /// even when the Session has no budget; `active_seconds` is corrected from
    /// the interval clock in the same root CAS that closes the interval.
    #[serde(default)]
    pub usage: crate::ManagedBudgetUsageCursor,
    /// Public budget value paired with this historical usage event. A later cap
    /// raise/removal must not rewrite an older event payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_list_cost_minor: Option<u64>,
}

/// A secret-free lifecycle fact committed beside a Managed aggregate. Its
/// stable id is both the durable outbox key and the receiver idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedLifecycleFact {
    pub id: String,
    #[serde(alias = "session_id")]
    pub object_id: String,
    pub workspace_id: Option<String>,
    pub event_type: String,
    pub timestamp: i64,
    /// Typed optional payload on the one shared transactional outbox. Keeping
    /// this here avoids a Billing-specific Session event store or dual write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_interval: Option<SessionRuntimeInterval>,
}

/// Cross-context delivery port for one durable lifecycle fact. Coordinator owns
/// the outbox and calls this port; Control implements it over authored webhook
/// subscriptions. The contract carries no transport or retry mechanism.
#[async_trait::async_trait]
pub trait LifecycleFactDelivery: Send + Sync {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String>;
}

/// One ordered fan-out over the existing lifecycle delivery port. Every
/// receiver is attempted on every call; replay safety remains each receiver's
/// responsibility under the fact's stable id.
pub struct CompositeLifecycleFactDelivery {
    deliveries: Vec<std::sync::Arc<dyn LifecycleFactDelivery>>,
}

impl CompositeLifecycleFactDelivery {
    #[must_use]
    pub fn new(deliveries: Vec<std::sync::Arc<dyn LifecycleFactDelivery>>) -> Self {
        Self { deliveries }
    }
}

#[async_trait::async_trait]
impl LifecycleFactDelivery for CompositeLifecycleFactDelivery {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
        let mut failures = Vec::new();
        for (index, delivery) in self.deliveries.iter().enumerate() {
            if let Err(error) = delivery.deliver(fact).await {
                failures.push(format!("receiver {index}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

/// Best-effort low-latency hint that Coordinator's durable lifecycle outbox may
/// have advanced. Implementations must read facts from the repository; losing a
/// notification is safe because startup and periodic replay remain authoritative.
pub trait LifecycleFactNotifier: Send + Sync {
    fn notify(&self);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct RecordingDelivery {
        calls: Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl LifecycleFactDelivery for RecordingDelivery {
        async fn deliver(&self, _fact: &ManagedLifecycleFact) -> Result<(), String> {
            self.calls.lock().unwrap().push(self.name);
            if self.fail {
                Err(format!("{} unavailable", self.name))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn composite_attempts_every_receiver_and_retries_on_any_failure() {
        // Cause/effect decision table: C1=receiver A succeeds/fails; C2=receiver
        // B succeeds/fails. R1 both succeed => ordered attempts and success;
        // R2/R3/R4 any failure => every receiver is still attempted and the
        // outbox receives an error, so its stable fact is retried. Receiver
        // idempotency makes already-successful replay safe.
        let fact = ManagedLifecycleFact {
            id: "fact-a".into(),
            object_id: "session-a".into(),
            workspace_id: Some("workspace-a".into()),
            event_type: "session.runtime_interval_closed".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        for (billing_fails, webhook_fails, succeeds) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
            (true, true, false),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let composite = CompositeLifecycleFactDelivery::new(vec![
                Arc::new(RecordingDelivery {
                    calls: calls.clone(),
                    name: "billing",
                    fail: billing_fails,
                }),
                Arc::new(RecordingDelivery {
                    calls: calls.clone(),
                    name: "webhook",
                    fail: webhook_fails,
                }),
            ]);
            assert_eq!(composite.deliver(&fact).await.is_ok(), succeeds);
            assert_eq!(*calls.lock().unwrap(), ["billing", "webhook"]);
        }
    }
}
