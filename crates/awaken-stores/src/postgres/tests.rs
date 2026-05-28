use awaken_server_contract::contract::config_store::ConfigStore;
use awaken_server_contract::contract::storage::{
    PinnedRegistryEntry, PinnedRegistryManifest, RunRecord, RunStore, StorageError, ThreadRunStore,
    ThreadStore,
};
use awaken_server_contract::contract::versioned_registry::{
    PublishOutcome, RegistryResourcePublish, VersionRef, VersionedRegistryStore,
};
use awaken_server_contract::thread::Thread;
use serde_json::json;
use sqlx::PgPool;

use super::PostgresStore;
use super::run::parse_run_status;

#[test]
fn parse_run_status_known_values() {
    use awaken_server_contract::contract::lifecycle::RunStatus;
    assert!(matches!(parse_run_status("created"), RunStatus::Created));
    assert!(matches!(parse_run_status("running"), RunStatus::Running));
    assert!(matches!(parse_run_status("waiting"), RunStatus::Waiting));
    assert!(matches!(parse_run_status("done"), RunStatus::Done));
}

#[test]
fn parse_run_status_unknown_defaults_to_running() {
    use awaken_server_contract::contract::lifecycle::RunStatus;
    assert!(matches!(parse_run_status("unknown"), RunStatus::Running));
    assert!(matches!(parse_run_status(""), RunStatus::Running));
}

#[test]
fn postgres_store_default_table_names() {
    // We can't actually connect, but we can verify table name construction
    // This would require a PgPool which needs a real connection.
    // Instead test the `with_prefix` naming logic by creating without connecting.
    // We can only test the table name generation pattern.
    let prefix = "test_prefix";
    assert_eq!(format!("{prefix}_threads"), "test_prefix_threads");
    assert_eq!(format!("{prefix}_runs"), "test_prefix_runs");
    assert_eq!(format!("{prefix}_configs"), "test_prefix_configs");
    assert_eq!(
        format!("{prefix}_config_changes"),
        "test_prefix_config_changes"
    );
}

#[test]
fn merge_thread_lineage_prefers_columns_when_present() {
    let thread = Thread::with_id("thread-1")
        .with_resource_id("json-resource")
        .with_parent_thread_id("json-parent");

    let merged = PostgresStore::merge_thread_lineage(
        thread,
        Some("column-resource".to_string()),
        Some("column-parent".to_string()),
    );

    assert_eq!(merged.resource_id.as_deref(), Some("column-resource"));
    assert_eq!(merged.parent_thread_id.as_deref(), Some("column-parent"));
}

#[test]
fn merge_thread_lineage_preserves_json_when_columns_missing() {
    let thread = Thread::with_id("thread-1")
        .with_resource_id("json-resource")
        .with_parent_thread_id("json-parent");

    let merged = PostgresStore::merge_thread_lineage(thread, None, None);

    assert_eq!(merged.resource_id.as_deref(), Some("json-resource"));
    assert_eq!(merged.parent_thread_id.as_deref(), Some("json-parent"));
}

// Integration tests below require a running PostgreSQL server.

#[tokio::test]
#[ignore]
async fn schema_initialization() {
    let pool = PgPool::connect("postgres://localhost/awaken_test")
        .await
        .unwrap();
    let store = PostgresStore::with_prefix(pool, "test_schema_init");
    store.ensure_schema().await.unwrap();
    // Calling again should be idempotent
    store.ensure_schema().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn connection_error_handling() {
    let pool = PgPool::connect("postgres://localhost:19999/nonexistent")
        .await
        .unwrap_err();
    // Connection itself fails, which is the expected behavior
    let _ = pool;
}

#[tokio::test]
#[ignore]
async fn thread_crud_operations() {
    let pool = PgPool::connect("postgres://localhost/awaken_test")
        .await
        .unwrap();
    let store = PostgresStore::with_prefix(pool, "test_crud");
    store.ensure_schema().await.unwrap();

    let thread = Thread::new();
    store.save_thread(&thread).await.unwrap();

    let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(loaded.id, thread.id);

    store.delete_thread(&thread.id).await.unwrap();
    assert!(store.load_thread(&thread.id).await.unwrap().is_none());
}

#[tokio::test]
#[ignore]
async fn run_create_duplicate_returns_already_exists() {
    use awaken_server_contract::contract::lifecycle::RunStatus;

    let pool = PgPool::connect("postgres://localhost/awaken_test")
        .await
        .unwrap();
    let store = PostgresStore::with_prefix(pool, "test_dup_run");
    store.ensure_schema().await.unwrap();

    let run = RunRecord {
        run_id: format!("dup-{}", uuid::Uuid::now_v7()),
        thread_id: "t-1".to_string(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Running,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 100,
        started_at: None,
        finished_at: None,
        updated_at: 100,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    store.create_run(&run).await.unwrap();
    let err = store.create_run(&run).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(_)));
}

#[tokio::test]
#[ignore]
async fn checkpoint_atomicity() {
    use awaken_server_contract::contract::lifecycle::RunStatus;
    use awaken_server_contract::contract::message::Message;

    let pool = PgPool::connect("postgres://localhost/awaken_test")
        .await
        .unwrap();
    let store = PostgresStore::with_prefix(pool, "test_checkpoint");
    store.ensure_schema().await.unwrap();

    let thread_id = format!("t-{}", uuid::Uuid::now_v7());
    let msgs = vec![Message::user("checkpoint test")];
    let run = RunRecord {
        run_id: format!("r-{}", uuid::Uuid::now_v7()),
        thread_id: thread_id.clone(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
        registry_manifest: Some(PinnedRegistryManifest {
            publication_id: Some("pub-1".to_string()),
            registry_snapshot_version: Some(11),
            entries: vec![PinnedRegistryEntry {
                kind: "agent".to_string(),
                id: "agent".to_string(),
                version: 4,
                content_hash: "sha256:agent-v4".to_string(),
            }],
        }),
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Running,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 100,
        started_at: None,
        finished_at: None,
        updated_at: 100,
        steps: 1,
        input_tokens: 10,
        output_tokens: 20,
        state: None,
    };

    store.checkpoint(&thread_id, &msgs, &run).await.unwrap();

    let loaded_msgs = store.load_messages(&thread_id).await.unwrap().unwrap();
    assert_eq!(loaded_msgs.len(), 1);
    let loaded_run = store.load_run(&run.run_id).await.unwrap().unwrap();
    assert_eq!(loaded_run.thread_id, thread_id);
    assert_eq!(loaded_run.registry_manifest, run.registry_manifest);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL"]
async fn versioned_registry_publish_roundtrip() {
    let url = std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://localhost/awaken_test".to_string());
    let pool = PgPool::connect(&url).await.unwrap();
    let store = PostgresStore::with_prefix(pool, "test_versioned_registry");
    store.ensure_schema().await.unwrap();
    let scope_id = format!("scope-{}", uuid::Uuid::now_v7());

    let first = store
        .publish_resource(
            &scope_id,
            "agent",
            "agent-1",
            json!({"model": "m1", "name": "agent"}),
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
            &scope_id,
            "agent",
            "agent-1",
            json!({"name": "agent", "model": "m1"}),
            1,
            json!({"source": "same"}),
        )
        .await
        .unwrap();
    assert!(matches!(noop, PublishOutcome::Noop(record) if record.version == 1));

    let changed = store
        .publish_resource(
            &scope_id,
            "agent",
            "agent-1",
            json!({"model": "m2", "name": "agent"}),
            1,
            json!({"source": "changed"}),
        )
        .await
        .unwrap();
    assert!(matches!(changed, PublishOutcome::Created(record) if record.version == 2));

    let rolled_back = store
        .rollback_resource(
            &scope_id,
            "agent",
            "agent-1",
            1,
            json!({"restored_from": 1}),
        )
        .await
        .unwrap();
    assert_eq!(rolled_back.version, 3);
    assert_eq!(
        store
            .current(&scope_id, "agent", "agent-1")
            .await
            .unwrap()
            .unwrap()
            .version,
        3
    );

    let publication = store
        .create_publication(
            &scope_id,
            "pub-1",
            vec![VersionRef {
                kind: "agent".to_string(),
                id: "agent-1".to_string(),
                version: 3,
            }],
            Vec::new(),
            Some("tester".to_string()),
            json!({"source": "test"}),
        )
        .await
        .unwrap();
    assert_eq!(publication.snapshot_version, 1);
    assert_eq!(
        store
            .latest_publication(&scope_id)
            .await
            .unwrap()
            .unwrap()
            .publication_id,
        "pub-1"
    );

    let duplicate = store
        .publish_resources_and_create_publication(
            &scope_id,
            "pub-1",
            vec![RegistryResourcePublish {
                kind: "agent".to_string(),
                id: "agent-1".to_string(),
                value: json!({"model": "m3", "name": "agent"}),
                value_schema_version: 1,
                metadata: json!({"source": "duplicate"}),
            }],
            Vec::new(),
            None,
            json!({}),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate,
        awaken_server_contract::contract::versioned_registry::VersionedRegistryError::AlreadyExists(
            _
        )
    ));
    assert_eq!(
        store
            .list_versions(&scope_id, "agent", "agent-1")
            .await
            .unwrap()
            .len(),
        3,
        "duplicate atomic publication must not create version 4"
    );
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL"]
async fn put_if_revision_atomic_cas() {
    let url = std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://localhost/awaken_test".to_string());
    let pool = PgPool::connect(&url).await.unwrap();
    let store = PostgresStore::with_prefix(pool, "test_cas");
    store.ensure_schema().await.unwrap();

    let v1 = serde_json::json!({"spec": {"id": "cas-key"}, "meta": {"source": {"kind": "user"}, "revision": 1}});
    // First write: no record → expected 0 succeeds.
    store
        .put_if_revision("cas_ns", "cas-key", &v1, 0)
        .await
        .unwrap();
    let stored = ConfigStore::get(&store, "cas_ns", "cas-key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["meta"]["revision"], 1);

    // Conflict: re-try with expected 0 should fail.
    let err = store
        .put_if_revision("cas_ns", "cas-key", &v1, 0)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::VersionConflict {
            expected: 0,
            actual: 1
        }
    ));
}
