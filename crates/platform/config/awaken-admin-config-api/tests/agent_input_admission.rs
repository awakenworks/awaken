// @formal evidence agent_inputs.rejected_revision_does_not_mutate
#![cfg(all(feature = "sqlite", feature = "test-support"))]
#[path = "support/agent_input_admission.rs"]
mod admission;

#[tokio::test]
async fn sqlite_agent_input_ambiguous_keys_fail_closed() {
    let store = awaken_admin_config_api::SqliteAdminStore::open_in_memory()
        .await
        .unwrap();
    admission::ambiguous_keys_fail_closed(&store).await;
}

#[tokio::test]
async fn reference_agent_input_ambiguous_keys_fail_closed() {
    let store = awaken_config_resolver::InMemoryAgentInputBindingRepository::new();
    admission::ambiguous_keys_fail_closed(&store).await;
}

#[tokio::test]
async fn sqlite_agent_input_rollback_replay_and_corrupt_key_readback() {
    use awaken_config_resolver::AgentInputBindingRepository;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-input-admission-{}-{nonce}.db",
        std::process::id()
    ));
    let store = awaken_admin_config_api::SqliteAdminStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    let control = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    admission::rollback_and_lost_response(&store, |reject| {
        let control = control.clone();
        async move {
            sqlx::raw_sql(if reject {
                "CREATE TRIGGER reject_input_insert BEFORE INSERT ON admin_agent_resource BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;
                 CREATE TRIGGER reject_input_update BEFORE UPDATE ON admin_agent_resource BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"
            } else { "DROP TRIGGER reject_input_insert; DROP TRIGGER reject_input_update;" })
            .execute(&control)
            .await
            .unwrap();
        }
    }).await;
    drop(store);
    awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .settled()
        .await;
    let store = awaken_admin_config_api::SqliteAdminStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        store
            .get_agent_inputs("retry-workspace", "retry-agent")
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    // K4 historical key/content disagreement must fail closed in both get/list.
    sqlx::query("UPDATE admin_agent_resource SET agent_id = ?1")
        .bind("retry-workspace\u{1f}wrong-agent")
        .execute(&control)
        .await
        .unwrap();
    assert!(
        store
            .get_agent_inputs("retry-workspace", "wrong-agent")
            .await
            .is_err()
    );
    assert!(store.list_agent_inputs("retry-workspace").await.is_err());
    control.close().await;
    drop(store);
    awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .settled()
        .await;
    std::fs::remove_file(path).unwrap();
}
