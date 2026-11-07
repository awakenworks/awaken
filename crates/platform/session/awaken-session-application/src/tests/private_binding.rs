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
async fn native_private_creation_binds_the_first_root_and_recovers_only_its_original_owner() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(super::creation::RecordingCreateRuntime::default());
    let probe = Arc::new(crate::ExecutionObservationProbe::default());
    let app = crate::SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    let record = awaken_session_contract::IdempotencyRecord {
        key: "private-original-create".into(),
        payload_hash: "same-protocol-fingerprint".into(),
    };
    let command = || {
        let mut command = super::creation::creation_command("private-created");
        command.idempotency = Some(record.clone());
        command
            .metadata
            .insert("account_id".into(), "metadata-cannot-choose-owner".into());
        command
    };
    let root = app
        .creation_commands
        .accept_private_session(binding(), command())
        .await
        .unwrap();
    assert_eq!(SessionPrivateBinding::of(&root), Some(&binding()));
    assert_eq!(
        SessionPrivateBinding::of(&repo.get(&root.session_id).await.unwrap()),
        Some(&binding())
    );
    let role: &dyn crate::SessionCreation = app.creation_commands.as_ref();
    let recovered = role
        .replay_accepted_private_session_create(&binding(), &root.session_id, &record)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered, root);
    assert!(
        role.replay_accepted_session_create("workspace", &root.session_id, &record)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        app.creation_commands.accept_session(command()).await,
        Err(crate::SessionCreationError::Conflict)
    ));
    let other = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "bob".into(),
        "workspace".into(),
        1,
    )
    .unwrap();
    assert!(
        role.replay_accepted_private_session_create(&other, &root.session_id, &record)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        app.creation_commands
            .accept_private_session(other, command())
            .await,
        Err(crate::SessionCreationError::Conflict)
    ));
    let replayed = app
        .creation_commands
        .accept_private_session(binding(), command())
        .await
        .unwrap();
    assert_eq!(replayed, root);
    assert_eq!(
        repo.get(&root.session_id).await.unwrap().revision,
        root.revision
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0);
    for field in [
        "org_id",
        "application_id",
        "account_id",
        "application_binding_revision",
    ] {
        let mut changed = serde_json::to_value(binding()).unwrap();
        changed[field] = if field == "application_binding_revision" {
            serde_json::json!(2)
        } else {
            serde_json::json!("other")
        };
        let changed: SessionPrivateBinding = serde_json::from_value(changed).unwrap();
        assert!(
            role.replay_accepted_private_session_create(&changed, &root.session_id, &record)
                .await
                .unwrap()
                .is_none(),
            "{field}"
        );
        assert!(
            matches!(
                app.creation_commands
                    .accept_private_session(changed, command())
                    .await,
                Err(crate::SessionCreationError::Conflict)
            ),
            "{field}"
        );
    }
    let awaited = app
        .creation_commands
        .create_private_session(
            binding(),
            super::creation::creation_command("private-await"),
        )
        .await
        .unwrap();
    assert_eq!(SessionPrivateBinding::of(&awaited), Some(&binding()));
    assert_eq!(
        SessionPrivateBinding::of(&repo.get(&awaited.session_id).await.unwrap()),
        Some(&binding())
    );
    assert!(
        runtime.baseline_installs.load(Ordering::SeqCst) > 0,
        "private creation uses the existing realization path"
    );
}

#[tokio::test]
async fn private_creation_refuses_a_foreign_workspace_before_any_root_insert() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let probe = Arc::new(crate::ExecutionObservationProbe::default());
    let app = crate::SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    let mut command = super::creation::creation_command("wrong-private-workspace");
    command.owner_scope = "another-workspace".into();
    assert!(
        matches!(app.creation_commands.accept_private_session(binding(), command).await, Err(crate::SessionCreationError::Rejected(error)) if error.code == "private_session_scope_mismatch")
    );
    assert!(matches!(
        repo.get(&SessionId::from("wrong-private-workspace")).await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    ));
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
        crate::SessionExecutionRoles::colocated(Arc::new(NoopRuntime), Arc::new(NoopMcpRealizer));
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

#[tokio::test]
async fn inconsistent_session_root_cannot_replace_the_file_readers_owner() {
    use awaken_resource_contract::FileContentVisibility as _;
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let alice = binding()
        .bind_fresh(persisted("file-reader-alice", false, "idle"))
        .unwrap();
    let bob = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "bob".into(),
        "workspace".into(),
        1,
    )
    .unwrap()
    .bind_fresh(persisted("file-source-bob", false, "idle"))
    .unwrap();
    create(repository.as_ref(), alice).await;
    create(repository.as_ref(), bob.clone()).await;
    let reads = Arc::new(FaultingSessionRepository::new(repository));
    let source = crate::SessionFileContentAccess::new(reads.clone());
    assert!(
        source
            .file_content_is_visible(
                "workspace",
                Some("file-reader-alice"),
                None,
                Some("file-reader-alice")
            )
            .await
            .unwrap()
    );
    assert!(
        !source
            .file_content_is_visible(
                "workspace",
                Some("file-source-bob"),
                None,
                Some("file-reader-alice")
            )
            .await
            .unwrap()
    );
    // A source-boundary fault returns a well-formed but differently addressed
    // root from the real selected repository. This cannot substitute its owner.
    let wrong_partition = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "other-workspace".into(),
        1,
    )
    .unwrap()
    .bind_fresh(persisted("file-reader-alice", false, "idle"))
    .unwrap();
    for malformed in [bob, wrong_partition] {
        reads.return_get_once(malformed);
        let result = source
            .file_content_is_visible(
                "workspace",
                Some("file-source-bob"),
                None,
                Some("file-reader-alice"),
            )
            .await;
        assert!(
            result.is_err(),
            "an inconsistent native observation cannot be an owner verdict: {result:?}"
        );
    }
}

#[tokio::test]
async fn observed_owner_coordinates_must_match_before_session_access_is_minted() {
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let id = SessionId::from("access-reader-alice");
    create(
        repository.as_ref(),
        binding()
            .bind_fresh(persisted(id.as_str(), false, "idle"))
            .unwrap(),
    )
    .await;
    let bob_binding = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "bob".into(),
        "workspace".into(),
        1,
    )
    .unwrap();
    let bob = bob_binding
        .clone()
        .bind_fresh(persisted("access-source-bob", false, "idle"))
        .unwrap();
    create(repository.as_ref(), bob.clone()).await;
    let reads = Arc::new(FaultingSessionRepository::new(repository));
    let executor =
        crate::SessionExecutionRoles::colocated(Arc::new(NoopRuntime), Arc::new(NoopMcpRealizer));
    let queries = SessionQueries::new(reads.clone(), &executor, None);
    assert!(queries.authorize_private(&binding(), &id).await.is_ok());
    assert_eq!(
        queries.authorize("workspace", &id).await,
        Err(SessionAccessError::NotFound)
    );
    reads.return_get_once(persisted("unrelated-ordinary-root", false, "idle"));
    let workspace = queries.authorize("workspace", &id).await;
    assert!(
        matches!(workspace, Err(SessionAccessError::Internal(_))),
        "an unrelated ordinary observation cannot mint Workspace access to a private root: {workspace:?}"
    );
    reads.return_get_once(bob);
    let private = queries.authorize_private(&bob_binding, &id).await;
    assert!(
        matches!(private, Err(SessionAccessError::Internal(_))),
        "another returned root cannot substitute its private owner: {private:?}"
    );
    let wrong_partition = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "other-workspace".into(),
        1,
    )
    .unwrap()
    .bind_fresh(persisted(id.as_str(), false, "idle"))
    .unwrap();
    reads.return_get_once(wrong_partition);
    assert!(
        matches!(
            queries.authorize_private(&binding(), &id).await,
            Err(SessionAccessError::Internal(_))
        ),
        "native owner and immutable private Workspace must agree"
    );
    assert_eq!(
        queries.authorize("workspace", &id).await,
        Err(SessionAccessError::NotFound),
        "a bad observation cannot leave a permissive cache"
    );
}
