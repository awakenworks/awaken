use awaken_agent_config::{AgentConfig, ConfigWrite, ScopeId, ScopedConfigRegistry};

pub async fn decision_table(store: &impl ScopedConfigRegistry) {
    // Causes: row absent/present, expected zero/exact/stale/out of storage range,
    // and an identical id in another scope.
    // R1 absent + zero -> create at 1; R2 absent + positive -> conflict/no insert;
    // R3 present + exact -> successor; R4 present + zero/stale -> unchanged;
    // R5 out-of-range -> error/no mutation. Scope participates in every rule.
    // RETURNING, current row, and immutable history must agree after each write.
    let scope = ScopeId::from("cas-owner");
    let other = ScopeId::from("cas-other");
    let mut config = AgentConfig {
        id: "revision-case".into(),
        ..Default::default()
    };
    for expected in [1, 9, i64::MAX as u64 - 1] {
        assert_eq!(
            store
                .put_config_if_revision_scoped(&scope, &config, expected)
                .await
                .unwrap(),
            ConfigWrite::Conflict {
                current_revision: None
            },
            "R2 absent"
        );
        assert!(
            store
                .get_config_revision_scoped(&scope, &config.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .list_config_revisions_scoped(&scope, &config.id)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        store
            .put_config_if_revision_scoped(&scope, &config, 0)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 1 },
        "R1"
    );
    assert_eq!(
        store
            .put_config_if_revision_scoped(&other, &config, 1)
            .await
            .unwrap(),
        ConfigWrite::Conflict {
            current_revision: None
        },
        "R2 scope isolation"
    );
    for expected in [0, 2, 10] {
        config.instructions = "must not apply".into();
        assert_eq!(
            store
                .put_config_if_revision_scoped(&scope, &config, expected)
                .await
                .unwrap(),
            ConfigWrite::Conflict {
                current_revision: Some(1)
            },
            "R4"
        );
        assert_ne!(
            store
                .get_config_scoped(&scope, &config.id)
                .await
                .unwrap()
                .unwrap(),
            config
        );
    }
    config.instructions = "winner".into();
    assert_eq!(
        store
            .put_config_if_revision_scoped(&scope, &config, 1)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 2 },
        "R3"
    );
    let current = store
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.revision, 2);
    assert_eq!(current.config, config);
    let history = store
        .list_config_revisions_scoped(&scope, &config.id)
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].revision, 2);
    assert_eq!(history[1].config, config);
    for expected in [i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        assert!(
            store
                .put_config_if_revision_scoped(&scope, &config, expected)
                .await
                .is_err(),
            "R5"
        );
        assert_eq!(
            store
                .get_config_revision_scoped(&scope, &config.id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            2
        );
    }
}

pub async fn exhausted(store: &impl ScopedConfigRegistry, config: &AgentConfig) {
    // C1 durable BIGINT max, C2 exact CAS/audited/unconditional mutation.
    // E1 explicit failure before data/history/effect/audit commit, E2 readable
    // unchanged integer revision. Exhaustion is never SQLite REAL promotion.
    let scope = ScopeId::from("exhausted");
    let mut changed = config.clone();
    changed.instructions = "must not apply".into();
    assert!(
        store
            .put_config_if_revision_scoped(&scope, &changed, i64::MAX as u64)
            .await
            .is_err()
    );
    assert!(store.put_config_scoped(&scope, &changed).await.is_err());
    let audit = awaken_agent_config::ManagementAuditRecord {
        tool: "patch".into(),
        call_id: "exhausted".into(),
        summary: "patch".into(),
    };
    store
        .record_management_audit_scoped(&scope, &audit)
        .await
        .unwrap();
    let effect = awaken_agent_config::ManagementEffect::UpsertAgentInputs {
        config: awaken_agent_config::AgentInputConfig {
            agent_id: config.id.clone(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    assert!(
        store
            .put_config_with_audit_effect_scoped(
                &scope,
                &changed,
                i64::MAX as u64,
                &audit,
                Some(&effect)
            )
            .await
            .is_err()
    );
    assert!(
        !store
            .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
            .await
            .unwrap()
            .unwrap()
            .business_committed
    );
    let row = store
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.revision, i64::MAX as u64);
    assert_eq!(&row.config, config);
    assert!(
        store
            .pending_management_effects_scoped(&scope)
            .await
            .unwrap()
            .is_empty()
    );
    let history = store
        .list_config_revisions_scoped(&scope, &config.id)
        .await
        .unwrap();
    assert_eq!(history.len(), 2, "only seeded revision 1 and injected max");
    assert_eq!(history[1].revision, i64::MAX as u64);
    assert_eq!(&history[1].config, config);
}

pub async fn concurrent_writers(store: std::sync::Arc<impl ScopedConfigRegistry + 'static>) {
    // R6: eight simultaneous creates/updates at the same expected revision
    // have exactly one winner. Every rejected intent leaves no history row.
    // A second read verifies the winner's exact content and returned revision.
    // Barrier overlap is executable race evidence, not exhaustive scheduling.
    let scope = ScopeId::from("cas-concurrent");
    for expected in [0, 1] {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        for writer in 0..8 {
            let store = store.clone();
            let barrier = barrier.clone();
            let scope = scope.clone();
            tasks.spawn(async move {
                let config = AgentConfig {
                    id: "race".into(),
                    instructions: format!("{expected}:{writer}"),
                    ..Default::default()
                };
                barrier.wait().await;
                let outcome = store
                    .put_config_if_revision_scoped(&scope, &config, expected)
                    .await
                    .unwrap();
                (config, outcome)
            });
        }
        let mut winner = None;
        let mut conflicts = 0;
        while let Some(result) = tasks.join_next().await {
            let (config, outcome) = result.unwrap();
            match outcome {
                ConfigWrite::Applied { revision } => {
                    assert_eq!(revision, expected + 1);
                    assert!(winner.replace(config).is_none(), "R6 one winner");
                }
                ConfigWrite::Conflict { current_revision } => {
                    assert_eq!(current_revision, Some(expected + 1));
                    conflicts += 1;
                }
            }
        }
        assert_eq!(conflicts, 7);
        let current = store
            .get_config_revision_scoped(&scope, "race")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, expected + 1);
        assert_eq!(Some(current.config), winner);
        assert_eq!(
            store
                .list_config_revisions_scoped(&scope, "race")
                .await
                .unwrap()
                .len(),
            expected as usize + 1
        );
    }
}
