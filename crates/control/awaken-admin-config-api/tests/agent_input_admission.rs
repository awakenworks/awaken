// @formal evidence agent_inputs.rejected_revision_does_not_mutate
#![cfg(all(feature = "sqlite", feature = "test-support"))]
#[path = "support/agent_input_admission.rs"]
mod admission;

#[test]
fn sqlite_agent_input_ambiguous_keys_fail_closed() {
    let store = awaken_admin_config_api::SqliteAdminStore::open_in_memory().unwrap();
    admission::ambiguous_keys_fail_closed(&store);
}

#[test]
fn reference_agent_input_ambiguous_keys_fail_closed() {
    let store = awaken_config_resolver::InMemoryAgentInputBindingRepository::new();
    admission::ambiguous_keys_fail_closed(&store);
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
    let store = awaken_admin_config_api::SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
    let control = rusqlite::Connection::open(&path).unwrap();
    admission::rollback_and_lost_response(&store, |reject| {
        control.execute_batch(if reject {
            "CREATE TRIGGER reject_input_insert BEFORE INSERT ON admin_agent_resource BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;
             CREATE TRIGGER reject_input_update BEFORE UPDATE ON admin_agent_resource BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"
        } else { "DROP TRIGGER reject_input_insert; DROP TRIGGER reject_input_update;" }).unwrap();
        std::future::ready(())
    }).await;
    drop(store);
    let store = awaken_admin_config_api::SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
    assert_eq!(
        store
            .get_agent_inputs("retry-workspace", "retry-agent")
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    // K4 historical key/content disagreement must fail closed in both get/list.
    control
        .execute(
            "UPDATE admin_agent_resource SET agent_id = ?1",
            ["retry-workspace\u{1f}wrong-agent"],
        )
        .unwrap();
    assert!(
        store
            .get_agent_inputs("retry-workspace", "wrong-agent")
            .is_err()
    );
    assert!(store.list_agent_inputs("retry-workspace").is_err());
    drop(control);
    drop(store);
    std::fs::remove_file(path).unwrap();
}
