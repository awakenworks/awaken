use std::sync::Arc;

use awaken_contract::contract::versioned_registry::PublishOutcome;
use awaken_contract::{
    AgentSpec, ModelBindingSpec, PinnedRegistryEntry, ProviderSpec, RegistryGraphValidationError,
    RegistryGraphValidationRequest, RegistryGraphValidator, StandardRegistryGraphValidator,
    VersionRef, VersionSelector, VersionedRegistryStore,
};
use awaken_stores::InMemoryVersionedRegistryStore;
use serde_json::{Value, json};

#[tokio::test]
async fn validates_latest_publication_reachable_agent_model_provider_graph() {
    let store = InMemoryVersionedRegistryStore::new();
    let provider = publish_provider(&store, "provider-1").await;
    let model = publish_model(&store, "model-1", "provider-1").await;
    let delegate = publish_agent(&store, agent("delegate", "model-1", [])).await;
    let root = publish_agent(&store, agent("root", "model-1", ["delegate"])).await;
    store
        .create_publication(
            "default",
            "pub-1",
            refs([&provider, &model, &delegate, &root]),
            Vec::new(),
            None,
            json!({}),
        )
        .await
        .unwrap();

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let report = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::LatestPublication {
                scope_id: "default".to_string(),
            },
            reference_policy: Default::default(),
        })
        .await
        .unwrap();

    assert_eq!(report.entries.len(), 4);
    assert!(report.entries.iter().any(|entry| {
        entry.kind == "agent" && entry.id == "root" && entry.content_hash == root.content_hash
    }));
    assert!(
        report
            .entries
            .iter()
            .any(|entry| { entry.kind == "provider" && entry.id == "provider-1" })
    );
}

#[tokio::test]
async fn manifest_validation_rejects_missing_delegate_entry() {
    let store = InMemoryVersionedRegistryStore::new();
    let provider = publish_provider(&store, "provider-1").await;
    let model = publish_model(&store, "model-1", "provider-1").await;
    let root = publish_agent(&store, agent("root", "model-1", ["delegate"])).await;
    let manifest = awaken_contract::PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: vec![root, model, provider],
    };

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let error = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::Manifest {
                scope_id: "default".to_string(),
                manifest,
            },
            reference_policy: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RegistryGraphValidationError::MissingResource { kind, id }
            if kind == "agent" && id == "delegate"
    ));
}

#[tokio::test]
async fn manifest_validation_detects_delegate_cycles() {
    let store = InMemoryVersionedRegistryStore::new();
    let provider = publish_provider(&store, "provider-1").await;
    let model = publish_model(&store, "model-1", "provider-1").await;
    let root = publish_agent(&store, agent("root", "model-1", ["delegate"])).await;
    let delegate = publish_agent(&store, agent("delegate", "model-1", ["root"])).await;
    let manifest = awaken_contract::PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: vec![root, delegate, model, provider],
    };

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let error = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::Manifest {
                scope_id: "default".to_string(),
                manifest,
            },
            reference_policy: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RegistryGraphValidationError::CycleDetected { .. }
    ));
}

#[tokio::test]
async fn manifest_validation_does_not_fall_back_to_current_for_missing_model() {
    // ADR-0035 D9: pinned manifests must fail closed; they must not
    // silently resolve a missing model reference against the store's
    // current version, which would let a concurrent admin publish drift
    // into an active run's resolution.
    let store = InMemoryVersionedRegistryStore::new();
    let _provider = publish_provider(&store, "provider-1").await;
    let _current_model = publish_model(&store, "model-1", "provider-1").await;
    let root = publish_agent(&store, agent("root", "model-1", [])).await;

    // Manifest deliberately omits the model entry.
    let manifest = awaken_contract::PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: vec![root],
    };

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let error = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::Manifest {
                scope_id: "default".to_string(),
                manifest,
            },
            reference_policy: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RegistryGraphValidationError::MissingResource { kind, id }
            if kind == "model" && id == "model-1"
    ));
}

#[tokio::test]
async fn manifest_validation_rejects_tampered_content_hash() {
    // ADR-0035 D3/D9: the manifest's content_hash must be verified against
    // the canonical bytes hash, not merely compared to a column value.
    let store = InMemoryVersionedRegistryStore::new();
    let provider = publish_provider(&store, "provider-1").await;
    let model = publish_model(&store, "model-1", "provider-1").await;
    let root = publish_agent(&store, agent("root", "model-1", [])).await;

    let mut tampered_root = root.clone();
    tampered_root.content_hash = "sha256:deadbeef".to_string();
    let manifest = awaken_contract::PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: vec![tampered_root, model, provider],
    };

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let error = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::Manifest {
                scope_id: "default".to_string(),
                manifest,
            },
            reference_policy: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RegistryGraphValidationError::ContentHashMismatch { kind, id, .. }
            if kind == "agent" && id == "root"
    ));
}

async fn publish_agent(
    store: &InMemoryVersionedRegistryStore,
    spec: AgentSpec,
) -> PinnedRegistryEntry {
    let id = spec.id.clone();
    publish(store, "agent", &id, serde_json::to_value(spec).unwrap()).await
}

async fn publish_model(
    store: &InMemoryVersionedRegistryStore,
    id: &str,
    provider_id: &str,
) -> PinnedRegistryEntry {
    let spec = ModelBindingSpec::new(id, provider_id, "upstream");
    publish(store, "model", id, serde_json::to_value(spec).unwrap()).await
}

async fn publish_provider(store: &InMemoryVersionedRegistryStore, id: &str) -> PinnedRegistryEntry {
    let spec = ProviderSpec {
        id: id.to_string(),
        adapter: "openai".to_string(),
        ..Default::default()
    };
    publish(store, "provider", id, serde_json::to_value(spec).unwrap()).await
}

async fn publish(
    store: &InMemoryVersionedRegistryStore,
    kind: &str,
    id: &str,
    value: Value,
) -> PinnedRegistryEntry {
    let outcome = store
        .publish_resource("default", kind, id, value, 1, json!({}))
        .await
        .unwrap();
    let record = match outcome {
        PublishOutcome::Created(record) | PublishOutcome::Noop(record) => record,
    };
    PinnedRegistryEntry {
        kind: kind.to_string(),
        id: id.to_string(),
        version: record.version,
        content_hash: record.content_hash,
    }
}

fn agent<'a>(id: &str, model_id: &str, delegates: impl IntoIterator<Item = &'a str>) -> AgentSpec {
    AgentSpec {
        id: id.to_string(),
        model_id: model_id.to_string(),
        system_prompt: "system".to_string(),
        delegates: delegates.into_iter().map(str::to_string).collect(),
        ..Default::default()
    }
}

fn refs<'a>(entries: impl IntoIterator<Item = &'a PinnedRegistryEntry>) -> Vec<VersionRef> {
    entries
        .into_iter()
        .map(|entry| VersionRef {
            kind: entry.kind.clone(),
            id: entry.id.clone(),
            version: entry.version,
        })
        .collect()
}
