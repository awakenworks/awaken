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
        model_binding: ModelBinding::new("prov", "gpt", "genai"),
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

    // The nested model binding names its three refs exactly.
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
    assert_eq!(v["plugin_config"], json!({}));

    // The default context policy is internally tagged on `kind`.
    assert_eq!(v["context_policy"], json!({ "kind": "keep_all" }));

    // An empty tool presentation renders as `{}` (its `facets` map is skipped empty).
    assert_eq!(v["tool_presentation"], json!({}));
}

// --- Item 1: RunActivation wire shape --------------------------------------

fn activation() -> RunActivation {
    let snapshot = ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("snap-1".into()),
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
