//! Cross-backend contract for the composed Resource Registry.

use awaken_resource_contract::{
    ChangeMemoryStoreState, ChangeRepositoryState, ClonePolicy, ConfigVersion,
    MemoryStoreConfigVersion, MemoryStoreDefinition, PublishMemoryStoreConfig,
    PublishRepositoryConfig, RegisterMemoryStore, RegisterRepository, RepositoryConfigVersion,
    RepositoryDefinition, ResourceRegistry, ResourceRegistryError, ResourceState, RetentionPolicy,
    UpdateMemoryStoreProfile,
};
use awaken_resource_persistence::{SchemaMode, open_embedded, open_postgres};

struct Fixture {
    workspace: String,
    memory_id: String,
    repository_id: String,
}

fn fixture() -> Fixture {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock follows the Unix epoch")
        .as_nanos();
    Fixture {
        workspace: format!("registry-contract-{}-{nonce}", std::process::id()),
        memory_id: format!("memory-{}-{nonce}", std::process::id()),
        repository_id: format!("repository-{}-{nonce}", std::process::id()),
    }
}

fn exercise_registry(registry: &dyn ResourceRegistry, fixture: &Fixture) {
    // Cause/effect decision table shared by every backend:
    // R1 absent + aligned V1 -> registered; duplicate -> AlreadyRegistered.
    // R2 wrong Workspace -> hidden and no mutation.
    // R3 expected V1 + next V2 -> current V2 while immutable V1 remains valid.
    // R4 stale predecessor -> ConfigConflict and no V3.
    // R5 Suspended -> inventory remains visible but execution/binding fail closed.
    // R6 Repository follows the same ownership/version rules as MemoryStore.
    let definition = MemoryStoreDefinition {
        id: fixture.memory_id.as_str().into(),
        workspace_id: fixture.workspace.clone(),
        name: "Memory".into(),
        description: String::new(),
        metadata: Default::default(),
        state: ResourceState::Active,
        current_config_version: ConfigVersion::INITIAL,
        timestamps: Default::default(),
    };
    let initial = MemoryStoreConfigVersion {
        memory_store_id: fixture.memory_id.as_str().into(),
        version: ConfigVersion::INITIAL,
        retention_policy: RetentionPolicy::default(),
    };
    registry
        .register_memory_store(RegisterMemoryStore {
            definition: definition.clone(),
            initial_config: initial.clone(),
        })
        .expect("R1 register MemoryStore");
    assert!(matches!(
        registry.register_memory_store(RegisterMemoryStore {
            definition,
            initial_config: initial,
        }),
        Err(ResourceRegistryError::AlreadyRegistered(_))
    ));
    assert!(
        registry
            .find_memory_store("another-workspace", &fixture.memory_id)
            .expect("R2 workspace-scoped lookup")
            .is_none()
    );
    registry
        .update_memory_store_profile(UpdateMemoryStoreProfile {
            workspace_id: fixture.workspace.clone(),
            id: fixture.memory_id.as_str().into(),
            name: "Renamed Memory".into(),
            description: "profile".into(),
            metadata: [("owner".into(), "registry".into())].into(),
        })
        .expect("R2 update profile");
    let mut second = registry
        .find_memory_store_config(
            &fixture.workspace,
            &fixture.memory_id,
            ConfigVersion::INITIAL,
        )
        .expect("R3 read V1")
        .expect("R3 V1 exists");
    second.version = ConfigVersion(2);
    second.retention_policy.retention_days = Some(30);
    assert!(matches!(
        registry.publish_memory_store_config(PublishMemoryStoreConfig {
            workspace_id: "another-workspace".into(),
            expected_current: ConfigVersion::INITIAL,
            config: second.clone(),
        }),
        Err(ResourceRegistryError::NotFound(_))
    ));
    registry
        .publish_memory_store_config(PublishMemoryStoreConfig {
            workspace_id: fixture.workspace.clone(),
            expected_current: ConfigVersion::INITIAL,
            config: second,
        })
        .expect("R3 publish V2");
    let mut third = registry
        .find_memory_store_config(&fixture.workspace, &fixture.memory_id, ConfigVersion(2))
        .expect("R4 read V2")
        .expect("R4 V2 exists");
    third.version = ConfigVersion(3);
    assert!(matches!(
        registry.publish_memory_store_config(PublishMemoryStoreConfig {
            workspace_id: fixture.workspace.clone(),
            expected_current: ConfigVersion::INITIAL,
            config: third,
        }),
        Err(ResourceRegistryError::ConfigConflict { .. })
    ));
    registry
        .verify_memory_binding(
            &fixture.workspace,
            &fixture.memory_id,
            ConfigVersion::INITIAL,
        )
        .expect("R3 immutable V1 remains bound");
    registry
        .change_memory_store_state(ChangeMemoryStoreState {
            workspace_id: fixture.workspace.clone(),
            id: fixture.memory_id.as_str().into(),
            state: ResourceState::Suspended,
        })
        .expect("R5 suspend MemoryStore");
    assert!(matches!(
        registry.resolve_memory_store(&fixture.workspace, &fixture.memory_id),
        Err(ResourceRegistryError::NotActive { .. })
    ));
    assert_eq!(
        registry
            .list_memory_stores(&fixture.workspace)
            .expect("R5 list inventory")
            .len(),
        1
    );

    let repository = RepositoryDefinition {
        id: fixture.repository_id.as_str().into(),
        workspace_id: fixture.workspace.clone(),
        name: "Repository".into(),
        description: String::new(),
        metadata: Default::default(),
        state: ResourceState::Active,
        current_config_version: ConfigVersion::INITIAL,
        timestamps: Default::default(),
    };
    let initial_repository = RepositoryConfigVersion {
        repository_id: fixture.repository_id.as_str().into(),
        version: ConfigVersion::INITIAL,
        remote_url: "https://example.invalid/one.git".into(),
        credential_binding: Some("vault://repository-token".into()),
        initial_branch: Some("main".into()),
        initial_commit: None,
        clone_policy: ClonePolicy { depth: Some(1) },
    };
    registry
        .register_repository(RegisterRepository {
            definition: repository.clone(),
            initial_config: initial_repository.clone(),
        })
        .expect("R6 register Repository");
    assert_eq!(
        registry
            .list_repositories(&fixture.workspace)
            .expect("R7 inventory"),
        vec![
            registry
                .find_repository(&fixture.workspace, &fixture.repository_id)
                .unwrap()
                .unwrap()
        ]
    );
    assert!(
        registry
            .list_repositories("another-workspace")
            .expect("R7 isolation")
            .is_empty()
    );
    let mut second_repository = initial_repository;
    second_repository.version = ConfigVersion(2);
    second_repository.remote_url = "https://example.invalid/two.git".into();
    registry
        .publish_repository_config(PublishRepositoryConfig {
            workspace_id: fixture.workspace.clone(),
            expected_current: ConfigVersion::INITIAL,
            config: second_repository,
        })
        .expect("R6 publish Repository V2");
    assert_eq!(
        registry
            .resolve_repository(&fixture.workspace, &fixture.repository_id)
            .expect("R6 resolve Repository")
            .version,
        ConfigVersion(2)
    );
    assert_eq!(
        registry.list_repositories(&fixture.workspace).unwrap()[0].current_config_version,
        ConfigVersion(2)
    );
    assert_eq!(
        registry
            .find_repository_config(
                &fixture.workspace,
                &fixture.repository_id,
                ConfigVersion::INITIAL
            )
            .unwrap()
            .unwrap()
            .remote_url,
        "https://example.invalid/one.git"
    );
    let retired_id = format!("{}-retired", fixture.repository_id);
    let mut retired = repository;
    retired.id = retired_id.as_str().into();
    registry
        .register_repository(RegisterRepository {
            definition: retired,
            initial_config: RepositoryConfigVersion {
                repository_id: retired_id.as_str().into(),
                version: ConfigVersion::INITIAL,
                remote_url: "https://github.com/example/retired.git".into(),
                credential_binding: None,
                initial_branch: None,
                initial_commit: None,
                clone_policy: Default::default(),
            },
        })
        .expect("R7 second repository");
    registry
        .change_repository_state(ChangeRepositoryState {
            workspace_id: fixture.workspace.clone(),
            id: retired_id.as_str().into(),
            state: ResourceState::Suspended,
        })
        .expect("R7 suspended");
    let inventory = registry.list_repositories(&fixture.workspace).unwrap();
    assert_eq!(inventory.len(), 2);
    assert!(
        inventory.windows(2).all(|pair| pair[0].id < pair[1].id),
        "R7 stable order"
    );
    assert!(
        registry
            .resolve_repository(&fixture.workspace, &retired_id)
            .is_err(),
        "R7 listing grants no execution"
    );
    registry
        .change_repository_state(ChangeRepositoryState {
            workspace_id: fixture.workspace.clone(),
            id: retired_id.as_str().into(),
            state: ResourceState::Deleted,
        })
        .expect("R7 deleted");
    assert_eq!(
        registry
            .list_repositories(&fixture.workspace)
            .unwrap()
            .len(),
        1,
        "R7 deleted rows hidden"
    );
}

fn verify_reopen(registry: &dyn ResourceRegistry, fixture: &Fixture) {
    // Restart oracle: logical profile, current pointer, and immutable history
    // must be recovered exactly; lifecycle still prevents execution.
    let definition = registry
        .find_memory_store(&fixture.workspace, &fixture.memory_id)
        .expect("reopen MemoryStore")
        .expect("durable MemoryStore");
    assert_eq!(definition.name, "Renamed Memory");
    assert_eq!(definition.current_config_version, ConfigVersion(2));
    assert_eq!(definition.state, ResourceState::Suspended);
    assert!(
        registry
            .find_memory_store_config(
                &fixture.workspace,
                &fixture.memory_id,
                ConfigVersion::INITIAL,
            )
            .expect("reopen V1")
            .is_some()
    );
    assert_eq!(
        registry
            .resolve_repository(&fixture.workspace, &fixture.repository_id)
            .expect("reopen Repository")
            .version,
        ConfigVersion(2)
    );
    let repositories = registry
        .list_repositories(&fixture.workspace)
        .expect("reopen repository inventory");
    assert_eq!(repositories.len(), 1);
    assert_eq!(repositories[0].id.as_str(), fixture.repository_id);
    assert_eq!(repositories[0].current_config_version, ConfigVersion(2));
}

fn replay_request(workspace: &str) -> RegisterMemoryStore {
    RegisterMemoryStore {
        definition: MemoryStoreDefinition {
            id: "memory-idempotency".into(),
            workspace_id: workspace.into(),
            name: "Original name".into(),
            description: "Original description".into(),
            metadata: [("owner".into(), "workshop".into())].into(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        initial_config: MemoryStoreConfigVersion {
            memory_store_id: "memory-idempotency".into(),
            version: ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        },
    }
}

#[test]
fn embedded_memory_creation_receipt_is_atomic_and_survives_edits_and_reopen() {
    // Lost response + concurrent retry must converge on one identity. The
    // retained receipt, not today's editable profile, decides payload equality.
    let root = tempfile::tempdir().unwrap();
    let request = replay_request("workspace-replay");
    let first;
    {
        let application = open_embedded(root.path()).unwrap();
        let registry = application.authorities().resource_registry();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let registry = registry.clone();
                let request = request.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    registry.register_memory_store_idempotent(request).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        first = results[0].clone();
        assert!(results.iter().all(|result| result == &first));
        assert_eq!(
            registry
                .list_memory_stores("workspace-replay")
                .unwrap()
                .len(),
            1
        );
        for variation in 0..6 {
            let mut changed = request.clone();
            match variation {
                0 => changed.definition.name.push('!'),
                1 => changed.definition.description.push('!'),
                2 => {
                    changed
                        .definition
                        .metadata
                        .insert("owner".into(), "changed".into());
                }
                3 => changed.initial_config.retention_policy.retention_days = Some(7),
                4 => changed.definition.state = ResourceState::Suspended,
                _ => changed.definition.workspace_id = "foreign-workspace".into(),
            }
            assert!(matches!(
                registry.register_memory_store_idempotent(changed),
                Err(ResourceRegistryError::AlreadyRegistered(_))
            ));
        }
        registry
            .update_memory_store_profile(UpdateMemoryStoreProfile {
                workspace_id: first.workspace_id.clone(),
                id: first.id.clone(),
                name: "Edited later".into(),
                description: "Keep this edit".into(),
                metadata: Default::default(),
            })
            .unwrap();
        let mut config = request.initial_config.clone();
        config.version = ConfigVersion(2);
        config.retention_policy.retention_days = Some(30);
        registry
            .publish_memory_store_config(PublishMemoryStoreConfig {
                workspace_id: first.workspace_id.clone(),
                expected_current: ConfigVersion::INITIAL,
                config,
            })
            .unwrap();
    }
    let reopened = open_embedded(root.path()).unwrap();
    let registry = reopened.authorities().resource_registry();
    let mut retry = request.clone();
    retry.definition.timestamps = awaken_resource_contract::ResourceTimestamps::created(12345);
    let replay = registry.register_memory_store_idempotent(retry).unwrap();
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.name, "Edited later");
    assert_eq!(replay.description, "Keep this edit");
    assert_eq!(replay.current_config_version, ConfigVersion(2));
    assert_eq!(
        replay.timestamps.created_unix_nanos,
        first.timestamps.created_unix_nanos
    );
    for state in [
        ResourceState::Suspended,
        ResourceState::Archived,
        ResourceState::Deleted,
    ] {
        registry
            .change_memory_store_state(ChangeMemoryStoreState {
                workspace_id: first.workspace_id.clone(),
                id: first.id.clone(),
                state,
            })
            .unwrap();
        assert!(matches!(
            registry.register_memory_store_idempotent(request.clone()),
            Err(ResourceRegistryError::NotActive { .. })
        ));
    }
    assert!(
        registry
            .list_memory_stores(&first.workspace_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn repository_creation_receipt_is_atomic_durable_and_does_not_adopt_legacy_rows() {
    let root = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let request = RegisterRepository {
        definition: RepositoryDefinition {
            id: fixture.repository_id.as_str().into(),
            workspace_id: fixture.workspace.clone(),
            name: "Workshop repository".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        initial_config: RepositoryConfigVersion {
            repository_id: fixture.repository_id.as_str().into(),
            version: ConfigVersion::INITIAL,
            remote_url: "https://github.com/example/workshop.git".into(),
            credential_binding: Some("github-source".into()),
            initial_branch: Some("main".into()),
            initial_commit: None,
            clone_policy: ClonePolicy::default(),
        },
    };
    {
        let application = open_embedded(root.path()).unwrap();
        let registry = application.authorities().resource_registry();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                let request = request.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.register_repository_idempotent(request).unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap().id, request.definition.id);
        }
        assert_eq!(
            registry
                .list_repositories(&fixture.workspace)
                .unwrap()
                .len(),
            1
        );
        let mut legacy = request.clone();
        legacy.definition.id = "legacy-repository-without-receipt".into();
        legacy.initial_config.repository_id = legacy.definition.id.clone();
        registry.register_repository(legacy.clone()).unwrap();
        assert!(matches!(
            registry.register_repository_idempotent(legacy),
            Err(ResourceRegistryError::AlreadyRegistered(_))
        ));
        let mut config = request.initial_config.clone();
        config.version = ConfigVersion(2);
        config.initial_branch = Some("release".into());
        registry
            .publish_repository_config(PublishRepositoryConfig {
                workspace_id: fixture.workspace.clone(),
                expected_current: ConfigVersion::INITIAL,
                config,
            })
            .unwrap();
    }
    let reopened = open_embedded(root.path()).unwrap();
    let registry = reopened.authorities().resource_registry();
    let mut retry = request.clone();
    retry.definition.timestamps = awaken_resource_contract::ResourceTimestamps::created(1234);
    let replay = registry.register_repository_idempotent(retry).unwrap();
    assert_eq!(replay.current_config_version, ConfigVersion(2));
    assert_eq!(
        registry
            .find_repository_config(&fixture.workspace, &fixture.repository_id, ConfigVersion(2))
            .unwrap()
            .unwrap()
            .initial_branch
            .as_deref(),
        Some("release")
    );
    let mut changed = request.clone();
    changed.definition.workspace_id = "foreign".into();
    assert!(matches!(
        registry.register_repository_idempotent(changed),
        Err(ResourceRegistryError::AlreadyRegistered(_))
    ));
    for state in [
        ResourceState::Suspended,
        ResourceState::Archived,
        ResourceState::Deleted,
    ] {
        registry
            .change_repository_state(ChangeRepositoryState {
                workspace_id: fixture.workspace.clone(),
                id: request.definition.id.clone(),
                state,
            })
            .unwrap();
        assert!(matches!(
            registry.register_repository_idempotent(request.clone()),
            Err(ResourceRegistryError::NotActive { .. })
        ));
    }
}

#[test]
fn embedded_registry_satisfies_the_shared_contract_and_survives_reopen() {
    let root = tempfile::tempdir().expect("embedded Registry root");
    let fixture = fixture();
    {
        let application = open_embedded(root.path()).expect("open embedded Resources");
        exercise_registry(
            application.authorities().resource_registry().as_ref(),
            &fixture,
        );
    }
    let reopened = open_embedded(root.path()).expect("reopen embedded Resources");
    verify_reopen(
        reopened.authorities().resource_registry().as_ref(),
        &fixture,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_registry_satisfies_the_same_contract_and_survives_reconnect() {
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return;
    };
    let fixture = fixture();
    {
        let application = open_postgres(&url, SchemaMode::Migrate)
            .await
            .expect("open PostgreSQL Resources");
        exercise_registry(
            application.authorities().resource_registry().as_ref(),
            &fixture,
        );
    }
    let reopened = open_postgres(&url, SchemaMode::Verify)
        .await
        .expect("reconnect PostgreSQL Resources");
    verify_reopen(
        reopened.authorities().resource_registry().as_ref(),
        &fixture,
    );
}
