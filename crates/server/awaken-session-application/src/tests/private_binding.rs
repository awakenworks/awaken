//! Native owner decisions over the real Session repository, before any content.
use super::*;
use crate::{SessionAccessError, SessionQueries, SessionRootReads};
use awaken_session_contract::{LifecycleOutbox, SessionPrivateBinding, SessionRepositoryReads};

fn binding() -> SessionPrivateBinding {
    SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "workspace".into(),
        1,
    )
    .unwrap()
}

#[tokio::test]
async fn workspace_creation_receipt_cannot_disclose_a_private_root() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(repo.as_ref(), persisted("legacy-replay", false, "idle")).await;
    create(
        repo.as_ref(),
        binding()
            .bind_fresh(persisted("personal-replay", false, "idle"))
            .unwrap(),
    )
    .await;
    let mutations = crate::SessionMutations::new(repo, awaken_time::system_clock());
    let record = |id: &str| awaken_session_contract::IdempotencyRecord {
        key: format!("create:{id}"),
        payload_hash: format!("payload:{id}"),
    };
    assert!(
        mutations
            .replay_accepted_session_create(
                "workspace",
                &SessionId::from("legacy-replay"),
                &record("legacy-replay")
            )
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        mutations
            .replay_accepted_session_create(
                "workspace",
                &SessionId::from("personal-replay"),
                &record("personal-replay")
            )
            .await
            .unwrap()
            .is_none(),
        "an original receipt is not a private owner proof"
    );
}

#[tokio::test]
async fn workspace_insert_replay_cannot_adopt_a_private_roots_receipt() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(
        repo.as_ref(),
        binding()
            .bind_fresh(persisted("private-insert-replay", false, "idle"))
            .unwrap(),
    )
    .await;
    let mutations = crate::SessionMutations::new(repo.clone(), awaken_time::system_clock());
    let fresh = PersistedSession::frozen_with_budget(
        "private-insert-replay",
        persisted("template", false, "idle")
            .frozen_baseline()
            .unwrap()
            .clone(),
        Default::default(),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let root = crate::creation::CompleteSessionRoot::from_fresh(fresh).unwrap();
    let result = mutations
        .create_session_root(
            "workspace",
            root,
            awaken_session_contract::IdempotencyRecord {
                key: "create:private-insert-replay".into(),
                payload_hash: "payload:private-insert-replay".into(),
            },
            Vec::new(),
        )
        .await;
    assert!(
        matches!(result, Err(crate::SessionCreationError::Conflict)),
        "a Workspace candidate cannot receive a private root through insert replay"
    );
    assert_eq!(
        SessionPrivateBinding::of(
            &repo
                .get(&SessionId::from("private-insert-replay"))
                .await
                .unwrap()
        ),
        Some(&binding())
    );
}

#[tokio::test]
async fn private_binding_owner_matrix_refuses_workspace_admin_and_filters_collection() {
    // Original Account/application/Org/Workspace/revision all agree -> proof.
    // Vary each one independently, omit private binding, or name a missing id
    // -> identical NotFound. Workspace authority alone never upgrades a caller.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(repo.as_ref(), persisted("legacy", false, "idle")).await;
    create(
        repo.as_ref(),
        binding()
            .bind_fresh(persisted("personal", false, "idle"))
            .unwrap(),
    )
    .await;
    let executor =
        crate::SessionExecutor::colocated(Arc::new(NoopRuntime), Arc::new(NoopMcpRealizer));
    let queries = SessionQueries::new(repo.clone(), &executor, None);
    let id = SessionId::from("personal");
    assert!(
        queries
            .authorize("workspace", &SessionId::from("legacy"))
            .await
            .is_ok()
    );
    assert_eq!(
        queries.authorize("workspace", &id).await,
        Err(SessionAccessError::NotFound)
    );
    let access = queries.authorize_private(&binding(), &id).await.unwrap();
    assert_eq!(access.session_id(), &id);
    assert_eq!(access.owner_scope(), "workspace");
    assert_eq!(
        queries
            .authorize_private(&binding(), &SessionId::from("legacy"))
            .await,
        Err(SessionAccessError::NotFound)
    );
    assert_eq!(
        queries
            .authorize_private(&binding(), &SessionId::from("missing"))
            .await,
        Err(SessionAccessError::NotFound)
    );
    for field in [
        "org_id",
        "application_id",
        "account_id",
        "workspace_id",
        "application_binding_revision",
    ] {
        let mut changed = serde_json::to_value(binding()).unwrap();
        changed[field] = if field == "application_binding_revision" {
            serde_json::json!(2)
        } else {
            serde_json::json!("other")
        };
        let changed: SessionPrivateBinding = serde_json::from_value(changed).unwrap();
        assert_eq!(
            queries.authorize_private(&changed, &id).await,
            Err(SessionAccessError::NotFound),
            "{field}"
        );
    }
    let visible = queries.sessions_by_owner("workspace").await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].session_id.as_str(), "legacy");
    assert_eq!(
        SessionPrivateBinding::of(&repo.get(&id).await.unwrap()),
        Some(&binding())
    );
    assert!(repo.pending_lifecycle().await.unwrap().is_empty());
}
