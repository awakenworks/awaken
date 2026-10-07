// @formal entrypoint webhook.outbox_fact_identity_is_stable
//! Managed lifecycle facts retained and delivered through the Session outbox (ADR-0048).
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
    pub opened_revision: awaken_session_types::SessionRevision,
    /// The root Thread's Runtime commit cursor when this interval was
    /// admitted. Every lifecycle commit the interval owns follows it, and one
    /// at or before it predates the interval, so a reader places the
    /// interval's span from this durable coordinate rather than from how much
    /// lifecycle it has seen. `None` for an interval recorded before this
    /// coordinate was kept, which the legacy selection places.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_after_commit_cursor: Option<u64>,
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
    pub opened_revision: awaken_session_types::SessionRevision,
    #[serde(default)]
    pub closed_revision: awaken_session_types::SessionRevision,
    /// See [`SessionRuntimeIntervalStart::admitted_after_commit_cursor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_after_commit_cursor: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<SessionRuntimeIntervalObservation>,
    /// Exact cumulative neutral usage at the closing boundary. This is retained
    /// even when the Session has no budget; `active_seconds` is corrected from
    /// the interval clock in the same root CAS that closes the interval.
    #[serde(default)]
    pub usage: awaken_session_budget_contract::ManagedBudgetUsageCursor,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_lifecycle_payloads_preserve_identity_and_default_interval_provenance() {
        // Compatibility table: L1=old session_id alias without an interval;
        // L2=interval recorded before revision, observation and usage fields;
        // L3=current payload after decoding L2. Canonical encoding preserves the
        // stable outbox key, defaults old provenance, and omits absent options.
        let legacy = r#"{"id":"fact-a","session_id":"session-a","workspace_id":null,"event_type":"session.started","timestamp":7}"#;
        let fact: ManagedLifecycleFact = serde_json::from_str(legacy).unwrap();
        assert_eq!(fact.object_id, "session-a", "L1");
        assert_eq!(fact.runtime_interval, None, "L1");
        assert_eq!(
            serde_json::to_string(&fact).unwrap(),
            r#"{"id":"fact-a","object_id":"session-a","workspace_id":null,"event_type":"session.started","timestamp":7}"#,
            "L1 canonical bytes"
        );

        let legacy_interval = r#"{"interval_id":"interval-a","activity_epoch":2,"started_at_unix_ms":3,"ended_at_unix_ms":4}"#;
        let interval: SessionRuntimeInterval = serde_json::from_str(legacy_interval).unwrap();
        assert_eq!(interval.opened_revision, Default::default(), "L2");
        assert_eq!(interval.closed_revision, Default::default(), "L2");
        assert!(interval.observations.is_empty(), "L2");
        assert_eq!(interval.admitted_after_commit_cursor, None, "L2");
        assert_eq!(interval.usage, Default::default(), "L2");
        assert_eq!(interval.max_list_cost_minor, None, "L2");
        assert_eq!(
            serde_json::to_string(&interval).unwrap(),
            r#"{"interval_id":"interval-a","activity_epoch":2,"started_at_unix_ms":3,"ended_at_unix_ms":4,"opened_revision":0,"closed_revision":0,"usage":{"by_model":{},"active_seconds":0,"web_fetch_requests":0,"web_search_requests":0}}"#,
            "L3 canonical bytes"
        );
    }
}
