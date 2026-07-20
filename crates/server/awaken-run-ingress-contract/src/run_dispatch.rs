//! One Run accepted into durable dispatch.
//!
//! [`RunDispatch`] is exactly the data a durable queue persists and replays. It
//! carries no live handles (G3/G4), so any worker can rebuild the Run after a
//! crash. Live worker dependencies stay in the `awaken-run-ingress` host.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::activation::RunActivation;
pub use awaken_tenancy::ExecutionScopeRef;
pub use awaken_worker_contract::PlacementRequirements;
use serde::{Deserialize, Serialize};

/// The durable, serializable record of an accepted run. It holds no `Arc<dyn ...>`,
/// registry, or live handle (G3); the runtime builds live execution objects from
/// the activation's pinned snapshot on each attempt (G4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDispatch {
    pub activation: RunActivation,
    /// Session whose runtime capabilities and commit/history boundary must drive
    /// this Run after recovery. Ordinary Runs omit it and route by their own
    /// thread. A child Run names its parent's session while retaining its own
    /// activation thread and first-class lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_thread_id: Option<ThreadId>,
    /// W3C `traceparent` captured when the run was admitted, so a durably-dispatched
    /// execution continues the admitting request's distributed trace across the
    /// queue boundary. Absent when admitted without an active trace (or by an older
    /// writer): a pre-existing queue row simply deserializes it as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Authorized execution ownership resolved at the ingress edge. The dispatch
    /// aggregate treats it as an opaque coordinate and never derives it from a
    /// thread id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_scope: Option<ExecutionScopeRef>,
    /// Hard worker requirements pinned at admission. Older durable rows omit this
    /// field and deserialize through the contract's explicit legacy posture;
    /// strict remote callers attach `PlacementRequirements::remote_required()`.
    #[serde(
        default,
        skip_serializing_if = "PlacementRequirements::is_legacy_default"
    )]
    pub placement: PlacementRequirements,
}

impl RunDispatch {
    pub fn new(activation: RunActivation) -> Self {
        Self {
            activation,
            session_thread_id: None,
            traceparent: None,
            execution_scope: None,
            placement: PlacementRequirements::default(),
        }
    }

    /// Route execution through an existing session without changing the Run's
    /// own thread identity used by lifecycle and queue single-writer rules.
    pub fn for_session(mut self, thread_id: ThreadId) -> Self {
        self.session_thread_id = Some(thread_id);
        self
    }

    /// Attach the admitting request's W3C `traceparent` (see the field docs).
    pub fn with_traceparent(mut self, traceparent: Option<String>) -> Self {
        self.traceparent = traceparent;
        self
    }

    /// Attach the verified scope's durable opaque representation.
    #[must_use]
    pub fn with_execution_scope(mut self, scope: ExecutionScopeRef) -> Self {
        self.execution_scope = Some(scope);
        self
    }

    /// Pin the immutable worker-placement requirements carried through every
    /// crash recovery and replacement attempt.
    #[must_use]
    pub fn with_placement(mut self, placement: PlacementRequirements) -> Self {
        self.placement = placement;
        self
    }

    pub fn run_id(&self) -> &RunId {
        &self.activation.run_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.activation.thread_id
    }

    pub fn session_thread_id(&self) -> &ThreadId {
        self.session_thread_id
            .as_ref()
            .unwrap_or(&self.activation.thread_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn activation() -> RunActivation {
        RunActivation::new(
            RunId("run-1".into()),
            ThreadId("thrd-1".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snap".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: "be helpful".into(),
                    max_steps: 8,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("prov", "model", "acp:test"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        )
    }

    /// The request a durable queue persists and replays must survive a
    /// serialize→deserialize round-trip unchanged — G3's whole point (it carries no
    /// live handle), and the accessors read the same ids back out.
    #[test]
    fn round_trips_through_serde_with_its_accessors_intact() {
        let req = RunDispatch::new(activation()).with_traceparent(Some("00-abc-01".into()));
        let json = serde_json::to_string(&req).expect("serializes");
        let back: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, req, "round-trip is lossless");
        assert_eq!(back.run_id(), req.run_id());
        assert_eq!(back.thread_id(), req.thread_id());
        assert_eq!(back.traceparent.as_deref(), Some("00-abc-01"));
    }

    /// A `None` traceparent is omitted on the wire (`skip_serializing_if`), so a row
    /// written by an older writer (no trace) is byte-identical and deserializes back
    /// to `None` rather than dead-lettering — the documented forward/back-compat
    /// guarantee.
    #[test]
    fn a_none_traceparent_is_omitted_and_a_legacy_row_loads_as_none() {
        let req = RunDispatch::new(activation());
        assert!(req.traceparent.is_none());
        let json = serde_json::to_string(&req).expect("serializes");
        assert!(
            !json.contains("traceparent"),
            "a None traceparent is not written: {json}"
        );
        // The same row (no traceparent key) is exactly what a pre-field writer
        // produced; it must load as None.
        let back: RunDispatch = serde_json::from_str(&json).expect("legacy row loads");
        assert!(back.traceparent.is_none());
        assert!(back.execution_scope.is_none());
        assert!(back.placement.is_legacy_default());
    }

    /// A durable queue row written by an OLDER peer — no `traceparent`, no
    /// `model_ref_override`. Frozen as bytes on purpose: unlike a self-round-trip
    /// (where both sides move together), this pins the exact nested wire shape, so a
    /// rename or retype anywhere down the tree (`model_binding`, `context_policy`'s
    /// `kind`, a content block's `type`) breaks this test instead of silently
    /// stranding every in-flight row a running deployment already persisted. Editing
    /// it means acknowledging a queue-format break.
    const LEGACY_QUEUE_ROW: &str = r#"{
      "activation": {
        "run_id": "run-1",
        "thread_id": "thrd-1",
        "snapshot": {
          "id": "snap",
          "root_agent_id": "agent",
          "resolved_spec": {
            "catalog_fingerprint": "fp",
            "instructions": "be helpful",
            "max_steps": 8,
            "model_binding": {
              "provider_identity_ref": "prov",
              "model_ref": "model",
              "backend_ref": "acp:test"
            },
            "model_candidates": [],
            "tool_descriptors": [],
            "plugin_ids": [],
            "plugin_config": {},
            "context_policy": { "kind": "keep_all" },
            "tool_presentation": {}
          },
          "fingerprint": "fp"
        },
        "input": [
          { "id": "u1", "role": "User", "content": [ { "type": "text", "text": "go" } ] }
        ]
      }
    }"#;

    #[test]
    fn a_frozen_legacy_queue_row_still_deserializes_intact() {
        let back: RunDispatch =
            serde_json::from_str(LEGACY_QUEUE_ROW).expect("a persisted legacy row must still load");
        // The envelope's own newer field defaults.
        assert!(back.traceparent.is_none());
        assert!(back.execution_scope.is_none());
        assert!(back.placement.is_legacy_default());
        // The activation's newer field defaults, and the run resolves to its pinned
        // binding — exactly how a run admitted before per-turn overrides behaves.
        assert!(back.activation.model_ref_override.is_none());
        assert_eq!(back.activation.effective_model_ref(), "model");
        assert_eq!(back.run_id().0, "run-1");
        assert_eq!(back.thread_id().0, "thrd-1");
    }

    /// The golden bytes are not stale: today's writer, with both newer fields at their
    /// defaults, still emits exactly the frozen legacy shape (compared as normalized
    /// JSON, so whitespace in the literal does not matter). If this fails, the wire
    /// shape moved and `LEGACY_QUEUE_ROW` — plus every deployed peer — is now behind.
    #[test]
    fn todays_default_writer_still_emits_the_frozen_legacy_shape() {
        let today = serde_json::to_value(RunDispatch::new(activation())).expect("value");
        let frozen: serde_json::Value =
            serde_json::from_str(LEGACY_QUEUE_ROW).expect("frozen parses");
        assert_eq!(
            today, frozen,
            "the default wire shape drifted from the frozen legacy row"
        );
    }

    #[test]
    fn non_default_delegation_limits_round_trip_on_the_queue_wire() {
        let mut request = RunDispatch::new(activation());
        request.activation.snapshot.resolved_spec.delegation_limits =
            awaken_agent_contract::agent::delegation::DelegationLimits::new(3, 4, 5);

        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(
            wire["activation"]["snapshot"]["resolved_spec"]["delegation_limits"],
            serde_json::json!({
                "max_depth": 3,
                "max_parallel": 4,
                "max_total": 5
            })
        );
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(
            restored.activation.snapshot.resolved_spec.delegation_limits,
            awaken_agent_contract::agent::delegation::DelegationLimits::new(3, 4, 5)
        );
    }

    #[test]
    fn execution_envelope_round_trips_without_exposing_provider_credentials() {
        let authority =
            awaken_tenancy::Authority::bound(awaken_tenancy::ScopeId("workspace-a".to_string()));
        let claimed = ExecutionScopeRef(awaken_tenancy::ScopeId("workspace-a".to_string()));
        let verified = authority
            .verify_execution_scope(&claimed)
            .expect("scope belongs to authority");
        let mut activation = activation();
        activation.snapshot.metadata.inference_access = Some(
            awaken_runtime_contract::InferenceAccess::new("credential-reference/v1", "grant-17"),
        );
        let request = RunDispatch::new(activation).with_execution_scope(verified.into_ref());
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["execution_scope"], "workspace-a");
        assert_eq!(
            wire["activation"]["snapshot"]["metadata"]["inference_access"]["scheme"],
            "credential-reference/v1"
        );
        assert!(!wire.to_string().contains("provider-key"));
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(restored, request);
    }

    #[test]
    fn candidate_access_is_ordered_pinned_and_model_scoped() {
        let access = awaken_runtime_contract::InferenceAccess::candidate_set([
            (
                "primary".to_string(),
                awaken_runtime_contract::InferenceAccess::exact_credential(
                    "cred-a",
                    "provider-a@1",
                    "route-a@2",
                ),
            ),
            (
                "fallback".to_string(),
                awaken_runtime_contract::InferenceAccess::exact_credential(
                    "cred-b",
                    "provider-b@4",
                    "route-b@3",
                ),
            ),
        ])
        .unwrap();
        assert_eq!(
            access
                .candidates
                .iter()
                .map(|candidate| candidate.model_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["primary", "fallback"]
        );
        let fallback = access.for_model("fallback").unwrap();
        assert_eq!(fallback.reference, "cred-b");
        assert_eq!(fallback.provider_ref.as_deref(), Some("provider-b@4"));
        assert!(fallback.candidates.is_empty());
        assert!(access.for_model("not-authored").is_none());
        let wire = serde_json::to_string(&access).unwrap();
        assert!(!wire.contains("secret"));
        assert_eq!(
            serde_json::from_str::<awaken_runtime_contract::InferenceAccess>(&wire).unwrap(),
            access
        );
    }

    #[test]
    fn host_executor_access_is_exact_and_non_secret() {
        let access = awaken_runtime_contract::InferenceAccess::host_executor("embedded-model");
        assert!(access.is_host_executor_for("embedded-model"));
        assert!(!access.is_host_executor_for("another-model"));
        assert!(access.provider_ref.is_none());
        assert!(access.route_ref.is_none());
        assert!(!serde_json::to_string(&access).unwrap().contains("secret"));
    }

    #[test]
    fn strict_worker_requirements_are_pinned_while_legacy_bytes_stay_unchanged() {
        let request =
            RunDispatch::new(activation()).with_placement(PlacementRequirements::remote_required());
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["placement"]["contract_version"], 1);
        assert_eq!(wire["placement"]["location"], "remote_required");
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(restored, request);
    }
}
