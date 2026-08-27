//! Value-level wire-shape pins for the boundary types.
//!
//! `serde_boundary.rs` proves the boundary types are *serializable* (compile-only:
//! `Serialize + DeserializeOwned` bounds). It cannot catch a silent RENAME — a field
//! or enum tag changing name still satisfies the bound while breaking every peer that
//! already persisted the old shape. These tests serialize concrete values to
//! `serde_json::Value` and assert the exact field NAMES and enum TAGS, so a rename is
//! a red test rather than a wire break discovered in production.

use serde_json::{Value, json};

use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// The sorted top-level object keys of a JSON value (for an exact key-set assertion).
fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect();
    k.sort();
    k
}

fn spec() -> ResolvedSpec {
    ResolvedSpec {
        catalog_fingerprint: CatalogFingerprint("fp-1".into()),
        instructions: "be concise".into(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("prov", "gpt", "genai"),
        ),
        model_candidates: Vec::new(),
        tool_descriptors: Vec::new(),
        plugin_ids: Vec::new(),
        plugin_config: Default::default(),
        context_policy: Default::default(),
        tool_presentation: Default::default(),
    }
}

// --- Item 1: ResolvedSpec wire shape ---------------------------------------

#[test]
fn resolved_spec_field_names_are_pinned() {
    let v = serde_json::to_value(spec()).expect("serialize");
    assert_eq!(
        keys(&v),
        vec![
            "catalog_fingerprint",
            "context_policy",
            "instructions",
            "max_steps",
            "model_binding",
            "model_candidates",
            "plugin_config",
            "plugin_ids",
            "tool_descriptors",
            "tool_presentation",
        ],
        "the resolved-spec wire keys are a compatibility surface"
    );

    // The fingerprint newtype is a BARE string on the wire, not a wrapped object.
    assert_eq!(v["catalog_fingerprint"], json!("fp-1"));

    // Host-executor candidates retain the frozen legacy model-binding shape.
    // Provider publications add an explicit provisioning object, pinned below.
    assert_eq!(
        v["model_binding"],
        json!({
            "provider_identity_ref": "prov",
            "model_ref": "gpt",
            "backend_ref": "genai",
        })
    );

    // The defaulted collections serialize as empty (present, not skipped).
    assert_eq!(v["model_candidates"], json!([]));
    assert_eq!(v["tool_descriptors"], json!([]));
    assert_eq!(v["plugin_ids"], json!([]));
    assert_eq!(
        v["plugin_config"],
        json!({
            "agent": {
                "mcp_servers": [],
                "skills": [],
            },
            "plugins": {},
        })
    );

    // The default context policy is internally tagged on `kind`.
    assert_eq!(v["context_policy"], json!({ "kind": "keep_all" }));

    // An empty tool presentation renders as `{}` (its `facets` map is skipped empty).
    assert_eq!(v["tool_presentation"], json!({}));
}

#[test]
fn delegation_bindings_read_legacy_ids_but_write_one_canonical_shape() {
    // Cause graph: C1 persisted input uses legacy `delegate_ids: [string]`;
    // C2 input uses revision-aware `delegates` objects. Both cause E1=one
    // canonical in-memory binding; every subsequent serialization causes
    // E2=only the revision-aware shape. Decision rules L1=C1→E1+E2 and
    // L2=C2→E1+E2; no parallel runtime roster survives deserialization.
    let legacy: awaken_runtime_contract::agent_bindings::AgentBindings =
        serde_json::from_value(json!({
            "mcp_servers": [],
            "skills": [],
            "delegate_ids": ["worker"]
        }))
        .unwrap();
    assert_eq!(legacy.delegates[0].agent_id, AgentId("worker".into()));
    assert_eq!(legacy.delegates[0].source_revision, None);
    assert_eq!(
        serde_json::to_value(legacy).unwrap()["delegates"],
        json!([{ "agent_id": "worker" }])
    );
}

#[test]
fn provider_model_candidate_provisioning_is_pinned() {
    // Test design — Cause: immutable publication selects one Provider candidate
    // with exact provider/route/credential plus adapter and API dialect. Effect:
    // the wire preserves every axis verbatim for later realization.
    // Constraints/invariants: adapter family and dialect are separate pins;
    // neither Runtime nor credential realization may reselect them. Decision
    // rule P1=fully pinned Provider=>the exact nested provisioning shape below.
    let mut spec = spec();
    spec.model_binding = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
        ModelBinding::new("identity-a", "model-a", "genai"),
        "provider-a@2",
        "route-a@3",
        "workspace-a",
        Some(awaken_runtime_contract::CredentialAccess::new(
            awaken_runtime_contract::CredentialRef {
                id: "credential-a".into(),
                revision: 4,
            },
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_runtime_contract::CredentialUsage::ProviderAdapter,
            awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )),
        awaken_runtime_contract::InferenceEndpoint {
            adapter_kind: "openai_chat_completions".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://provider.example/v1".into(),
            upstream_model: "upstream-a".into(),
            processing_placement: None,
        },
    )
    .expect("coherent provider candidate");

    let wire = serde_json::to_value(spec).expect("serialize provider candidate");
    assert_eq!(
        wire["model_binding"]["provisioning"],
        json!({
            "type": "provider",
            "provider_ref": "provider-a@2",
            "route_ref": "route-a@3",
            "access_kind": "direct",
            "scope_id": "workspace-a",
            "credential": {
                "credential": { "id": "credential-a", "revision": 4 },
                "material_source": "control_plane_reference",
                "usage": { "type": "provider_adapter" },
                "policy": {
                    "allowed_plaintext_holders": [
                        { "boundary": "workload", "trust_domain": "awaken.workload.acp" },
                        { "boundary": "worker", "trust_domain": "awaken.worker" }
                    ],
                    "model_exposure": "forbidden"
                }
            },
            "endpoint": {
                "adapter_kind": "openai_chat_completions",
                "api_dialect": "open_ai_chat",
                "base_url": "https://provider.example/v1",
                "upstream_model": "upstream-a"
            }
        })
    );
}

#[test]
fn acp_execution_profile_preserves_backend_wire_and_is_optional_on_provider_wire() {
    // Causes: C1 backend-owned ACP always has a capability profile; C2 native
    // Provider has none; C3 ACP Provider has one. Effects: E1 BackendOwned keeps
    // the historical flattened field names; E2 native Provider has no `acp`;
    // E3 ACP Provider carries exactly one nested profile. These three rules
    // prevent a parallel capability/configuration shape per provisioning mode.
    let profile = awaken_runtime_contract::resolved::AcpExecutionProfile {
        capability_adapter_version: "1.2.3".into(),
        capability_fingerprint: "sha256:profile".into(),
        session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration {
            mode: Some("plan".into()),
            options: [("reasoning_effort".into(), "high".into())]
                .into_iter()
                .collect(),
        },
    };
    let backend = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
        ModelBinding::new("local-codex", "", "acp:codex"),
        awaken_runtime_contract::CredentialRef {
            id: "local-codex".into(),
            revision: 1,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "1.2.3",
        "sha256:profile",
        profile.session_configuration.clone(),
    )
    .expect("coherent backend-owned candidate");
    assert_eq!(
        serde_json::to_value(backend).unwrap()["provisioning"],
        json!({
            "type": "backend_owned",
            "credential": {"id":"local-codex", "revision":1},
            "model_selection": "default",
            "capability_adapter_version": "1.2.3",
            "capability_fingerprint": "sha256:profile",
            "session_configuration": {
                "mode": "plan",
                "options": {"reasoning_effort":"high"}
            }
        }),
        "E1"
    );

    let endpoint = || awaken_runtime_contract::InferenceEndpoint {
        adapter_kind: "openai_chat_completions".into(),
        api_dialect: "open_ai_chat".into(),
        base_url: "https://provider.example/v1".into(),
        upstream_model: "gpt-5".into(),
        processing_placement: None,
    };
    let native = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
        ModelBinding::new("openai", "gpt-5", "genai"),
        "openai@1",
        "openai.open_ai_chat@1",
        "workspace-a",
        None,
        endpoint(),
    )
    .expect("coherent native provider candidate");
    assert!(
        serde_json::to_value(&native).unwrap()["provisioning"]
            .get("acp")
            .is_none(),
        "E2"
    );

    // Cause/effect wire matrix for the closed reasoning policy:
    // W1 provider-default -> omitted for backward-compatible canonical wire.
    // W2 explicitly disabled -> one enum token, never provider-shaped JSON.
    // W3 unknown token -> rejected by serde before an executable candidate exists.
    let native_wire = serde_json::to_value(&native).unwrap();
    assert!(
        native_wire["provisioning"]
            .get("unspecified_reasoning")
            .is_none(),
        "W1"
    );
    let disabled =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_reasoning(
            ModelBinding::new("deepseek", "deepseek-chat", "genai"),
            "deepseek@1",
            "deepseek.open_ai_chat@1",
            "workspace-a",
            None,
            endpoint(),
            awaken_runtime_contract::UnspecifiedReasoning::Disabled,
        )
        .expect("coherent provider candidate with explicit reasoning policy");
    let disabled_wire = serde_json::to_value(disabled).unwrap();
    assert_eq!(
        disabled_wire["provisioning"]["unspecified_reasoning"],
        json!("disabled"),
        "W2"
    );
    let mut unknown_wire = disabled_wire;
    unknown_wire["provisioning"]["unspecified_reasoning"] = json!({"type": "vendor_extension"});
    assert!(
        serde_json::from_value::<awaken_runtime_contract::resolved::ResolvedModelCandidate>(
            unknown_wire
        )
        .is_err(),
        "W3"
    );

    let provider =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
            ModelBinding::new("openai", "gpt-5", "acp:codex"),
            "openai@1",
            "openai.open_ai_chat@1",
            "workspace-a",
            None,
            endpoint(),
            profile,
        )
        .expect("coherent ACP provider candidate");
    let provider = serde_json::to_value(provider).unwrap();
    assert_eq!(
        provider["provisioning"]["acp"]["capability_fingerprint"], "sha256:profile",
        "E3"
    );
    assert_eq!(
        provider["provisioning"]["acp"]["session_configuration"]["mode"], "plan",
        "E3"
    );
}

// --- Item 1: RunActivation wire shape --------------------------------------

fn activation() -> RunActivation {
    let snapshot = ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("snap-1".into()),
        metadata: Default::default(),
        root_agent_id: AgentId("agent-1".into()),
        resolved_spec: spec(),
        fingerprint: CatalogFingerprint("fp-1".into()),
    };
    RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-1".into()),
        awaken_agent_contract::agent::thread::Id("thread-1".into()),
        snapshot,
        Vec::new(),
    )
}

#[test]
fn run_activation_field_names_and_id_encodings_are_pinned() {
    let v = serde_json::to_value(activation()).expect("serialize");
    // A None override is omitted (skip_serializing_if), so it is NOT a key here.
    assert_eq!(
        keys(&v),
        vec!["input", "run_id", "snapshot", "thread_id"],
        "activation wire keys; model_ref_override omitted when None"
    );

    // The run/thread id newtypes are bare strings on the wire.
    assert_eq!(v["run_id"], json!("run-1"));
    assert_eq!(v["thread_id"], json!("thread-1"));
    assert_eq!(v["input"], json!([]));

    // The nested snapshot names its four fields, its ids bare strings.
    assert_eq!(
        keys(&v["snapshot"]),
        vec!["fingerprint", "id", "resolved_spec", "root_agent_id",]
    );
    assert_eq!(v["snapshot"]["id"], json!("snap-1"));
    assert_eq!(v["snapshot"]["root_agent_id"], json!("agent-1"));
    assert_eq!(v["snapshot"]["fingerprint"], json!("fp-1"));
}

#[test]
fn run_activation_writes_model_ref_override_only_when_set() {
    let set = activation().with_model_ref_override(Some("chosen".into()));
    let v = serde_json::to_value(&set).expect("serialize");
    assert_eq!(v["model_ref_override"], json!("chosen"));
    assert!(
        keys(&v).contains(&"model_ref_override".to_string()),
        "a set override is a wire key"
    );
}

// --- Item 3: exhaustive enum-tag snapshot for the boundary enums -----------

/// The G20 executor-result enums (`RunState`/`EndCause`/`Failure`) and the resolved
/// `ContextPolicy` cross planes as serialized data. This pins EVERY variant's tag so
/// a rename — or a variant swapping between unit/newtype/struct encoding — trips a
/// red test. Externally tagged (serde default) except `ContextPolicy` (internally
/// tagged on `kind`).
#[test]
fn boundary_enum_tags_are_exhaustively_pinned() {
    // RunState: two unit variants + one newtype variant carrying an EndCause.
    assert_eq!(
        serde_json::to_value(RunState::Running).unwrap(),
        json!("Running")
    );
    assert_eq!(
        serde_json::to_value(RunState::Awaiting).unwrap(),
        json!("Awaiting")
    );
    assert_eq!(
        serde_json::to_value(RunState::Ended(EndCause::NaturalEnd)).unwrap(),
        json!({ "Ended": "NaturalEnd" })
    );

    // EndCause: the closed set of terminal mechanisms.
    assert_eq!(
        serde_json::to_value(EndCause::NaturalEnd).unwrap(),
        json!("NaturalEnd")
    );
    assert_eq!(
        serde_json::to_value(EndCause::MaxSteps).unwrap(),
        json!("MaxSteps")
    );
    assert_eq!(
        serde_json::to_value(EndCause::Cancelled).unwrap(),
        json!("Cancelled")
    );
    assert_eq!(
        serde_json::to_value(EndCause::Stopped("budget".into())).unwrap(),
        json!({ "Stopped": "budget" })
    );
    assert_eq!(
        serde_json::to_value(EndCause::Error(Failure::CapabilityBound)).unwrap(),
        json!({ "Error": "CapabilityBound" })
    );
    assert_eq!(
        serde_json::to_value(EndCause::Indeterminate).unwrap(),
        json!("Indeterminate")
    );

    // Failure: one struct variant + two unit variants.
    assert_eq!(
        serde_json::to_value(Failure::Inference {
            code: "rate_limited".into(),
            message: "429".into(),
        })
        .unwrap(),
        json!({ "Inference": { "code": "rate_limited", "message": "429" } })
    );
    assert_eq!(
        serde_json::to_value(Failure::CapabilityBound).unwrap(),
        json!("CapabilityBound")
    );
    assert_eq!(
        serde_json::to_value(Failure::StateConflict).unwrap(),
        json!("StateConflict")
    );

    // ContextPolicy: internally tagged on `kind`, snake_case variant names.
    assert_eq!(
        serde_json::to_value(ContextPolicy::KeepAll).unwrap(),
        json!({ "kind": "keep_all" })
    );
    assert_eq!(
        serde_json::to_value(ContextPolicy::KeepLast { keep_last: 5 }).unwrap(),
        json!({ "kind": "keep_last", "keep_last": 5 })
    );
}
