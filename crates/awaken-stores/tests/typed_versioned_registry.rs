use std::sync::Arc;

use awaken_server_contract::{
    PublishOutcome, TypedVersionedRegistry, VersionedRegistryError, VersionedRegistryStore,
};
use awaken_stores::InMemoryVersionedRegistryStore;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TypedSpec {
    id: String,
    label: String,
}

fn typed_spec(label: &str) -> TypedSpec {
    TypedSpec {
        id: "agent-1".to_string(),
        label: label.to_string(),
    }
}

#[tokio::test]
async fn typed_registry_publishes_reads_and_rolls_back_typed_records() {
    let store: Arc<dyn VersionedRegistryStore> = Arc::new(InMemoryVersionedRegistryStore::new());
    let registry = TypedVersionedRegistry::<TypedSpec>::new(Arc::clone(&store), "scope-a", "agent");

    let outcome = registry
        .publish("agent-1", typed_spec("v1"), 1, json!({"source": "test"}))
        .await
        .expect("publish typed record");
    let first = match outcome {
        PublishOutcome::Created(record) => record,
        PublishOutcome::Noop(_) => panic!("first publish must create a version"),
    };
    assert_eq!(first.kind, "agent");
    assert_eq!(first.id, "agent-1");
    assert_eq!(first.version, 1);
    assert_eq!(first.value, typed_spec("v1"));

    let current = registry
        .current("agent-1")
        .await
        .expect("read current")
        .expect("current record");
    assert_eq!(current.value, typed_spec("v1"));

    let unchanged = registry
        .publish(
            "agent-1",
            typed_spec("v1"),
            1,
            json!({"metadata": "ignored-by-hash"}),
        )
        .await
        .expect("re-publish unchanged record");
    let unchanged = match unchanged {
        PublishOutcome::Noop(record) => record,
        PublishOutcome::Created(_) => panic!("unchanged publish must be a content-hash noop"),
    };
    assert_eq!(unchanged.version, 1);

    let changed = registry
        .publish("agent-1", typed_spec("v2"), 1, json!({}))
        .await
        .expect("publish changed record");
    let changed = match changed {
        PublishOutcome::Created(record) => record,
        PublishOutcome::Noop(_) => panic!("changed publish must create a new version"),
    };
    assert_eq!(changed.version, 2);
    assert_eq!(changed.value, typed_spec("v2"));

    let versions = registry
        .list_versions("agent-1")
        .await
        .expect("list versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].value, typed_spec("v1"));
    assert_eq!(versions[1].value, typed_spec("v2"));

    let state = registry
        .resource_state("agent-1")
        .await
        .expect("read resource state")
        .expect("resource state");
    assert_eq!(state.scope_id, "scope-a");
    assert_eq!(state.kind, "agent");
    assert_eq!(state.current_version, Some(2));

    registry.archive("agent-1").await.expect("archive resource");
    let publish_err = registry
        .publish("agent-1", typed_spec("v3"), 1, json!({}))
        .await
        .expect_err("archived resources reject publish");
    assert!(matches!(
        publish_err,
        VersionedRegistryError::Archived { kind, id } if kind == "agent" && id == "agent-1"
    ));

    registry
        .unarchive("agent-1")
        .await
        .expect("unarchive resource");
    let rolled_back = registry
        .rollback("agent-1", 1, json!({"restored_from": 1}))
        .await
        .expect("rollback by copy");
    assert_eq!(rolled_back.version, 3);
    assert_eq!(rolled_back.value, typed_spec("v1"));

    assert_eq!(
        registry.version_ref("agent-1", 3).kind,
        "agent",
        "typed wrapper must stamp version refs with its bound kind"
    );
}

#[tokio::test]
async fn typed_registry_surfaces_deserialization_errors() {
    let store: Arc<dyn VersionedRegistryStore> = Arc::new(InMemoryVersionedRegistryStore::new());
    store
        .publish_resource(
            "scope-a",
            "agent",
            "bad",
            json!({"id": "bad", "label": 7}),
            1,
            json!({}),
        )
        .await
        .expect("publish malformed typed value");

    let registry = TypedVersionedRegistry::<TypedSpec>::new(Arc::clone(&store), "scope-a", "agent");
    let error = registry
        .current("bad")
        .await
        .expect_err("malformed typed value must fail to decode");
    assert!(matches!(error, VersionedRegistryError::Serialization(_)));
}
