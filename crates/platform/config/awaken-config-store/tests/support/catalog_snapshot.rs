//! Real native Config adapters supply identical catalog read frames.
use awaken_agent_config::catalog_snapshot::AgentCatalogSnapshotRepository;
use awaken_agent_config::personal_selection::{
    AgentSelectionIntent, AgentSelectionOwner, PersonalAgentSelectionRepository,
};
use awaken_agent_config::{AgentConfig, ScopeId, ScopedConfigRegistry, StoredPublication};

pub(super) fn owner() -> AgentSelectionOwner {
    AgentSelectionOwner::new("https://native.example".into(), "person-a".into()).unwrap()
}

pub(super) fn config(id: &str, name: &str) -> AgentConfig {
    AgentConfig {
        id: id.into(),
        name: Some(name.into()),
        instructions: format!("private {name}"),
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b").unwrap(),
        ..Default::default()
    }
}

fn publication(config: &AgentConfig, revision: u64, workspace: &str) -> StoredPublication {
    let metadata = awaken_runtime_contract::snapshot::AgentSnapshotMetadata {
        source: awaken_runtime_contract::AgentConfigRevisionRef {
            agent_id: awaken_runtime_contract::snapshot::AgentId(config.id.clone()),
            revision,
        },
        publication_version: awaken_runtime_contract::AgentPublicationVersion(format!(
            "v{revision}"
        )),
        ..Default::default()
    };
    let snapshot = awaken_agent_config::compile_resolved(config, &[], metadata, |_| true).unwrap();
    StoredPublication::published_at_revision(snapshot, &config.id, revision, workspace)
}

pub(super) async fn read_frame(
    store: &(
         impl ScopedConfigRegistry + PersonalAgentSelectionRepository + AgentCatalogSnapshotRepository
     ),
) {
    // C1 never-held scope; C2 exact published v1 and unpublished v2; C3 v3
    // published only for another execution Workspace; C4 other Account/issuer
    // preferences; C5 bounded overflow. Effects: no invented epoch; exact old
    // executable source, isolated preference and explicit unavailable on bounds.
    let scope = ScopeId::from("catalog");
    let person = owner();
    assert!(store.agent_catalog_epoch(&scope).await.unwrap().is_none());
    assert!(
        store
            .agent_catalog_snapshot(&scope, &person, 2)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let first = config("agent", "Published name");
    store.put_config_scoped(&scope, &first).await.unwrap();
    let v1 = publication(&first, 1, "catalog");
    store
        .put_publication_if_source_current_scoped(&scope, &v1)
        .await
        .unwrap();
    let published_epoch = store.agent_catalog_epoch(&scope).await.unwrap().unwrap();
    let second = config("agent", "Secret draft name");
    store.put_config_scoped(&scope, &second).await.unwrap();
    let third = config("agent", "Other target name");
    store.put_config_scoped(&scope, &third).await.unwrap();
    let author_epoch = store.agent_catalog_epoch(&scope).await.unwrap();
    store
        .put_publication_if_source_current_scoped(
            &scope,
            &publication(&third, 3, "other-workspace"),
        )
        .await
        .unwrap();
    assert_eq!(
        store.agent_catalog_epoch(&scope).await.unwrap(),
        author_epoch,
        "a foreign execution publication does not serialize this directory"
    );
    assert!(
        store
            .agent_catalog_epoch(&ScopeId::from("other-workspace"))
            .await
            .unwrap()
            .is_some()
    );
    let intent = AgentSelectionIntent {
        request_key: "select".into(),
        expected_generation: 0,
        selected: true,
    };
    store
        .set_personal_agent_selection(&scope, &person, "agent", &intent)
        .await
        .unwrap()
        .unwrap();
    let snapshot = store
        .agent_catalog_snapshot(&scope, &person, 2)
        .await
        .unwrap();
    assert!(snapshot.epoch.unwrap() > published_epoch);
    assert_eq!(snapshot.entries.len(), 1);
    let entry = &snapshot.entries[0];
    assert_eq!(entry.head.config, third);
    assert_eq!(entry.head.revision, 3);
    assert_eq!(entry.published.as_ref().unwrap().config, first);
    assert_eq!(entry.published.as_ref().unwrap().revision, 1);
    assert_eq!(
        entry.publication.as_ref().unwrap().fingerprint,
        v1.fingerprint
    );
    assert_eq!(snapshot.preferences.len(), 1);
    assert!(snapshot.preferences[0].state.selected);
    for foreign in [
        AgentSelectionOwner::new("https://native.example".into(), "person-b".into()).unwrap(),
        AgentSelectionOwner::new("https://other.example".into(), "person-a".into()).unwrap(),
    ] {
        assert!(
            store
                .agent_catalog_snapshot(&scope, &foreign, 2)
                .await
                .unwrap()
                .preferences
                .is_empty()
        );
    }
    let before = snapshot.epoch;
    store
        .put_config_scoped(&ScopeId::from("other"), &config("agent", "Other scope"))
        .await
        .unwrap();
    assert_eq!(store.agent_catalog_epoch(&scope).await.unwrap(), before);
    store
        .put_config_scoped(&scope, &config("second", "Second"))
        .await
        .unwrap();
    assert!(
        store
            .agent_catalog_snapshot(&scope, &person, 1)
            .await
            .is_err()
    );
    assert!(
        store
            .agent_catalog_snapshot(&scope, &person, 0)
            .await
            .is_err()
    );
    assert!(
        store
            .agent_catalog_snapshot(&scope, &person, 4097)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .agent_catalog_snapshot(&scope, &person, 2)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
}

/// Persisted owner preferences outlive definition visibility and existence.
pub(super) async fn owner_preferences_without_definition(
    store: &(impl PersonalAgentSelectionRepository + AgentCatalogSnapshotRepository),
) {
    let scope = ScopeId::from("owner-orphans");
    let person = owner();
    let intent = AgentSelectionIntent {
        request_key: "original".into(),
        expected_generation: 0,
        selected: true,
    };
    for id in ["a-orphan", "b-orphan"] {
        store
            .set_personal_agent_selection(&scope, &person, id, &intent)
            .await
            .unwrap()
            .unwrap();
    }
    let snapshot = store
        .agent_catalog_snapshot(&scope, &person, 2)
        .await
        .unwrap();
    assert!(snapshot.entries.is_empty());
    assert!(snapshot.epoch.is_some());
    assert_eq!(
        snapshot
            .preferences
            .iter()
            .map(|row| row.agent_id.as_str())
            .collect::<Vec<_>>(),
        ["a-orphan", "b-orphan"]
    );
    assert!(
        snapshot
            .preferences
            .iter()
            .all(|row| row.state.generation == 1 && row.state.selected)
    );
    assert!(
        store
            .agent_catalog_snapshot(&scope, &person, 1)
            .await
            .is_err(),
        "orphan preferences retain the independent finite native bound"
    );
    for foreign in [
        AgentSelectionOwner::new("https://native.example".into(), "person-b".into()).unwrap(),
        AgentSelectionOwner::new("https://other.example".into(), "person-a".into()).unwrap(),
    ] {
        assert!(
            store
                .agent_catalog_snapshot(&scope, &foreign, 2)
                .await
                .unwrap()
                .preferences
                .is_empty()
        );
    }
    store
        .set_personal_agent_selection(
            &scope,
            &person,
            "a-orphan",
            &AgentSelectionIntent {
                request_key: "remove".into(),
                expected_generation: 1,
                selected: false,
            },
        )
        .await
        .unwrap()
        .unwrap();
    let current = store
        .agent_catalog_snapshot(&scope, &person, 2)
        .await
        .unwrap();
    assert!(current.epoch > snapshot.epoch);
    assert_eq!(current.preferences[0].state.generation, 2);
    assert!(!current.preferences[0].state.selected);
    assert!(
        store
            .personal_agent_selection_outcome(&scope, &person, "a-orphan", "original")
            .await
            .unwrap()
            .unwrap()
            .intent
            .selected
    );
}
