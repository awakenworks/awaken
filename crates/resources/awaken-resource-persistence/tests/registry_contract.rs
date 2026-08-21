//! Cross-backend contract for the composed Resource Registry.

use awaken_resource_contract::{
    ChangeMemoryStoreState, ClonePolicy, ConfigVersion, MemoryStoreConfigVersion,
    MemoryStoreDefinition, PublishMemoryStoreConfig, PublishRepositoryConfig, RegisterMemoryStore,
    RegisterRepository, RepositoryConfigVersion, RepositoryDefinition, ResourceRegistry,
    ResourceRegistryError, ResourceState, RetentionPolicy, UpdateMemoryStoreProfile,
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
            definition: repository,
            initial_config: initial_repository.clone(),
        })
        .expect("R6 register Repository");
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
