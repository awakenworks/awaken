// @formal surface exclude=observability_adapter
// @formal owner crates/server/awaken-session-application/src/sandbox_observation.rs
// @formal reason: A diagnostic projection after the original CAS; it cannot persist,
// @formal reason: authorize an effect, change a clock ledger, or decide lifecycle state.
//! Lossy, structure-only capacity observations at the existing root CAS.
//! No observer state, lifecycle decision or extra repository read lives here.

use awaken_session_contract::{
    PersistedSession, SessionEnvironmentPhase, SessionEnvironmentState, SessionMutationPayload,
    SessionMutationResult,
};
use awaken_session_types::SessionRevision;

const TARGET: &str = "awaken::sandbox_observation";

#[derive(Debug, serde::Serialize)]
pub(crate) struct Observation {
    schema: &'static str,
    session_id: String,
    revision: u64,
    // A wire projection of the existing root counter, including its legal
    // zero. A nonzero lease Epoch would change that meaning; this JSON scalar
    // grants no fencing, allocation or advancement operation.
    activity_epoch: serde_json::Number,
    observed_at_unix_ms: u64,
    phase: &'static str,
    execution: &'static str,
    terminal: bool,
    created_at_unix_ms: Option<u64>,
    idle_since_unix_ms: Option<u64>,
    operation_started_at_unix_ms: Option<u64>,
    operation_fingerprint: Option<String>,
    environment_fingerprint: Option<String>,
    declared_packages_fingerprint: Option<String>,
}

impl Observation {
    fn from_session(session: &PersistedSession, observed_at_unix_ms: u64) -> Self {
        let phase = match session.environment.step().phase() {
            SessionEnvironmentPhase::Unmaterialized => "unmaterialized",
            SessionEnvironmentPhase::Resident => "resident",
            SessionEnvironmentPhase::Suspending => "suspending",
            SessionEnvironmentPhase::Hibernated => "hibernated",
            SessionEnvironmentPhase::Restoring => "restoring",
        };
        let operation_started_at_unix_ms = match &session.environment {
            SessionEnvironmentState::Suspending { operation, .. }
            | SessionEnvironmentState::Restoring { operation, .. } => {
                Some(operation.started_at_unix_ms()).filter(|value| *value > 0)
            }
            _ => None,
        };
        let environment = session
            .frozen_baseline()
            .map(|baseline| &baseline.environment);
        Self {
            schema: "awaken.sandbox.observation/v1",
            session_id: session.session_id.as_str().to_owned(),
            revision: session.revision.0,
            activity_epoch: session.activity_epoch.into(),
            observed_at_unix_ms: session.updated_at_unix_ms.unwrap_or(observed_at_unix_ms),
            phase,
            execution: session.execution.as_str(),
            terminal: session.is_terminal(),
            created_at_unix_ms: session.created_at_unix_ms,
            idle_since_unix_ms: session.environment.idle_since_unix_ms(),
            operation_started_at_unix_ms,
            operation_fingerprint: session
                .environment
                .effect_id()
                .map(|effect| awaken_agent_contract::stable_fingerprint(&effect)),
            // Hash even the nominal fingerprint: historical fixtures or imports
            // may contain arbitrary strings. No raw configuration is exported.
            environment_fingerprint: environment.map(|environment| {
                awaken_agent_contract::stable_fingerprint(&environment.config_fingerprint)
            }),
            declared_packages_fingerprint: environment.map(|environment| {
                awaken_agent_contract::stable_fingerprint(&environment.packages)
            }),
        }
    }

    fn from_payload(payload: &SessionMutationPayload, now: u64) -> Option<Self> {
        match payload {
            SessionMutationPayload::Replace(session) => Some(Self::from_session(session, now)),
            SessionMutationPayload::Delete(tombstone) => Some(Self {
                schema: "awaken.sandbox.observation/v1",
                session_id: tombstone.session_id.as_str().to_owned(),
                revision: 0,
                activity_epoch: 0.into(),
                observed_at_unix_ms: now,
                phase: "deleted",
                execution: "terminated",
                terminal: true,
                created_at_unix_ms: None,
                idle_since_unix_ms: None,
                operation_started_at_unix_ms: None,
                operation_fingerprint: None,
                environment_fingerprint: None,
                declared_packages_fingerprint: None,
            }),
            SessionMutationPayload::EventNoOp { .. } => None,
        }
    }

    fn applied(mut self, result: Option<&SessionMutationResult>) -> Option<Self> {
        let Some(SessionMutationResult::Applied { new_revision }) = result else {
            return None;
        };
        self.revision = new_revision.0;
        Some(self)
    }

    fn emit(self) {
        if let Ok(observation) = serde_json::to_string(&self) {
            tracing::debug!(target: TARGET, observation = %observation,
                "committed Sandbox capacity observation");
        }
    }
}

/// Compile only when the operator enables this existing diagnostic target.
/// Capture the compiled mutation, not an independently reloaded root.
pub(crate) fn prepare(payload: &SessionMutationPayload, now: u64) -> Option<Observation> {
    tracing::enabled!(target: TARGET, tracing::Level::DEBUG)
        .then(|| Observation::from_payload(payload, now))
        .flatten()
}

pub(crate) fn committed(candidate: Option<Observation>, result: Option<&SessionMutationResult>) {
    if let Some(observation) = candidate.and_then(|candidate| candidate.applied(result)) {
        observation.emit();
    }
}

pub(crate) fn created(session: &PersistedSession, revision: SessionRevision, now: u64) {
    if tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        let mut observation = Observation::from_session(session, now);
        observation.revision = revision.0;
        observation.emit();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use awaken_sync::LeafLock;
    use std::sync::Arc;

    pub(crate) struct Capture(pub(crate) Arc<LeafLock<Vec<serde_json::Value>>>);

    impl tracing::Subscriber for Capture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == TARGET
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(Option<serde_json::Value>);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "observation" {
                        self.0 = Some(serde_json::from_str(&format!("{value:?}")).unwrap());
                    }
                }
            }
            let mut visitor = Visitor(None);
            event.record(&mut visitor);
            if let Some(value) = visitor.0 {
                self.0.lock().push(value);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn existing_tracing_path_exports_only_applied_structure_when_enabled() {
        // E1 disabled diagnostic target -> no prepared payload. E2 enabled +
        // Applied -> one JSON field the capacity collector can decode. E3 an
        // exact replay -> no second event. E4 Create shares the same schema.
        let session = crate::tests::persisted("capacity-wire-test", false, "idle");
        let payload = SessionMutationPayload::Replace(session.clone());
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            assert!(prepare(&payload, 100).is_none(), "E1");
        });
        let events = Arc::new(LeafLock::new(Vec::new()));
        tracing::subscriber::with_default(Capture(events.clone()), || {
            committed(
                prepare(&payload, 100),
                Some(&SessionMutationResult::Applied {
                    new_revision: SessionRevision(9),
                }),
            );
            committed(
                prepare(&payload, 100),
                Some(&SessionMutationResult::Replayed {
                    new_revision: SessionRevision(9),
                }),
            );
            created(&session, SessionRevision(1), 100);
        });
        let events = events.lock();
        assert_eq!(events.len(), 2, "E2+E3");
        assert_eq!(events[0]["revision"], 9);
        assert_eq!(events[0]["schema"], "awaken.sandbox.observation/v1", "E4");
        assert_eq!(events[1]["schema"], events[0]["schema"], "E4");
    }

    #[test]
    fn only_a_fresh_applied_receipt_emits_and_uses_its_revision() {
        // Cause/effect rules: C1 Applied/Replayed/Conflict/mismatch/no-op/error;
        // C2 a stale candidate revision. R1 Applied -> one observation stamped
        // with the receipt revision; R2 every other result -> no observation.
        // Repository failure is None and never manufactures a successful fact.
        let session = crate::tests::persisted("capacity-test", false, "idle");
        for (result, expected) in [
            (
                Some(SessionMutationResult::Applied {
                    new_revision: SessionRevision(9),
                }),
                Some(9),
            ),
            (
                Some(SessionMutationResult::Replayed {
                    new_revision: SessionRevision(9),
                }),
                None,
            ),
            (
                Some(SessionMutationResult::Conflict {
                    current_revision: SessionRevision(9),
                }),
                None,
            ),
            (Some(SessionMutationResult::IdempotencyMismatch), None),
            (
                Some(SessionMutationResult::AcknowledgedNoOp {
                    observed_revision: SessionRevision(9),
                }),
                None,
            ),
            (None, None),
        ] {
            assert_eq!(
                Observation::from_session(&session, 100)
                    .applied(result.as_ref())
                    .map(|observation| observation.revision),
                expected
            );
        }
    }

    #[test]
    fn projection_retains_unknown_clocks_and_excludes_payloads() {
        // C1 historical clocks absent; C2 metadata/title/binding contain
        // sensitive text; C3 a Resident idle edge exists. R1 C1 -> null, never
        // now; R2 C2 -> none of those payloads exported; R3 C3 -> exact edge.
        let mut session = crate::tests::persisted("capacity-test", false, "idle");
        session.title = Some("PRIVATE-TITLE".into());
        session
            .metadata
            .insert("PRIVATE-METADATA".into(), "SECRET".into());
        session.created_at_unix_ms = None;
        session.environment = serde_json::from_value(serde_json::json!({
            "phase": "resident", "binding": "PRIVATE-BINDING", "idle_since_unix_ms": 80
        }))
        .unwrap();
        let observation = Observation::from_session(&session, 100);
        assert_eq!(observation.created_at_unix_ms, None);
        assert_eq!(observation.idle_since_unix_ms, Some(80));
        assert_eq!(observation.phase, "resident");
        let encoded = serde_json::to_string(&observation).unwrap();
        for private in [
            "PRIVATE-TITLE",
            "PRIVATE-METADATA",
            "PRIVATE-BINDING",
            "SECRET",
        ] {
            assert!(!encoded.contains(private));
        }
        // C4 the existing root permits zero and the full u64 range. Its JSON
        // projection is lossless; it must not narrow to a nonzero lease epoch.
        for activity in [0, u64::MAX] {
            session.activity_epoch = activity;
            let value = serde_json::to_value(Observation::from_session(&session, 100)).unwrap();
            assert_eq!(value["activity_epoch"], serde_json::json!(activity), "C4");
        }
    }
}
