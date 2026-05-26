use std::sync::Arc;

use awaken_contract::contract::versioned_registry::PublishOutcome;
use awaken_contract::{
    AgentSpec, ModelPoolSpec, ModelSpec, PinnedRegistryEntry, ProviderSpec,
    RegistryGraphValidationError, RegistryGraphValidationRequest, RegistryGraphValidator,
    StandardRegistryGraphValidator, VersionRef, VersionSelector, VersionedRegistryStore,
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
async fn validates_agent_referencing_model_pool_graph() {
    let store = InMemoryVersionedRegistryStore::new();
    let provider = publish_provider(&store, "provider-1").await;
    let m0 = publish_model(&store, "m0", "provider-1").await;
    let m1 = publish_model(&store, "m1", "provider-1").await;
    let pool = publish_model_pool(&store, "pool-1", ["m0", "m1"]).await;
    let root = publish_agent(&store, agent("root", "pool-1", [])).await;
    store
        .create_publication(
            "default",
            "pub-1",
            refs([&provider, &m0, &m1, &pool, &root]),
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

    // agent → pool → (m0, m1) → provider must all be reachable.
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.kind == "model_pool" && e.id == "pool-1")
    );
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.kind == "model" && e.id == "m0")
    );
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.kind == "model" && e.id == "m1")
    );
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.kind == "provider" && e.id == "provider-1")
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

#[tokio::test]
async fn manifest_validation_rejects_model_and_pool_id_collision() {
    // Runtime resolution returns AmbiguousModelReference when an id names both
    // a model and a pool. Durable manifest validation must reject the same
    // collision rather than silently preferring the model, or graph validation
    // would pass for a reference the runtime resolver later refuses.
    let store = InMemoryVersionedRegistryStore::new();
    let provider = publish_provider(&store, "provider-1").await;
    let member = publish_model(&store, "m0", "provider-1").await;
    let model_x = publish_model(&store, "x", "provider-1").await;
    let pool_x = publish_model_pool(&store, "x", ["m0"]).await;
    let root = publish_agent(&store, agent("root", "x", [])).await;
    let manifest = awaken_contract::PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: vec![root, model_x, pool_x, member, provider],
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
        RegistryGraphValidationError::InvalidReference { kind, id, reason }
            if kind == "model"
                && id == "x"
                && reason == "id resolves to both a model and a model pool"
    ));
}

#[tokio::test]
async fn exact_expansion_rejects_model_and_pool_id_collision() {
    // Exact expansion resolves transitive references against the store's
    // current pointer; an id that is both a model and a pool must be rejected
    // there too, matching the runtime resolver instead of silently expanding
    // to the model.
    let store = InMemoryVersionedRegistryStore::new();
    let _provider = publish_provider(&store, "provider-1").await;
    let _member = publish_model(&store, "m0", "provider-1").await;
    let _model_x = publish_model(&store, "x", "provider-1").await;
    let _pool_x = publish_model_pool(&store, "x", ["m0"]).await;
    let root = publish_agent(&store, agent("root", "x", [])).await;

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let error = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::Exact {
                scope_id: "default".to_string(),
                kind: "agent".to_string(),
                id: "root".to_string(),
                version: root.version,
            },
            reference_policy: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        RegistryGraphValidationError::InvalidReference { kind, id, .. }
            if kind == "model" && id == "x"
    ));
}

#[tokio::test]
async fn exact_expansion_resolves_model_when_no_pool_collision() {
    // The common case: an id names only a model, with no same-named pool. The
    // ambiguity check probes the pool kind too; that soft probe must return
    // "absent" (Ok(None)), not a MissingResource error, so expansion succeeds.
    let store = InMemoryVersionedRegistryStore::new();
    let _provider = publish_provider(&store, "provider-1").await;
    let _model = publish_model(&store, "model-1", "provider-1").await;
    let root = publish_agent(&store, agent("root", "model-1", [])).await;

    let validator = StandardRegistryGraphValidator::new(Arc::new(store));
    let report = validator
        .validate(RegistryGraphValidationRequest {
            root: VersionSelector::Exact {
                scope_id: "default".to_string(),
                kind: "agent".to_string(),
                id: "root".to_string(),
                version: root.version,
            },
            reference_policy: Default::default(),
        })
        .await
        .expect("agent referencing a model with no same-id pool must resolve");

    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.kind == "model" && entry.id == "model-1")
    );
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
    let spec = ModelSpec::new(id, provider_id, "upstream");
    publish(store, "model", id, serde_json::to_value(spec).unwrap()).await
}

async fn publish_model_pool<'a>(
    store: &InMemoryVersionedRegistryStore,
    id: &str,
    members: impl IntoIterator<Item = &'a str>,
) -> PinnedRegistryEntry {
    let spec = ModelPoolSpec::new(id, members);
    publish(store, "model_pool", id, serde_json::to_value(spec).unwrap()).await
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
