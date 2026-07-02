//! Resolution converges inline and by-id snapshot inputs and fails closed on a
//! fingerprint mismatch or a missing catalog (G4/G22/G28).

use awaken_runtime::Runtime;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resolver::{
    AgentSnapshotCatalog, AgentSnapshotResolver, Error, RunResolver,
};
use awaken_runtime_contract::snapshot::{
    AgentId, AgentSnapshotInput, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

fn install(runtime: &Runtime, fingerprint: &str) {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("catalog installs");
}

fn snapshot(fingerprint: &str) -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fingerprint.clone(),
            instructions: String::new(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_instance_ref: "provider-1".to_string(),
                model_ref: "model-1".to_string(),
                backend_ref: "backend-1".to_string(),
            },
            tool_descriptors: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
        },
        fingerprint,
    }
}

#[test]
fn resolve_returns_plan_when_fingerprints_match() {
    let runtime = Runtime::new();
    install(&runtime, "catalog-a");

    let resolved = runtime.resolve(&snapshot("catalog-a")).expect("resolves");
    assert_eq!(resolved.snapshot_id.0, "snapshot-1");
    assert_eq!(resolved.spec.model_binding.model_ref, "model-1");
}

#[test]
fn resolve_fails_closed_on_fingerprint_mismatch() {
    let runtime = Runtime::new();
    install(&runtime, "catalog-a");

    assert_eq!(
        runtime.resolve(&snapshot("catalog-b")),
        Err(Error::FingerprintMismatch)
    );
}

#[test]
fn resolve_fails_closed_without_a_catalog() {
    let runtime = Runtime::new();
    assert_eq!(
        runtime.resolve(&snapshot("catalog-a")),
        Err(Error::NoActiveCatalog)
    );
}

#[test]
fn inline_and_by_id_inputs_converge_to_the_same_validated_plan() {
    let runtime = Runtime::new();
    install(&runtime, "catalog-a");
    let id = runtime.register_snapshot(snapshot("catalog-a"));

    let inline = runtime
        .load_snapshot(&AgentSnapshotInput::Inline(Box::new(snapshot("catalog-a"))))
        .and_then(|s| runtime.resolve(&s))
        .expect("inline resolves");
    let by_id = runtime
        .load_snapshot(&AgentSnapshotInput::ById(id.clone()))
        .and_then(|s| runtime.resolve(&s))
        .expect("by-id resolves");

    assert_eq!(inline.snapshot_id, by_id.snapshot_id);
    assert_eq!(inline.spec, by_id.spec);
}

#[test]
fn by_id_lookup_and_catalog_listing_work() {
    let runtime = Runtime::new();
    install(&runtime, "catalog-a");
    let id = runtime.register_snapshot(snapshot("catalog-a"));

    assert!(runtime.get_snapshot(&id).expect("ok").is_some());
    assert_eq!(runtime.list_snapshots(), vec![id]);
    assert_eq!(
        runtime.load_snapshot(&AgentSnapshotInput::ById(ExecutableAgentSnapshotId(
            "missing".to_string()
        ))),
        Err(Error::SnapshotNotFound)
    );
}
