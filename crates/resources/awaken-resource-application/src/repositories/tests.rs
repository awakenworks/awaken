use super::*;
use awaken_resource_contract::{ChangeRepositoryState, ClonePolicy, PublishRepositoryConfig};

struct Credentials;

#[async_trait]
impl RepositoryCredentialValidator for Credentials {
    async fn validate(
        &self,
        workspace: &str,
        source: &str,
        remote: &str,
    ) -> Result<(), ResourceRegistryError> {
        if workspace == "workspace-a"
            && source == "github-authorization"
            && remote.starts_with("https://github.com/")
        {
            Ok(())
        } else {
            Err(invalid("Repository authorization is unavailable"))
        }
    }
}

async fn application() -> RepositoryApplication {
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    RepositoryApplication::new(
        Arc::new(crate::RegistryApplication::new(
            storage,
            awaken_time::system_clock(),
        )),
        Arc::new(Credentials),
        awaken_time::system_clock(),
    )
}

fn command() -> CreateRepositoryCommand {
    CreateRepositoryCommand {
        workspace_id: "workspace-a".into(),
        name: "Workshop materials".into(),
        description: "Read workshop exercises".into(),
        remote_url: "https://github.com/example/workshop.git".into(),
        credential_binding: None,
        initial_branch: Some("main".into()),
        initial_commit: None,
        clone_policy: ClonePolicy { depth: Some(1) },
    }
}

#[tokio::test]
async fn concurrent_staged_activation_has_one_transition_and_never_undoes_admin_suspend() {
    // C1 one staged registration/proof; C2 eight concurrent exact completions;
    // C3 administrative suspension after completion. E1 C1+C2 => all callers
    // resolve with one aggregate revision increment; E2 C3 => the old proof
    // rejects and the resource stays suspended. Observe the original SQL store,
    // not a copied completion predicate or timestamp-based approximation.
    use awaken_resource_contract::{
        AggregateRevision, InsertOutcome, RegisterRepository, RepositoryConfigVersion,
        RepositoryDefinition, RepositoryRegistrationMode, ResourceAdministration,
        ResourceInventory, ResourceRegistryRepository, ResourceTimestamps,
    };
    let store = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let registry = Arc::new(crate::RegistryApplication::new(
        store.clone(),
        awaken_time::system_clock(),
    ));
    let registered = registry
        .register_repository_idempotent(
            RegisterRepository {
                definition: RepositoryDefinition {
                    id: "staged".into(),
                    workspace_id: "workspace-a".into(),
                    name: "staged".into(),
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Suspended,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps: ResourceTimestamps::default(),
                },
                initial_config: RepositoryConfigVersion {
                    repository_id: "staged".into(),
                    version: ConfigVersion::INITIAL,
                    remote_url: "https://github.com/example/staged.git".into(),
                    credential_binding: None,
                    initial_branch: None,
                    initial_commit: None,
                    clone_policy: ClonePolicy::default(),
                },
            },
            RepositoryRegistrationMode::Preparation,
        )
        .await
        .unwrap();
    assert_eq!(registered.outcome, InsertOutcome::Inserted);
    let proof = registered.preparation.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let (registry, proof, barrier) = (registry.clone(), proof.clone(), barrier.clone());
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            registry
                .change_repository_state(ChangeRepositoryState {
                    workspace_id: "workspace-a".into(),
                    id: "staged".into(),
                    state: ResourceState::Active,
                    preparation: Some(proof),
                })
                .await
        }));
    }
    for worker in workers {
        worker.await.unwrap().unwrap();
    }
    let stored = store.load_repository("staged").await.unwrap().unwrap();
    assert_eq!(
        stored.revision,
        AggregateRevision::new(2).unwrap(),
        "E1 one completion CAS"
    );
    assert_eq!(stored.aggregate.definition().state, ResourceState::Active);
    registry
        .change_repository_state(ChangeRepositoryState {
            workspace_id: "workspace-a".into(),
            id: "staged".into(),
            state: ResourceState::Suspended,
            preparation: None,
        })
        .await
        .unwrap();
    assert!(
        registry
            .change_repository_state(ChangeRepositoryState {
                workspace_id: "workspace-a".into(),
                id: "staged".into(),
                state: ResourceState::Active,
                preparation: Some(proof),
            })
            .await
            .is_err(),
        "E2 old completion cannot revive an administrator's suspension"
    );
    assert_eq!(
        registry
            .find_repository("workspace-a", "staged")
            .await
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Suspended
    );
}

#[tokio::test]
async fn exact_retry_survives_config_changes_but_changed_payload_and_retirement_do_not() {
    // R1 same key+payload -> same identity; R2 payload change -> conflict;
    // R3 later config -> preserve current config; R4 retired -> never revive.
    let app = application().await;
    let original = app.create_idempotent(command(), "request-1").await.unwrap();
    assert_eq!(
        original.definition,
        app.create_idempotent(command(), "request-1")
            .await
            .unwrap()
            .definition
    );
    for change in 0..7 {
        let mut altered = command();
        match change {
            0 => altered.name.push('!'),
            1 => altered.description.push('!'),
            2 => altered.remote_url.push('2'),
            3 => altered.initial_branch = Some("release".into()),
            4 => {
                altered.initial_branch = None;
                altered.initial_commit = Some("a".repeat(40));
            }
            5 => altered.clone_policy.depth = Some(2),
            _ => altered.credential_binding = Some("github-authorization".into()),
        }
        assert!(
            matches!(
                app.create_idempotent(altered, "request-1").await,
                Err(ResourceRegistryError::AlreadyRegistered(_))
            ),
            "R2/{change}"
        );
    }
    let mut next = original.config.clone();
    next.version = ConfigVersion(2);
    next.initial_branch = Some("release".into());
    app.registry
        .publish_repository_config(PublishRepositoryConfig {
            workspace_id: "workspace-a".into(),
            expected_current: ConfigVersion::INITIAL,
            config: next.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        app.create_idempotent(command(), "request-1")
            .await
            .unwrap()
            .config,
        next,
        "R3"
    );
    for state in [
        ResourceState::Suspended,
        ResourceState::Archived,
        ResourceState::Deleted,
    ] {
        app.registry
            .change_repository_state(ChangeRepositoryState {
                workspace_id: "workspace-a".into(),
                id: original.definition.id.clone(),
                state,
                preparation: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(
                app.create_idempotent(command(), "request-1").await,
                Err(ResourceRegistryError::NotActive { .. })
            ),
            "R4/{state:?}"
        );
    }
}

#[tokio::test]
async fn workspace_scoping_and_session_ownership_apply_to_both_list_and_get() {
    let app = application().await;
    let own = app.create_idempotent(command(), "same-key").await.unwrap();
    let mut foreign = command();
    foreign.workspace_id = "workspace-b".into();
    let other = app.create_idempotent(foreign, "same-key").await.unwrap();
    assert_ne!(own.definition.id, other.definition.id);
    assert!(
        app.get("workspace-b", own.definition.id.as_str())
            .await
            .unwrap()
            .is_none()
    );
    for (id, marker) in [
        ("managed:session:repository:one", None),
        ("profiled:session:repository:two", None),
        (
            "legacy-session-repository",
            Some(awaken_resource_contract::SESSION_REPOSITORY_OWNER_KIND),
        ),
        (
            "partial-session-repository",
            Some(awaken_resource_contract::SESSION_REPOSITORY_OWNER_SESSION),
        ),
    ] {
        let mut definition = own.definition.clone();
        definition.id = id.into();
        if let Some(marker) = marker {
            definition.metadata.insert(marker.into(), "session".into());
        }
        let mut config = own.config.clone();
        config.repository_id = id.into();
        app.registry
            .register_repository(RegisterRepository {
                definition,
                initial_config: config,
            })
            .await
            .unwrap();
        assert!(app.get("workspace-a", id).await.unwrap().is_none());
    }
    assert_eq!(app.list("workspace-a").await.unwrap(), vec![own.definition]);
}

#[tokio::test]
async fn private_repository_admission_happens_before_any_registry_write() {
    let app = application().await;
    let mut private = command();
    private.credential_binding = Some("foreign-or-expired".into());
    assert!(
        app.create_idempotent(private.clone(), "private")
            .await
            .is_err()
    );
    assert!(app.list("workspace-a").await.unwrap().is_empty());
    private.credential_binding = Some("github-authorization".into());
    let saved = app.create_idempotent(private, "private").await.unwrap();
    assert_eq!(
        saved.config.credential_binding.as_deref(),
        Some("github-authorization")
    );
}

#[test]
fn authoring_rejects_secret_urls_invalid_git_refs_and_ambiguous_revision_selection() {
    for remote in [
        "http://github.com/a/b",
        "https://token@github.com/a/b",
        "https://user:token@github.com/a/b",
        "https://github.com/a/b?token=private",
        "https://github.com/a/b#token",
        "file:///tmp/repo",
        "git@github.com:a/b",
        "https://github.com/",
        " https://github.com/a/b",
        "https://github.com/a/\nb",
    ] {
        let mut input = command();
        input.remote_url = remote.into();
        let error = validate(&input, "key").unwrap_err().to_string();
        assert!(!error.contains(remote), "Never echo secret-bearing URLs");
    }
    for branch in [
        "",
        "-main",
        "refs//main",
        "main.lock",
        "feature/.hidden",
        "main.",
        "a..b",
        "@{-1}",
        "HEAD",
        "a b",
        "a:b",
        "a\\b",
        "/main",
        "main/",
        "a\u{7f}b",
    ] {
        let mut input = command();
        input.initial_branch = Some(branch.into());
        assert!(
            validate(&input, "key").is_err(),
            "invalid branch {branch:?}"
        );
    }
    for branch in ["main", "feature/workshop", "版本/演示", "@", "foo./bar"] {
        let mut input = command();
        input.initial_branch = Some(branch.into());
        assert!(validate(&input, "key").is_ok());
    }
    for commit in ["abc123".into(), "g".repeat(40), "a".repeat(41)] {
        let mut input = command();
        input.initial_branch = None;
        input.initial_commit = Some(commit);
        assert!(validate(&input, "key").is_err());
    }
    for length in [40, 64] {
        let mut input = command();
        input.initial_commit = Some("a".repeat(length));
        assert!(
            validate(&input, "key").is_err(),
            "branch and commit are exclusive"
        );
        input.initial_branch = None;
        assert!(validate(&input, "key").is_ok());
    }
    for key in ["", "has space", "line\nbreak", "中文"] {
        assert!(validate(&command(), key).is_err());
    }
    assert!(validate(&command(), &"a".repeat(256)).is_err());
    let mut input = command();
    input.clone_policy.depth = Some(0);
    assert!(validate(&input, "key").is_err());
}
