use serde::{Deserialize, Serialize};

use crate::DataSubjectId;
use crate::permission::ToolCapabilityNarrowing;
use crate::runtime_context::RuntimeRunContext;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunActivation {
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub snapshot: crate::snapshot::ExecutableAgentSnapshot,
    pub input: Vec<awaken_agent_contract::agent::message::Message>,
    /// Durable parent/call identity when this Run was created by an Agent tool.
    /// This belongs to the serializable activation, not `RuntimeRunContext`:
    /// recovery must retain the same origin even when every live handle changes.
    #[serde(default, alias = "initiator", skip_serializing_if = "Option::is_none")]
    pub delegation_origin: Option<awaken_agent_contract::agent::delegation::DelegationOrigin>,
    /// Per-run model selector (R5): the model ref of one candidate already present
    /// in the agent publication. Deliberately OFF the fingerprinted
    /// snapshot — a per-Run model switch is a runtime choice, not a catalog change,
    /// so it must not mint a new `catalog_fingerprint` (mirrors how display metadata
    /// is excluded from the content address). Absent ⇒ the run uses its snapshot's
    /// `model_binding.model_ref`. This names *which* model to run; the runtime never
    /// sees *how* it is reached — that is the provisioning seam's job. A selector
    /// outside the published candidate set is rejected before any model call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref_override: Option<String>,
    /// Request-grained content owner. This neutral id is durable because retries,
    /// remote claims, and same-Run resumes must retain attribution without
    /// consulting ambient process state or a protocol-specific session default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_subject_id: Option<DataSubjectId>,
    /// Durable, backend-neutral restriction on the executor's configured tool
    /// authority. It belongs to the activation because recovery must enforce the
    /// same restriction on every attempt and every worker process.
    #[serde(
        default,
        skip_serializing_if = "ToolCapabilityNarrowing::is_configured"
    )]
    pub tool_capability_narrowing: ToolCapabilityNarrowing,
}

impl RunActivation {
    /// A fresh activation carrying only the runtime-core inputs. *How* the run
    /// reaches its model is not here: the resolve seam turns the run's model ref into
    /// a concrete provider (executor) before execution — the runtime only ever names a
    /// model and is handed the executor, never learning how the model is reached.
    ///
    /// Distributed *trace* propagation is NOT carried here either: the admitting request's
    /// W3C `traceparent` rides the ingress envelope (`RunDispatch`) across
    /// the durable queue and is restored as the `wake.dispatch` span's remote
    /// parent, so a durably-drained run still nests under the trace that submitted
    /// it. The runtime core never reads a trace field.
    #[must_use]
    pub fn new(
        run_id: awaken_agent_contract::agent::run::Id,
        thread_id: awaken_agent_contract::agent::thread::Id,
        snapshot: crate::snapshot::ExecutableAgentSnapshot,
        input: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Self {
        Self {
            run_id,
            thread_id,
            snapshot,
            input,
            delegation_origin: None,
            model_ref_override: None,
            data_subject_id: None,
            tool_capability_narrowing: ToolCapabilityNarrowing::Configured,
        }
    }

    #[cfg(test)]
    fn for_binding(binding_model_ref: &str) -> Self {
        use crate::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
        use crate::snapshot::{AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
        Self::new(
            awaken_agent_contract::agent::run::Id("r".into()),
            awaken_agent_contract::agent::thread::Id("t".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: crate::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("prov", binding_model_ref, "backend"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            Vec::new(),
        )
    }

    /// Set the per-run model override (R5) — the model ref to run this attempt on.
    #[must_use]
    pub fn with_model_ref_override(mut self, model_ref: Option<String>) -> Self {
        self.model_ref_override = model_ref;
        self
    }

    /// Attach the request-grained neutral content owner.
    #[must_use]
    pub fn with_data_subject(mut self, subject: Option<DataSubjectId>) -> Self {
        self.data_subject_id = subject;
        self
    }

    /// Attach the stable relationship that created a delegated child Run.
    #[must_use]
    pub fn with_delegation_origin(
        mut self,
        delegation_origin: awaken_agent_contract::agent::delegation::DelegationOrigin,
    ) -> Self {
        self.delegation_origin = Some(delegation_origin);
        self
    }

    /// Deny every tool for this Run without changing the pinned Agent snapshot.
    #[must_use]
    pub fn without_tools(mut self) -> Self {
        self.tool_capability_narrowing = ToolCapabilityNarrowing::DenyAll;
        self
    }

    /// Apply durable capability narrowing to one attempt's process-local
    /// context. Deny-all is the strongest intersection, so replacing any
    /// existing policy cannot widen authority.
    #[must_use]
    pub fn narrow_context(&self, context: RuntimeRunContext) -> RuntimeRunContext {
        match self.tool_capability_narrowing {
            ToolCapabilityNarrowing::Configured => context,
            ToolCapabilityNarrowing::DenyAll => {
                context.with_tool_capability_narrowing(ToolCapabilityNarrowing::DenyAll)
            }
        }
    }

    /// The model ref this attempt requests: its per-run selector (R5) when set,
    /// otherwise the model its pinned snapshot binding names. This is the single
    /// input the resolve seam turns into a provider — the runtime never looks a model
    /// up, it is handed the resolved executor. The execution gate separately
    /// verifies that a non-blank selector belongs to the published candidate set;
    /// a blank selector is treated as absent.
    #[must_use]
    pub fn effective_model_ref(&self) -> &str {
        match self.model_ref_override.as_deref() {
            Some(m) if !m.is_empty() => m,
            _ => &self.snapshot.resolved_spec.model_binding.model_ref,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunActivation;

    // Cause-effect coverage for `effective_model_ref`. Causes: the override is
    // present / blank / absent. Effect: the ref used is the override, else the
    // snapshot binding — a blank override collapses to "absent".
    #[test]
    fn effective_ref_is_the_override_when_present() {
        let a = RunActivation::for_binding("bound").with_model_ref_override(Some("chosen".into()));
        assert_eq!(a.effective_model_ref(), "chosen");
    }

    #[test]
    fn effective_ref_falls_back_to_the_binding_when_no_override() {
        let a = RunActivation::for_binding("bound");
        assert_eq!(a.effective_model_ref(), "bound");
    }

    #[test]
    fn a_blank_override_collapses_to_the_binding() {
        let a = RunActivation::for_binding("bound").with_model_ref_override(Some(String::new()));
        assert_eq!(a.effective_model_ref(), "bound");
    }

    /// The override is off the fingerprinted snapshot — switching it must not change
    /// the snapshot identity (a per-Run model switch is not a catalog change).
    #[test]
    fn overriding_the_model_does_not_touch_the_snapshot_fingerprint() {
        let base = RunActivation::for_binding("bound");
        let fp = base.snapshot.fingerprint.clone();
        let overridden = base.with_model_ref_override(Some("chosen".into()));
        assert_eq!(overridden.snapshot.fingerprint, fp);
    }
}

/// The serialization *contract* for the activation — the runtime-core input crosses
/// the config/data/worker planes only as serialized bytes, so its wire shape is a
/// compatibility boundary, not an implementation detail. These pin: a lossless
/// round-trip, `model_ref_override`'s `skip_serializing_if` wire compaction, and —
/// the two that a self-round-trip cannot catch — that an OLDER writer's payload
/// (which predates the field) still loads, and a NEWER writer's unknown field does
/// not dead-letter today's reader. Together they hold "planes exchange only
/// serializable objects, evolvably" honest.
#[cfg(test)]
mod serde_contract {
    use super::RunActivation;

    #[test]
    fn round_trips_lossless_with_and_without_an_override() {
        for act in [
            RunActivation::for_binding("bound"),
            RunActivation::for_binding("bound").with_model_ref_override(Some("chosen".into())),
            RunActivation::for_binding("bound")
                .with_data_subject(Some(crate::DataSubjectId("dsub_1".into()))),
            RunActivation::for_binding("bound").without_tools(),
        ] {
            let json = serde_json::to_string(&act).expect("serializes");
            let back: RunActivation = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, act, "round-trip is lossless");
        }
    }

    #[test]
    fn a_none_override_is_omitted_on_the_wire_but_a_set_one_is_written() {
        let none = RunActivation::for_binding("bound");
        let json = serde_json::to_string(&none).expect("serializes");
        assert!(
            !json.contains("model_ref_override"),
            "a None override must not be written (skip_serializing_if): {json}"
        );

        let set = none.with_model_ref_override(Some("chosen".into()));
        let json = serde_json::to_string(&set).expect("serializes");
        assert!(
            json.contains("model_ref_override"),
            "a set override is written"
        );
    }

    #[test]
    fn configured_tool_authority_is_compact_but_deny_all_is_durable() {
        let configured = serde_json::to_value(RunActivation::for_binding("bound")).unwrap();
        assert!(configured.get("tool_capability_narrowing").is_none());

        let denied = serde_json::to_value(RunActivation::for_binding("bound").without_tools())
            .expect("restricted activation serializes");
        assert_eq!(
            denied.get("tool_capability_narrowing"),
            Some(&serde_json::json!("deny_all"))
        );
        let recovered: RunActivation = serde_json::from_value(denied).unwrap();
        assert_eq!(
            recovered.tool_capability_narrowing,
            crate::permission::ToolCapabilityNarrowing::DenyAll
        );
    }

    #[test]
    fn a_legacy_payload_without_the_override_field_loads_as_none() {
        // Simulate a writer that predates `model_ref_override`: take a full payload and
        // strip the key. It must deserialize (that is what `#[serde(default)]` buys)
        // and fall back to the snapshot binding — never a "missing field" error.
        let full = RunActivation::for_binding("bound").with_model_ref_override(Some("x".into()));
        let mut value = serde_json::to_value(&full).expect("to value");
        value
            .as_object_mut()
            .unwrap()
            .remove("model_ref_override")
            .expect("the key was present to remove");
        value
            .as_object_mut()
            .unwrap()
            .remove("tool_capability_narrowing");

        let legacy: RunActivation = serde_json::from_value(value).expect("a legacy row loads");
        assert!(legacy.model_ref_override.is_none());
        assert!(legacy.tool_capability_narrowing.is_configured());
        assert_eq!(
            legacy.effective_model_ref(),
            "bound",
            "a legacy run falls back to its snapshot binding"
        );
    }

    #[test]
    fn an_unknown_future_field_is_ignored_not_rejected() {
        // A newer writer may add fields this reader has never seen. Without
        // `deny_unknown_fields`, today's reader must skip them rather than
        // dead-letter the row — the forward half of the compatibility guarantee.
        let mut value = serde_json::to_value(RunActivation::for_binding("bound")).expect("value");
        value
            .as_object_mut()
            .unwrap()
            .insert("a_field_from_the_future".into(), serde_json::json!(42));
        let back: RunActivation =
            serde_json::from_value(value).expect("an unknown field is ignored");
        assert_eq!(back.effective_model_ref(), "bound");
    }

    #[test]
    fn delegated_origin_survives_the_activation_wire() {
        let origin = awaken_agent_contract::agent::delegation::DelegationOrigin::root_for_agent(
            awaken_agent_contract::agent::run::Id("parent".into()),
            "call",
            "coordinator",
        );
        let activation = RunActivation::for_binding("bound").with_delegation_origin(origin.clone());
        let mut value = serde_json::to_value(&activation).unwrap();
        assert!(value.get("delegation_origin").is_some());
        assert!(value.get("initiator").is_none());
        let recovered: RunActivation = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(recovered.delegation_origin, Some(origin));

        let object = value.as_object_mut().unwrap();
        let legacy = object.remove("delegation_origin").unwrap();
        object.insert("initiator".into(), legacy);
        let recovered_legacy: RunActivation = serde_json::from_value(value).unwrap();
        assert!(recovered_legacy.delegation_origin.is_some());
    }

    /// Cause/effect decision table for attribution wire compatibility:
    /// R1 subject=Some -> field is durable and round-trips; R2 subject=None ->
    /// field is omitted; R3 legacy field=absent -> None. These are the complete
    /// combinations because attribution is an optional opaque value.
    #[test]
    fn request_subject_is_optional_evolvable_and_durable() {
        let attributed = RunActivation::for_binding("bound")
            .with_data_subject(Some(crate::DataSubjectId("dsub_alice".into())));
        let value = serde_json::to_value(&attributed).unwrap();
        assert_eq!(
            value.get("data_subject_id"),
            Some(&serde_json::json!("dsub_alice"))
        );
        assert_eq!(
            serde_json::from_value::<RunActivation>(value)
                .unwrap()
                .data_subject_id,
            attributed.data_subject_id
        );

        let legacy = serde_json::to_value(RunActivation::for_binding("bound")).unwrap();
        assert!(legacy.get("data_subject_id").is_none());
        assert!(
            serde_json::from_value::<RunActivation>(legacy)
                .unwrap()
                .data_subject_id
                .is_none()
        );
    }
}
