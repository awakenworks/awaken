#![cfg(feature = "file")]

use awaken_server_contract::contract::versioned_registry::{
    ConfigRevisionRef, PublishOutcome, RegistryResourcePublish, VersionRef, VersionedRegistryError,
    VersionedRegistryStore,
};
use awaken_stores::FileVersionedRegistryStore;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn publish_persists_across_store_instances_and_noops_on_current_hash() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());

    let first = store
        .publish_resource(
            "default",
            "agent",
            "agent-1",
            json!({"name": "agent", "model": "m1"}),
            1,
            json!({"source": "test"}),
        )
        .await
        .unwrap();
    let first = match first {
        PublishOutcome::Created(record) => record,
        PublishOutcome::Noop(_) => panic!("first publish must create"),
    };
    assert_eq!(first.version, 1);

    let reopened = FileVersionedRegistryStore::new(tmp.path());
    let current = reopened
        .current("default", "agent", "agent-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.version, 1);
    assert_eq!(current.content_hash, first.content_hash);

    let noop = reopened
        .publish_resource(
            "default",
            "agent",
            "agent-1",
            json!({"model": "m1", "name": "agent"}),
            1,
            json!({"source": "same"}),
        )
        .await
        .unwrap();
    assert!(matches!(noop, PublishOutcome::Noop(record) if record.version == 1));
}

#[tokio::test]
async fn rollback_copies_historical_value_as_next_file_version() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());
    store
        .publish_resource(
            "default",
            "tool",
            "search",
            json!({"timeout": 1}),
            1,
            json!({}),
        )
        .await
        .unwrap();
    store
        .publish_resource(
            "default",
            "tool",
            "search",
            json!({"timeout": 2}),
            1,
            json!({}),
        )
        .await
        .unwrap();

    let rolled_back = store
        .rollback_resource("default", "tool", "search", 1, json!({"restored_from": 1}))
        .await
        .unwrap();

    assert_eq!(rolled_back.version, 3);
    assert_eq!(rolled_back.value, json!({"timeout": 1}));
}

#[tokio::test]
async fn archive_rejects_file_publishes_but_keeps_history_readable() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());
    store
        .publish_resource("default", "model", "m1", json!({"id": "m1"}), 1, json!({}))
        .await
        .unwrap();

    store
        .archive_resource("default", "model", "m1")
        .await
        .unwrap();
    assert!(
        store
            .get("default", "model", "m1", 1)
            .await
            .unwrap()
            .is_some()
    );

    let error = store
        .publish_resource(
            "default",
            "model",
            "m1",
            json!({"id": "m1-v2"}),
            1,
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        VersionedRegistryError::Archived { kind, id }
        if kind == "model" && id == "m1"
    ));
}

#[tokio::test]
async fn file_registry_scopes_are_isolated() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());
    store
        .publish_resource(
            "scope-a",
            "provider",
            "p",
            json!({"name": "a"}),
            1,
            json!({}),
        )
        .await
        .unwrap();

    assert!(
        store
            .current("scope-b", "provider", "p")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn create_publication_tracks_latest_and_rejects_archived_entries() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());
    store
        .publish_resource(
            "default",
            "agent",
            "agent-1",
            json!({"name": "agent", "model": "m1"}),
            1,
            json!({}),
        )
        .await
        .unwrap();
    store
        .publish_resource(
            "default",
            "model",
            "m1",
            json!({"provider": "p1"}),
            1,
            json!({}),
        )
        .await
        .unwrap();

    let entries = vec![
        VersionRef {
            kind: "agent".to_string(),
            id: "agent-1".to_string(),
            version: 1,
        },
        VersionRef {
            kind: "model".to_string(),
            id: "m1".to_string(),
            version: 1,
        },
    ];
    let publication = store
        .create_publication(
            "default",
            "pub-1",
            entries.clone(),
            Vec::new(),
            Some("tester".to_string()),
            json!({"reason": "test"}),
        )
        .await
        .unwrap();
    assert_eq!(publication.snapshot_version, 1);
    assert_eq!(publication.entries, entries);

    assert_eq!(
        store
            .latest_publication("default")
            .await
            .unwrap()
            .unwrap()
            .publication_id,
        "pub-1"
    );
    assert_eq!(
        store
            .get_publication("default", 1)
            .await
            .unwrap()
            .unwrap()
            .created_by
            .as_deref(),
        Some("tester")
    );

    let agent_hash = store
        .get("default", "agent", "agent-1", 1)
        .await
        .unwrap()
        .unwrap()
        .content_hash;
    let manifest = store
        .pinned_manifest_for_publication("default", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(manifest.publication_id.as_deref(), Some("pub-1"));
    assert_eq!(manifest.registry_snapshot_version, Some(1));
    assert!(manifest.entries.iter().any(|entry| {
        entry.kind == "agent"
            && entry.id == "agent-1"
            && entry.version == 1
            && entry.content_hash == agent_hash
    }));
    assert_eq!(
        store
            .latest_pinned_manifest("default")
            .await
            .unwrap()
            .unwrap()
            .publication_id
            .as_deref(),
        Some("pub-1")
    );

    let duplicate = store
        .create_publication(
            "default",
            "pub-1",
            entries.clone(),
            Vec::new(),
            None,
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate,
        VersionedRegistryError::AlreadyExists(_)
    ));

    store
        .archive_resource("default", "model", "m1")
        .await
        .unwrap();
    let archived = store
        .create_publication("default", "pub-2", entries, Vec::new(), None, json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        archived,
        VersionedRegistryError::Archived { kind, id } if kind == "model" && id == "m1"
    ));
}

#[tokio::test]
async fn atomic_file_publish_rejects_duplicate_publication_without_new_versions() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());

    store
        .publish_resources_and_create_publication(
            "default",
            "pub-atomic-1",
            vec![RegistryResourcePublish {
                kind: "agent".to_string(),
                id: "agent-1".to_string(),
                value: json!({"name": "agent", "model": "m1"}),
                value_schema_version: 1,
                metadata: json!({}),
            }],
            Vec::new(),
            None,
            json!({}),
        )
        .await
        .unwrap();

    let duplicate = store
        .publish_resources_and_create_publication(
            "default",
            "pub-atomic-1",
            vec![RegistryResourcePublish {
                kind: "agent".to_string(),
                id: "agent-1".to_string(),
                value: json!({"name": "agent", "model": "m2"}),
                value_schema_version: 1,
                metadata: json!({}),
            }],
            Vec::new(),
            None,
            json!({}),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        duplicate,
        VersionedRegistryError::AlreadyExists(_)
    ));
    let versions = store
        .list_versions("default", "agent", "agent-1")
        .await
        .unwrap();
    assert_eq!(
        versions
            .iter()
            .map(|record| record.version)
            .collect::<Vec<_>>(),
        vec![1],
        "duplicate publication id must abort before publishing resource version 2"
    );
}

#[tokio::test]
async fn rollback_file_publication_restores_previous_graph_atomically() {
    let tmp = TempDir::new().unwrap();
    let store = FileVersionedRegistryStore::new(tmp.path());
    let source_revisions = vec![ConfigRevisionRef {
        namespace: "agents".to_string(),
        id: "root".to_string(),
        revision: 7,
    }];

    let first = store
        .publish_resources_and_create_publication(
            "default",
            "pub-v1",
            vec![
                RegistryResourcePublish {
                    kind: "agent".to_string(),
                    id: "root".to_string(),
                    value: json!({"name": "root", "model": "m1"}),
                    value_schema_version: 1,
                    metadata: json!({}),
                },
                RegistryResourcePublish {
                    kind: "model".to_string(),
                    id: "m1".to_string(),
                    value: json!({"provider": "p1"}),
                    value_schema_version: 1,
                    metadata: json!({}),
                },
            ],
            source_revisions.clone(),
            None,
            json!({"reason": "initial"}),
        )
        .await
        .unwrap();

    store
        .publish_resources_and_create_publication(
            "default",
            "pub-v2",
            vec![
                RegistryResourcePublish {
                    kind: "agent".to_string(),
                    id: "root".to_string(),
                    value: json!({"name": "root", "model": "m2"}),
                    value_schema_version: 1,
                    metadata: json!({}),
                },
                RegistryResourcePublish {
                    kind: "model".to_string(),
                    id: "m1".to_string(),
                    value: json!({"provider": "p2"}),
                    value_schema_version: 1,
                    metadata: json!({}),
                },
            ],
            Vec::new(),
            None,
            json!({"reason": "changed"}),
        )
        .await
        .unwrap();

    let rollback = store
        .rollback_publication(
            "default",
            first.snapshot_version,
            "pub-rollback",
            Some("operator".to_string()),
            json!({"reason": "restore"}),
        )
        .await
        .unwrap();

    assert_eq!(rollback.snapshot_version, 3);
    assert_eq!(rollback.source_config_revisions, source_revisions);
    assert!(rollback.entries.iter().all(|entry| entry.version == 3));
    assert_eq!(
        store
            .current("default", "agent", "root")
            .await
            .unwrap()
            .unwrap()
            .value,
        json!({"name": "root", "model": "m1"})
    );

    let duplicate = store
        .rollback_publication(
            "default",
            first.snapshot_version,
            "pub-rollback",
            None,
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate,
        VersionedRegistryError::AlreadyExists(_)
    ));
    let versions = store
        .list_versions("default", "agent", "root")
        .await
        .unwrap();
    assert_eq!(
        versions
            .iter()
            .map(|record| record.version)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "duplicate rollback publication id must abort before creating version 4"
    );
}
