use awaken_server_contract::contract::versioned_registry::{
    ConfigRevisionRef, PublishOutcome, RegistryResourcePublish, VersionRef, VersionedRegistryError,
    VersionedRegistryStore,
};
use awaken_stores::InMemoryVersionedRegistryStore;
use serde_json::json;

#[tokio::test]
async fn publish_creates_monotonic_versions_and_noops_on_current_hash() {
    let store = InMemoryVersionedRegistryStore::new();

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

    let noop = store
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
    let noop = match noop {
        PublishOutcome::Noop(record) => record,
        PublishOutcome::Created(_) => panic!("canonical-equivalent current value must no-op"),
    };
    assert_eq!(noop.version, 1);
    assert_eq!(noop.content_hash, first.content_hash);

    let second = store
        .publish_resource(
            "default",
            "agent",
            "agent-1",
            json!({"name": "agent", "model": "m2"}),
            1,
            json!({"source": "changed"}),
        )
        .await
        .unwrap();
    let second = match second {
        PublishOutcome::Created(record) => record,
        PublishOutcome::Noop(_) => panic!("changed value must create"),
    };
    assert_eq!(second.version, 2);

    let versions = store
        .list_versions("default", "agent", "agent-1")
        .await
        .unwrap();
    assert_eq!(
        versions
            .iter()
            .map(|record| record.version)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        store
            .current("default", "agent", "agent-1")
            .await
            .unwrap()
            .unwrap()
            .version,
        2
    );
}

#[tokio::test]
async fn rollback_copies_historical_value_as_next_version() {
    let store = InMemoryVersionedRegistryStore::new();
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
    assert_eq!(
        store
            .current("default", "tool", "search")
            .await
            .unwrap()
            .unwrap()
            .version,
        3
    );

    let repeated = store
        .rollback_resource("default", "tool", "search", 3, json!({"restored_from": 3}))
        .await
        .unwrap();
    assert_eq!(repeated.version, 4);
}

#[tokio::test]
async fn retention_plan_protects_current_publications_pinned_and_young_versions() {
    use awaken_server_contract::VersionRef;
    use awaken_server_contract::contract::versioned_registry::{
        RegistryRetentionPolicy, VersionedRegistryRetention,
    };

    let store = InMemoryVersionedRegistryStore::new();
    // Publish 5 versions of a tool: v1..v5
    for n in 1..=5 {
        store
            .publish_resource("default", "tool", "search", json!({"v": n}), 1, json!({}))
            .await
            .unwrap();
    }
    // v5 is current. Snapshot a publication referencing v3 to protect it.
    store
        .create_publication(
            "default",
            "pub-protect",
            vec![VersionRef {
                kind: "tool".to_string(),
                id: "search".to_string(),
                version: 3,
            }],
            vec![],
            None,
            json!({}),
        )
        .await
        .unwrap();
    // Externally pin v2 via protected_versions (mimics run resolution protection).
    let policy = RegistryRetentionPolicy {
        keep_last_versions: Some(1),
        keep_younger_than_ms: None,
        protected_versions: vec![VersionRef {
            kind: "tool".to_string(),
            id: "search".to_string(),
            version: 2,
        }],
    };
    let plan = store
        .purge_eligible_versions("default", 0, policy, true)
        .await
        .unwrap();
    // Protected: v5 (current), v3 (publication), v2 (protected_versions),
    // v4 (keep_last_versions=1). Eligible: just v1.
    let eligible_versions: Vec<u64> = plan.iter().map(|v| v.version).collect();
    assert_eq!(eligible_versions, vec![1]);

    // dry_run did not delete; the v1 record is still readable.
    assert!(
        store
            .get("default", "tool", "search", 1)
            .await
            .unwrap()
            .is_some()
    );

    let plan = store
        .purge_eligible_versions(
            "default",
            0,
            RegistryRetentionPolicy {
                keep_last_versions: Some(1),
                keep_younger_than_ms: None,
                protected_versions: vec![],
            },
            false,
        )
        .await
        .unwrap();
    // Now v1 and v2 are eligible (no external pin). v3 still protected by publication, v4 by keep_last, v5 current.
    let eligible_versions: Vec<u64> = plan.iter().map(|v| v.version).collect();
    assert_eq!(eligible_versions, vec![1, 2]);
    // Confirm physical purge happened.
    assert!(
        store
            .get("default", "tool", "search", 1)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get("default", "tool", "search", 2)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get("default", "tool", "search", 3)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn create_publication_returns_entries_sorted_by_kind_and_id() {
    // ADR-0035 D6/D11: backend-agnostic `RegistryPublication.entries`
    // order so callers/projectors hashing the publication get a stable
    // value across memory / file / postgres.
    let store = InMemoryVersionedRegistryStore::new();
    store
        .publish_resource(
            "default",
            "provider",
            "p1",
            json!({"id":"p1"}),
            1,
            json!({}),
        )
        .await
        .unwrap();
    let model = store
        .publish_resource("default", "model", "m1", json!({"id":"m1"}), 1, json!({}))
        .await
        .unwrap();
    let agent = store
        .publish_resource("default", "agent", "a1", json!({"id":"a1"}), 1, json!({}))
        .await
        .unwrap();

    let entries = vec![
        version_ref(&model),
        version_ref(&agent),
        version_ref_named("provider", "p1", 1),
    ];

    let publication = store
        .create_publication("default", "pub-sort", entries, vec![], None, json!({}))
        .await
        .unwrap();

    let kinds: Vec<&str> = publication
        .entries
        .iter()
        .map(|entry| entry.kind.as_str())
        .collect();
    assert_eq!(kinds, vec!["agent", "model", "provider"]);
}

fn version_ref(
    outcome: &awaken_server_contract::contract::versioned_registry::PublishOutcome<
        serde_json::Value,
    >,
) -> awaken_server_contract::VersionRef {
    let record = match outcome {
        awaken_server_contract::contract::versioned_registry::PublishOutcome::Created(record)
        | awaken_server_contract::contract::versioned_registry::PublishOutcome::Noop(record) => {
            record
        }
    };
    awaken_server_contract::VersionRef {
        kind: record.kind.clone(),
        id: record.id.clone(),
        version: record.version,
    }
}

fn version_ref_named(kind: &str, id: &str, version: u64) -> awaken_server_contract::VersionRef {
    awaken_server_contract::VersionRef {
        kind: kind.to_string(),
        id: id.to_string(),
        version,
    }
}

#[tokio::test]
async fn rollback_injects_restored_from_metadata_and_rejects_mismatch() {
    // ADR-0035 D4: rollback metadata must carry `restored_from = <to_version>`.
    let store = InMemoryVersionedRegistryStore::new();
    store
        .publish_resource("default", "tool", "search", json!({"v": 1}), 1, json!({}))
        .await
        .unwrap();
    store
        .publish_resource("default", "tool", "search", json!({"v": 2}), 1, json!({}))
        .await
        .unwrap();

    // No restored_from supplied — store injects it automatically.
    let rolled_back = store
        .rollback_resource("default", "tool", "search", 1, json!({"reason": "regress"}))
        .await
        .unwrap();
    assert_eq!(rolled_back.metadata["restored_from"], json!(1));
    assert_eq!(rolled_back.metadata["reason"], "regress");

    // Supplying a mismatched restored_from must be rejected.
    let err = store
        .rollback_resource("default", "tool", "search", 1, json!({"restored_from": 9}))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        awaken_server_contract::contract::versioned_registry::VersionedRegistryError::InvalidRequest(_)
    ));
}

#[tokio::test]
async fn archive_rejects_new_publishes_but_keeps_historical_versions_readable() {
    let store = InMemoryVersionedRegistryStore::new();
    store
        .publish_resource("default", "model", "m1", json!({"id": "m1"}), 1, json!({}))
        .await
        .unwrap();

    store
        .archive_resource("default", "model", "m1")
        .await
        .unwrap();
    let state = store
        .resource_state("default", "model", "m1")
        .await
        .unwrap()
        .unwrap();
    assert!(state.archived_at_ms.is_some());
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

    store
        .unarchive_resource("default", "model", "m1")
        .await
        .unwrap();
    assert!(
        store
            .resource_state("default", "model", "m1")
            .await
            .unwrap()
            .unwrap()
            .archived_at_ms
            .is_none()
    );
}

#[tokio::test]
async fn scopes_are_isolated() {
    let store = InMemoryVersionedRegistryStore::new();
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
    let store = InMemoryVersionedRegistryStore::new();
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
async fn atomic_publish_creates_versions_and_publication_together() {
    let store = InMemoryVersionedRegistryStore::new();

    let publication = store
        .publish_resources_and_create_publication(
            "default",
            "pub-atomic-1",
            vec![
                RegistryResourcePublish {
                    kind: "agent".to_string(),
                    id: "agent-1".to_string(),
                    value: json!({"name": "agent", "model": "m1"}),
                    value_schema_version: 1,
                    metadata: json!({"kind": "agent"}),
                },
                RegistryResourcePublish {
                    kind: "model".to_string(),
                    id: "m1".to_string(),
                    value: json!({"provider": "p1"}),
                    value_schema_version: 1,
                    metadata: json!({"kind": "model"}),
                },
            ],
            Vec::new(),
            Some("tester".to_string()),
            json!({"reason": "atomic"}),
        )
        .await
        .unwrap();

    assert_eq!(publication.snapshot_version, 1);
    assert_eq!(publication.entries.len(), 2);
    assert_eq!(
        store
            .current("default", "agent", "agent-1")
            .await
            .unwrap()
            .unwrap()
            .version,
        1
    );
    assert_eq!(
        store
            .latest_publication("default")
            .await
            .unwrap()
            .unwrap()
            .publication_id,
        "pub-atomic-1"
    );
}

#[tokio::test]
async fn atomic_publish_rejects_duplicate_publication_without_new_versions() {
    let store = InMemoryVersionedRegistryStore::new();

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
async fn rollback_publication_restores_previous_graph_atomically() {
    let store = InMemoryVersionedRegistryStore::new();
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
                    metadata: json!({"revision": 7}),
                },
                RegistryResourcePublish {
                    kind: "model".to_string(),
                    id: "m1".to_string(),
                    value: json!({"provider": "p1"}),
                    value_schema_version: 1,
                    metadata: json!({"revision": 7}),
                },
            ],
            source_revisions.clone(),
            Some("publisher".to_string()),
            json!({"reason": "initial"}),
        )
        .await
        .unwrap();
    let first_agent_hash = store
        .get("default", "agent", "root", 1)
        .await
        .unwrap()
        .unwrap()
        .content_hash;

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
                    metadata: json!({"revision": 8}),
                },
                RegistryResourcePublish {
                    kind: "model".to_string(),
                    id: "m1".to_string(),
                    value: json!({"provider": "p2"}),
                    value_schema_version: 1,
                    metadata: json!({"revision": 8}),
                },
            ],
            Vec::new(),
            Some("publisher".to_string()),
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
            json!({"reason": "restore v1"}),
        )
        .await
        .unwrap();

    assert_eq!(rollback.snapshot_version, 3);
    assert_eq!(rollback.source_config_revisions, source_revisions);
    assert_eq!(rollback.created_by.as_deref(), Some("operator"));
    assert_eq!(rollback.entries.len(), 2);
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
    assert_eq!(
        store
            .current("default", "model", "m1")
            .await
            .unwrap()
            .unwrap()
            .value,
        json!({"provider": "p1"})
    );
    let manifest = store
        .pinned_manifest_for_publication("default", rollback.snapshot_version)
        .await
        .unwrap()
        .unwrap();
    assert!(manifest.entries.iter().any(|entry| {
        entry.kind == "agent"
            && entry.id == "root"
            && entry.version == 3
            && entry.content_hash == first_agent_hash
    }));

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
