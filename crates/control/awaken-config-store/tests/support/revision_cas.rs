use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, ConfigWrite, ScopeId, ScopedConfigRegistry,
};

// Readback projection only: no CAS predicate, successor calculation or model.
// Every rejected write compares the exact preexisting content, generation,
// lifecycle timestamps and all immutable history entries, including order.
type RevisionImage = (AgentConfig, u64, Option<u64>, Option<u64>);
type Frame = (Option<RevisionImage>, Vec<RevisionImage>);

fn image(row: AgentConfigRevision) -> RevisionImage {
    let AgentConfigRevision {
        config,
        revision,
        created_at_unix_ms,
        updated_at_unix_ms,
    } = row;
    (config, revision, created_at_unix_ms, updated_at_unix_ms)
}

pub(super) async fn frame(store: &impl ScopedConfigRegistry, scope: &ScopeId, id: &str) -> Frame {
    (
        store
            .get_config_revision_scoped(scope, id)
            .await
            .unwrap()
            .map(image),
        store
            .list_config_revisions_scoped(scope, id)
            .await
            .unwrap()
            .into_iter()
            .map(image)
            .collect(),
    )
}

pub async fn publication_source_table(store: &impl ScopedConfigRegistry) {
    // P1 source absent; P2 stale expected; P3 future expected; P4 exact source.
    // All candidates use the real compiler and StoredPublication, with nonempty
    // instructions and a resolved model. Expected/source pairing is explicit.
    // Rejection must preserve BOTH the full authored/history frame and every
    // durable publication byte, with empty AND populated publication history.
    // This suite does not claim registrar execution.
    // The ConfigService test separately observes zero calls after CAS conflict.
    let config = AgentConfig {
        id: "publication-source".into(),
        instructions: "real compiled publication candidate".into(),
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b"),
        ..Default::default()
    };
    for (present, populate_first) in [(false, false), (true, false), (true, true)] {
        let scope = ScopeId::from(match (present, populate_first) {
            (false, _) => "publication-absent",
            (true, false) => "publication-present",
            (true, true) => "publication-retained",
        });
        if present {
            store.put_config_scoped(&scope, &config).await.unwrap();
            store.put_config_scoped(&scope, &config).await.unwrap();
        }
        let revisions: &[u64] = if populate_first {
            &[2, 0, 1, 3, u64::MAX, 2]
        } else {
            &[0, 1, 3, u64::MAX, 2]
        };
        for &expected in revisions {
            let metadata = awaken_runtime_contract::snapshot::AgentSnapshotMetadata {
                source: awaken_runtime_contract::AgentConfigRevisionRef {
                    agent_id: awaken_runtime_contract::snapshot::AgentId(config.id.clone()),
                    revision: expected,
                },
                publication_version: awaken_runtime_contract::AgentPublicationVersion(format!(
                    "v{expected}"
                )),
                ..Default::default()
            };
            let snapshot = awaken_agent_config::compile_resolved(&config, &[], metadata).unwrap();
            let publication = awaken_agent_config::StoredPublication::published_at_revision(
                snapshot,
                &config.id,
                expected,
                scope.clone(),
            );
            let before = frame(store, &scope, &config.id).await;
            let durable_before =
                serde_json::to_value(store.list_published_scoped(&scope).await.unwrap()).unwrap();
            let outcome = store
                .put_publication_if_config_revision_scoped(&scope, &publication, expected)
                .await
                .unwrap();
            assert_eq!(
                frame(store, &scope, &config.id).await,
                before,
                "P1-P4 authoring frame"
            );
            let durable = store.list_published_scoped(&scope).await.unwrap();
            if present && expected == 2 {
                assert_eq!(outcome, ConfigWrite::Applied { revision: 2 }, "P4");
                assert_eq!(
                    serde_json::to_value(&durable).unwrap(),
                    serde_json::json!([publication]),
                    "P4 exact durable payload"
                );
                assert_eq!(
                    store
                        .put_publication_if_config_revision_scoped(&scope, &publication, expected)
                        .await
                        .unwrap(),
                    outcome,
                    "P4 exact retry"
                );
                assert_eq!(
                    serde_json::to_value(store.list_published_scoped(&scope).await.unwrap())
                        .unwrap(),
                    serde_json::to_value(durable).unwrap(),
                    "P4 retry frame"
                );
            } else {
                assert_eq!(
                    outcome,
                    ConfigWrite::Conflict {
                        current_revision: if present { Some(2) } else { None }
                    },
                    "P1-P3"
                );
                assert_eq!(
                    serde_json::to_value(durable).unwrap(),
                    durable_before,
                    "P1-P3 zero durable writes"
                );
            }
        }
    }
}

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
    let absent = frame(store, &scope, &config.id).await;
    assert_eq!(absent, (None, vec![]));
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
        assert_eq!(
            frame(store, &scope, &config.id).await,
            absent,
            "R2 full frame"
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
    // R4S: another populated scope has the same id and revision. A missing
    // scope predicate must be detected even when RETURNING reports one winner.
    let mut foreign = config.clone();
    foreign.instructions = "foreign scope must survive".into();
    store.put_config_scoped(&other, &foreign).await.unwrap();
    let foreign_before = frame(store, &other, &config.id).await;
    let before = frame(store, &scope, &config.id).await;
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
        assert_eq!(
            frame(store, &scope, &config.id).await,
            before,
            "R4 full frame"
        );
        assert_eq!(
            frame(store, &other, &config.id).await,
            foreign_before,
            "R4S full frame"
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
    assert_eq!(
        image(history[0].clone()),
        before.1[0],
        "R3 immutable predecessor"
    );
    assert_eq!(
        frame(store, &other, &config.id).await,
        foreign_before,
        "R3 scope frame"
    );
    let before = frame(store, &scope, &config.id).await;
    for expected in [i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        assert!(
            store
                .put_config_if_revision_scoped(&scope, &config, expected)
                .await
                .is_err(),
            "R5"
        );
        assert_eq!(
            frame(store, &scope, &config.id).await,
            before,
            "R5 full frame"
        );
        assert_eq!(
            frame(store, &other, &config.id).await,
            foreign_before,
            "R5 scope frame"
        );
    }
    // R7 unconditional put advances the same aggregate, invalidating a prior
    // CAS token. Positive monotonicity here is SQL evidence, not plan proof.
    config.instructions = "unconditional successor".into();
    store.put_config_scoped(&scope, &config).await.unwrap();
    let unconditional = frame(store, &scope, &config.id).await;
    assert_eq!(unconditional.0.as_ref().unwrap().0, config);
    assert_eq!(unconditional.0.as_ref().unwrap().1, 3);
    assert_eq!(unconditional.1.len(), 3);
    assert_eq!(&unconditional.1[..2], before.1.as_slice());
    config.instructions = "stale after unconditional".into();
    assert_eq!(
        store
            .put_config_if_revision_scoped(&scope, &config, 2)
            .await
            .unwrap(),
        ConfigWrite::Conflict {
            current_revision: Some(3)
        }
    );
    assert_eq!(
        frame(store, &scope, &config.id).await,
        unconditional,
        "R7 full frame"
    );
    assert_eq!(
        frame(store, &other, &config.id).await,
        foreign_before,
        "R7 scope frame"
    );
}

pub async fn exhausted(store: &impl ScopedConfigRegistry, config: &AgentConfig) {
    // C1 durable BIGINT max, C2 exact CAS/audited/unconditional mutation.
    // E1 explicit failure before data/history/effect/audit commit, E2 readable
    // unchanged integer revision. Exhaustion is never SQLite REAL promotion.
    let scope = ScopeId::from("exhausted");
    let before = frame(store, &scope, &config.id).await;
    let mut changed = config.clone();
    changed.instructions = "must not apply".into();
    assert!(
        store
            .put_config_if_revision_scoped(&scope, &changed, i64::MAX as u64)
            .await
            .is_err()
    );
    assert_eq!(
        frame(store, &scope, &config.id).await,
        before,
        "exhausted CAS frame"
    );
    assert!(store.put_config_scoped(&scope, &changed).await.is_err());
    assert_eq!(
        frame(store, &scope, &config.id).await,
        before,
        "exhausted unconditional frame"
    );
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
    assert_eq!(
        frame(store, &scope, &config.id).await,
        before,
        "exhausted audited frame"
    );
}

pub async fn history_failure_rolls_back(store: &impl ScopedConfigRegistry) {
    // R8 cause: the existing history trigger's generation-2 INSERT fails after
    // the row write. Plain CAS, unconditional put and audited+effect commit
    // must return that injected error and preserve the entire generation-1
    // frame. Audit/effect changes belong to the same transaction and roll back.
    let scope = ScopeId::from("history-failure");
    let mut config = AgentConfig {
        id: "history-failure".into(),
        ..Default::default()
    };
    store.put_config_scoped(&scope, &config).await.unwrap();
    let before = frame(store, &scope, &config.id).await;
    config.instructions = "must roll back".into();
    let audit = awaken_agent_config::ManagementAuditRecord {
        tool: "patch".into(),
        call_id: "history-failure".into(),
        summary: "patch".into(),
    };
    store
        .record_management_audit_scoped(&scope, &audit)
        .await
        .unwrap();
    let audit_before = store
        .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
        .await
        .unwrap()
        .unwrap();
    let effect = awaken_agent_config::ManagementEffect::UpsertAgentInputs {
        config: awaken_agent_config::AgentInputConfig {
            agent_id: config.id.clone(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    for operation in 0..3 {
        let error = match operation {
            0 => store
                .put_config_if_revision_scoped(&scope, &config, 1)
                .await
                .unwrap_err(),
            1 => store.put_config_scoped(&scope, &config).await.unwrap_err(),
            _ => store
                .put_config_with_audit_effect_scoped(&scope, &config, 1, &audit, Some(&effect))
                .await
                .unwrap_err(),
        };
        assert!(
            error
                .to_string()
                .contains("injected config history failure"),
            "R8 expected trigger failure: {error}"
        );
        assert_eq!(
            frame(store, &scope, &config.id).await,
            before,
            "R8 complete rollback"
        );
        let after = store
            .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.record, audit_before.record);
        assert_eq!(after.business_committed, audit_before.business_committed);
        assert!(
            store
                .pending_management_effects_scoped(&scope)
                .await
                .unwrap()
                .is_empty(),
            "R8 effect rollback"
        );
    }
}

pub async fn concurrent_writers(stores: &[std::sync::Arc<impl ScopedConfigRegistry + 'static>]) {
    // R6: eight simultaneous creates/updates at the same expected revision
    // have exactly one winner. Every rejected intent leaves no history row.
    // A second read verifies the winner's exact content and returned revision.
    // Independent adapter connections arbitrate through the same database.
    // Barrier overlap is executable race evidence, not exhaustive scheduling.
    assert!(stores.len() >= 2);
    let store = &stores[0];
    let scope = ScopeId::from("cas-concurrent");
    for expected in [0, 1] {
        let before = frame(store.as_ref(), &scope, "race").await;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        for writer in 0..8 {
            let store = stores[writer % stores.len()].clone();
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
        let after = frame(store.as_ref(), &scope, "race").await;
        assert_eq!(after.1.len(), expected as usize + 1);
        assert_eq!(
            &after.1[..before.1.len()],
            before.1.as_slice(),
            "R6 immutable history"
        );
        assert_eq!(after.1.last().unwrap().0, after.0.as_ref().unwrap().0);
        assert_eq!(after.1.last().unwrap().1, expected + 1);
        for reader in &stores[1..] {
            assert_eq!(
                frame(reader.as_ref(), &scope, "race").await,
                after,
                "R6 cross-connection readback"
            );
        }
    }
}
