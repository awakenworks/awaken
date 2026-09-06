use awaken_config_resolver::{AgentInputBindingRepository, AgentInputConfig};

pub fn ambiguous_keys_fail_closed(store: &impl AgentInputBindingRepository) {
    // K1: delimiter in either coordinate can alias ("a", "b<sep>c") with
    // ("a<sep>b", "c"). K2: NUL cannot be represented by PostgreSQL TEXT.
    // Both reject before read/write/list, not just at the HTTP edge. K3: valid
    // Unicode/colon coordinates remain exact and old valid storage keys work.
    // Rejection is required for ALL adapters, including the tuple-key reference.
    let config = |id: &str| AgentInputConfig {
        agent_id: id.into(),
        environment: None,
        inputs: Vec::new(),
        revision: 1,
    };
    store.put_agent_inputs("owner", config("agent")).unwrap();
    for (workspace, agent) in [
        ("a", "b\u{1f}c"),
        ("a\u{1f}b", "c"),
        ("a\0b", "c"),
        ("a", "b\0c"),
    ] {
        assert!(
            store.put_agent_inputs(workspace, config(agent)).is_err(),
            "K1/K2 write"
        );
        assert!(
            store.get_agent_inputs(workspace, agent).is_err(),
            "K1/K2 read"
        );
    }
    for workspace in ["a\u{1f}b", "a\0b"] {
        assert!(store.list_agent_inputs(workspace).is_err(), "K1/K2 list");
    }
    assert_eq!(
        store.get_agent_inputs("owner", "agent").unwrap(),
        Some(config("agent"))
    );
    store
        .put_agent_inputs("工作区:one", config("agent:two"))
        .unwrap();
    assert_eq!(
        store.list_agent_inputs("工作区:one").unwrap(),
        vec![config("agent:two")]
    );
}

pub async fn rollback_and_lost_response<F, Fut>(
    store: &impl AgentInputBindingRepository,
    mut reject_writes: F,
) where
    F: FnMut(bool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // B1 write failure before commit -> absent; same plan retry -> revision 1.
    // B2 commit response is unavailable to caller -> exact replay does NOT
    // execute INSERT/UPDATE (a rejecting trigger is still installed).
    // B3 different same-revision payload -> conflict, not replay/success.
    // B4 successor write fails -> old durable content; same successor retries.
    // SQL triggers inject failure inside the actual adapter transaction, not a
    // mocked repository. Connection reuse checks lock/rollback cleanup as well.
    let config = AgentInputConfig {
        agent_id: "retry-agent".into(),
        environment: None,
        inputs: Vec::new(),
        revision: 1,
    };
    reject_writes(true).await;
    assert!(
        store
            .put_agent_inputs("retry-workspace", config.clone())
            .is_err(),
        "B1"
    );
    assert_eq!(
        store
            .get_agent_inputs("retry-workspace", &config.agent_id)
            .unwrap(),
        None
    );
    reject_writes(false).await;
    store
        .put_agent_inputs("retry-workspace", config.clone())
        .unwrap();
    reject_writes(true).await;
    store
        .put_agent_inputs("retry-workspace", config.clone())
        .expect("B2 replay must not execute a write");
    let mut changed = config.clone();
    changed.environment = Some(awaken_config_resolver::AgentEnvironmentBinding {
        environment_id: "new".into(),
        revision: 1,
    });
    assert!(
        matches!(
            store.put_agent_inputs("retry-workspace", changed.clone()),
            Err(awaken_config_resolver::AgentInputRepositoryError::RevisionConflict { .. })
        ),
        "B3"
    );
    changed.revision = 2;
    assert!(
        store
            .put_agent_inputs("retry-workspace", changed.clone())
            .is_err(),
        "B4"
    );
    assert_eq!(
        store
            .get_agent_inputs("retry-workspace", &config.agent_id)
            .unwrap(),
        Some(config)
    );
    reject_writes(false).await;
    store
        .put_agent_inputs("retry-workspace", changed.clone())
        .unwrap();
    assert_eq!(
        store
            .get_agent_inputs("retry-workspace", &changed.agent_id)
            .unwrap(),
        Some(changed)
    );
}
