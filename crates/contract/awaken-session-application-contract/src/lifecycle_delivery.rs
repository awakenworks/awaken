//! Lifecycle delivery ports over the durable Session outbox.

use awaken_session_contract::ManagedLifecycleFact;

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
