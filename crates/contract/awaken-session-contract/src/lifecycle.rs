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

/// Best-effort low-latency hint that Coordinator's durable lifecycle outbox may
/// have advanced. Implementations must read facts from the repository; losing a
/// notification is safe because startup and periodic replay remain authoritative.
pub trait LifecycleFactNotifier: Send + Sync {
    fn notify(&self);
}
