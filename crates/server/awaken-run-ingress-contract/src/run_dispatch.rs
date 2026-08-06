//! One Run accepted into durable dispatch.
//!
//! [`RunDispatch`] is exactly the data a durable queue persists and replays. It
//! carries no live handles (G3/G4), so any worker can rebuild the Run after a
//! crash. Live worker dependencies stay in the `awaken-run-ingress` host.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::PlaintextHolder;
use awaken_runtime_contract::activation::RunActivation;
pub use awaken_tenancy::ExecutionScopeRef;
pub use awaken_worker_contract::PlacementRequirements;
use serde::{Deserialize, Serialize};

/// Dispatch-neutral envelope for a frozen Session resource manifest.
///
/// The dispatch bounded context owns delivery, not Session resource vocabulary,
/// so the already-resolved manifest crosses as canonical serialized data. The
/// Runtime Host decodes it through the Session contract before any sandbox is
/// opened. `workspace_id` remains explicit so claim-time scope equality can be
/// checked without interpreting the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResourceEnvelope {
    pub workspace_id: String,
    /// Opaque Session-owned resource generation used only for ordered replay.
    #[serde(default)]
    pub resource_revision: u64,
    pub resolved_resources_json: String,
}

impl SessionResourceEnvelope {
    #[must_use]
    pub fn new(
        workspace_id: impl Into<String>,
        resolved_resources_json: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            resource_revision: 0,
            resolved_resources_json: resolved_resources_json.into(),
        }
    }

    #[must_use]
    pub fn at_revision(
        workspace_id: impl Into<String>,
        resource_revision: u64,
        resolved_resources_json: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            resource_revision,
            resolved_resources_json: resolved_resources_json.into(),
        }
    }

    /// Encode the Session-owned manifest into its one durable dispatch shape.
    pub fn from_manifest(
        manifest: &awaken_session_contract::SessionResourceManifest,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::at_revision(
            manifest.workspace_id.clone(),
            manifest.revision,
            serde_json::to_string(&manifest.resources)?,
        ))
    }

    /// Restore the Session-owned manifest before validation or realization.
    pub fn decode_manifest(
        &self,
    ) -> Result<awaken_session_contract::SessionResourceManifest, serde_json::Error> {
        Ok(
            awaken_session_contract::SessionResourceManifest::at_revision(
                self.workspace_id.clone(),
                self.resource_revision,
                serde_json::from_str(&self.resolved_resources_json)?,
            ),
        )
    }
}

/// Dispatch-neutral envelope for the immutable Session runtime projection.
///
/// Environment and tool-policy vocabulary remain owned by the Session bounded
/// context. Dispatch persists only canonical, secret-free serialized data so a
/// cold remote Worker can reconstruct the same eager/deferred provisioning
/// decision without consulting mutable Control state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeEnvelope {
    pub projection_json: String,
}

impl SessionRuntimeEnvelope {
    #[must_use]
    pub fn new(projection_json: impl Into<String>) -> Self {
        Self {
            projection_json: projection_json.into(),
        }
    }
}

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
    /// Frozen Session resource input for cold-worker activation. The value is
    /// secret-free and carries only the intrinsic Workspace partition plus the
    /// already-resolved resource configuration. A worker installs it before opening
    /// the Session environment; it must never re-read current Agent bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_resources: Option<SessionResourceEnvelope>,
    /// Frozen Environment and Session-local tool-policy projection used by a
    /// cold Worker before context construction. Older rows omit it and retain
    /// their historical eager behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_runtime: Option<SessionRuntimeEnvelope>,
    /// Provider-neutral Environment creation shape. This is a placement
    /// preference only: workers without a ready receipt remain eligible and use
    /// the canonical cold path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_environment_shape: Option<String>,
    /// Exact Environment/deployment request for inference credential plaintext.
    /// Claim admission validates it against every selected published candidate
    /// and the selected Worker's installed capabilities before persisting the
    /// attempt-epoch bindings. `None` is valid only when no selected candidate
    /// carries a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_plaintext_holder: Option<PlaintextHolder>,
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
            session_resources: None,
            session_runtime: None,
            preferred_environment_shape: None,
            inference_plaintext_holder: None,
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

    /// Attach the exact resource manifest selected by Session creation.
    #[must_use]
    pub fn with_session_resources(mut self, resources: SessionResourceEnvelope) -> Self {
        self.session_resources = Some(resources);
        self
    }

    /// Attach the exact runtime projection frozen at Session creation.
    #[must_use]
    pub fn with_session_runtime(mut self, runtime: SessionRuntimeEnvelope) -> Self {
        self.session_runtime = Some(runtime);
        self
    }

    #[must_use]
    pub fn with_preferred_environment_shape(mut self, shape: impl Into<String>) -> Self {
        self.preferred_environment_shape = Some(shape.into());
        self
    }

    /// Attach the trusted Environment/deployment's exact inference holder
    /// request. This is one request, never a preference list or fallback order.
    #[must_use]
    pub fn with_inference_plaintext_holder(mut self, holder: PlaintextHolder) -> Self {
        self.inference_plaintext_holder = Some(holder);
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
    use awaken_runtime_contract::resolved::{
        CatalogFingerprint, ModelBinding, ModelProvisioning, ResolvedSpec,
    };
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn provider_candidate(
        model: &str,
        credential: &str,
        provider: &str,
        route: &str,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
            ModelBinding::new(provider, model, "genai"),
            provider,
            route,
            "workspace-a",
            Some(awaken_runtime_contract::CredentialAccess::new(
                awaken_runtime_contract::CredentialRef {
                    id: credential.into(),
                    revision: 0,
                },
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
            )),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: model.into(),
            },
        )
    }

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
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("prov", "model", "acp:test"),
                    ),
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

    /// Resource-envelope compatibility causes/effects: C1 current dispatch has a
    /// Session generation; C2 legacy dispatch omits it. E1 C1 round-trips the
    /// exact generation; E2 C2 decodes as generation zero without dead-lettering.
    #[test]
    fn frozen_session_resources_round_trip_with_their_matching_scope() {
        let resources =
            SessionResourceEnvelope::at_revision("workspace-a", 4, r#"{"inputs":[],"skills":[]}"#);
        let request = RunDispatch::new(activation())
            .with_execution_scope(ExecutionScopeRef(awaken_tenancy::ScopeId::from(
                "workspace-a",
            )))
            .with_session_resources(resources.clone());

        let json = serde_json::to_string(&request).expect("serializes");
        let recovered: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(recovered.session_resources, Some(resources));
        assert_eq!(
            recovered.execution_scope,
            Some(ExecutionScopeRef(awaken_tenancy::ScopeId::from(
                "workspace-a"
            )))
        );

        let legacy: SessionResourceEnvelope = serde_json::from_value(serde_json::json!({
            "workspace_id": "workspace-a",
            "resolved_resources_json": "{\"inputs\":[]}"
        }))
        .expect("legacy envelope");
        assert_eq!(legacy.resource_revision, 0);
    }

    #[test]
    fn frozen_session_runtime_projection_round_trips_without_session_vocabulary() {
        let runtime = SessionRuntimeEnvelope::new(
            r#"{"environment":{"sandbox_provisioning":"on_tool_use"},"toolsets":[]}"#,
        );
        let request = RunDispatch::new(activation()).with_session_runtime(runtime.clone());

        let json = serde_json::to_string(&request).expect("serializes");
        let recovered: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(recovered.session_runtime, Some(runtime));
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
        assert!(back.session_resources.is_none());
        assert!(back.session_runtime.is_none());
        assert!(back.placement.is_legacy_default());
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
        activation.snapshot.resolved_spec.model_binding =
            provider_candidate("model", "grant-17", "provider@1", "route@1");
        let request = RunDispatch::new(activation).with_execution_scope(verified.into_ref());
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["execution_scope"], "workspace-a");
        assert_eq!(
            wire["activation"]["snapshot"]["resolved_spec"]["model_binding"]["provisioning"]["credential"]
                ["credential"]["id"],
            "grant-17"
        );
        assert!(!wire.to_string().contains("provider-key"));
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(restored, request);
    }

    #[test]
    fn candidate_pool_is_ordered_pinned_and_model_scoped() {
        let mut activation = activation();
        activation.snapshot.resolved_spec.model_binding =
            provider_candidate("primary", "cred-a", "provider-a@1", "route-a@2");
        activation.snapshot.resolved_spec.model_candidates = vec![provider_candidate(
            "fallback",
            "cred-b",
            "provider-b@4",
            "route-b@3",
        )];
        let spec = &activation.snapshot.resolved_spec;
        assert_eq!(
            std::iter::once(&spec.model_binding)
                .chain(spec.model_candidates.iter())
                .map(|candidate| candidate.binding.model_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["primary", "fallback"]
        );
        let ModelProvisioning::Provider {
            provider_ref,
            credential: Some(credential),
            ..
        } = &spec.model_candidates[0].provisioning
        else {
            panic!("fallback is provider-backed")
        };
        assert_eq!(credential.credential.id, "cred-b");
        assert_eq!(provider_ref, "provider-b@4");
        assert!(spec.candidate_for_model("not-authored").is_none());
        let wire = serde_json::to_string(&activation).unwrap();
        assert!(!wire.contains("secret"));
        assert_eq!(
            serde_json::from_str::<RunActivation>(&wire).unwrap(),
            activation
        );
    }

    #[test]
    fn host_executor_candidate_is_exact_and_non_secret() {
        let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("host", "embedded-model", "native"),
        );
        assert_eq!(candidate.binding.model_ref, "embedded-model");
        assert!(matches!(
            candidate.provisioning,
            ModelProvisioning::HostExecutor
        ));
        assert!(
            !serde_json::to_string(&candidate)
                .unwrap()
                .contains("secret")
        );
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

    /// Resource-envelope ACL cause/effect decision table. Causes: C1 a valid
    /// Session manifest; C2 an envelope with valid resource JSON; C3 malformed
    /// resource JSON. Effects: E1 workspace/revision and resources are encoded
    /// losslessly; E2 the exact manifest is restored; E3 decoding fails closed.
    /// Rules A1 C1=>E1, A2 C1+C2=>E2, A3 C3=>E3 cover every JSON validity class.
    #[test]
    fn session_resource_envelope_is_the_single_lossless_fail_closed_acl() {
        let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            7,
            Default::default(),
        );
        let envelope = SessionResourceEnvelope::from_manifest(&manifest).expect("A1 encode");
        assert_eq!(envelope.workspace_id, "workspace-a", "A1");
        assert_eq!(envelope.resource_revision, 7, "A1");
        assert_eq!(envelope.decode_manifest().expect("A2 decode"), manifest);

        let malformed = SessionResourceEnvelope::at_revision("workspace-a", 7, "{");
        assert!(malformed.decode_manifest().is_err(), "A3");
    }
}
