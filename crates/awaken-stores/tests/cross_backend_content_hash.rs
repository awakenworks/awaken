//! ADR-0035 D3 conformance: every backend must compute the same
//! `content_hash` for canonical-equivalent values. The hash is the
//! cross-backend stability guarantee that pinned manifests rely on, so
//! drift between backends would silently invalidate every Postgres-vs-
//! memory replay or audit comparison.

#![cfg(feature = "file")]

use awaken_contract::contract::versioned_registry::{
    PublishOutcome, VersionedRegistryStore, registry_content_hash,
};
use awaken_stores::{FileVersionedRegistryStore, InMemoryVersionedRegistryStore};
use serde_json::{Value, json};
use tempfile::TempDir;

async fn publish_hash<S: VersionedRegistryStore>(
    store: &S,
    kind: &str,
    id: &str,
    value: Value,
    schema: u32,
) -> String {
    let outcome = store
        .publish_resource("default", kind, id, value, schema, json!({}))
        .await
        .unwrap();
    match outcome {
        PublishOutcome::Created(record) => record.content_hash,
        PublishOutcome::Noop(record) => record.content_hash,
    }
}

#[tokio::test]
async fn backends_compute_identical_content_hash_for_canonical_equivalent_values() {
    let fixtures: Vec<(&str, &str, Value, u32)> = vec![
        (
            "agent",
            "agent-1",
            json!({"name": "ada", "model": "m1", "delegates": ["b", "a"]}),
            1,
        ),
        (
            "agent",
            "agent-2",
            json!({"model": "m1", "delegates": ["b", "a"], "name": "ada"}),
            1,
        ),
        (
            "provider",
            "p1",
            json!({"id": "p1", "adapter": "openai", "models": []}),
            2,
        ),
    ];

    let mem = InMemoryVersionedRegistryStore::new();
    let tmp = TempDir::new().unwrap();
    let file = FileVersionedRegistryStore::new(tmp.path());

    for (kind, id, value, schema) in &fixtures {
        let (expected, _) = registry_content_hash(*schema, value).unwrap();
        let mem_hash = publish_hash(&mem, kind, id, value.clone(), *schema).await;
        let file_hash = publish_hash(&file, kind, id, value.clone(), *schema).await;
        assert_eq!(
            mem_hash, expected,
            "memory backend hash for {kind}/{id} drifted from contract helper"
        );
        assert_eq!(
            file_hash, expected,
            "file backend hash for {kind}/{id} drifted from contract helper"
        );
        assert_eq!(
            mem_hash, file_hash,
            "memory vs file content_hash drift on {kind}/{id}"
        );
    }
}

#[tokio::test]
async fn canonical_object_key_reorder_produces_same_hash_across_backends() {
    // Two equivalent payloads differing only in JSON key order must
    // produce the same hash in every backend — otherwise `publish_resource`
    // would create spurious new versions when a writer re-serializes
    // through a different JSON encoder.
    let mem = InMemoryVersionedRegistryStore::new();
    let tmp = TempDir::new().unwrap();
    let file = FileVersionedRegistryStore::new(tmp.path());

    let value_a = json!({"alpha": 1, "beta": 2, "nested": {"y": 9, "x": 8}});
    let value_b = json!({"nested": {"x": 8, "y": 9}, "beta": 2, "alpha": 1});

    let mem_a = publish_hash(&mem, "tool", "tool-a", value_a.clone(), 1).await;
    let mem_b = publish_hash(&mem, "tool", "tool-b", value_b.clone(), 1).await;
    let file_a = publish_hash(&file, "tool", "tool-a", value_a, 1).await;
    let file_b = publish_hash(&file, "tool", "tool-b", value_b, 1).await;

    assert_eq!(mem_a, mem_b, "memory backend key-order canonical drift");
    assert_eq!(file_a, file_b, "file backend key-order canonical drift");
    assert_eq!(mem_a, file_a, "memory vs file key-order canonical drift");
}
