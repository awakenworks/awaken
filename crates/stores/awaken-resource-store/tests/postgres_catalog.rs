//! Live Postgres conformance for the Resources-owned catalog adapter.
#![cfg(feature = "postgres")]

use std::time::{SystemTime, UNIX_EPOCH};

use awaken_resource_contract::{
    ClonePolicy, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
    RepositoryConfigVersion, RepositoryDefinition, ResourceCatalog, ResourceConfigSource,
    ResourceState, RetentionPolicy,
};
use awaken_resource_store::PostgresResourceStore;
use sqlx::Executor;
use sqlx::postgres::PgPool;

fn base_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_owned()
    })
}

async fn isolated_url() -> Option<String> {
    let admin = match PgPool::connect(&base_url()).await {
        Ok(pool) => pool,
        Err(error) => {
            println!("[skip] no Postgres reachable: {error}");
            return None;
        }
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let schema = format!("t_resource_catalog_{}_{}", std::process::id(), nonce);
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create isolated schema");
    admin.close().await;
    let separator = if base_url().contains('?') { '&' } else { '?' };
    Some(format!(
        "{}{}options=-c%20search_path%3D{}",
        base_url(),
        separator,
        schema
    ))
}

fn memory() -> (MemoryStoreDefinition, MemoryStoreConfigVersion) {
    (
        MemoryStoreDefinition {
            id: "memory-1".into(),
            workspace_id: "ws".into(),
            name: "Memory".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        MemoryStoreConfigVersion {
            memory_store_id: "memory-1".into(),
            version: ConfigVersion::INITIAL,
            retention_policy: RetentionPolicy::default(),
        },
    )
}

fn repository() -> (RepositoryDefinition, RepositoryConfigVersion) {
    (
        RepositoryDefinition {
            id: "repository-1".into(),
            workspace_id: "ws".into(),
            name: "Repository".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        RepositoryConfigVersion {
            repository_id: "repository-1".into(),
            version: ConfigVersion::INITIAL,
            remote_url: "https://example.invalid/one.git".into(),
            credential_binding: Some("vault://repo-token".into()),
            initial_branch: Some("main".into()),
            initial_commit: None,
            clone_policy: ClonePolicy { depth: Some(1) },
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn resources_catalog_owns_schema_cas_and_restart_durability() {
    // Test design — aggregate state machine, applied to every catalog aggregate
    // (MemoryStore and Repository): Created(V1) --publish(expected V1,V2)-->
    // Active(V2) --restart--> Active(V2), while immutable V1 remains addressable.
    // Wrong Workspace or stale predecessor is a no-op. This is backend-contract
    // testing: the same domain transitions are asserted at the Postgres boundary.
    let Some(url) = isolated_url().await else {
        return;
    };
    let store = PostgresResourceStore::connect(&url)
        .await
        .expect("open Resources store");
    let (definition, initial) = memory();
    store
        .create_memory_store(definition, initial)
        .expect("R1 create");
    let mut second = store
        .memory_config("ws", "memory-1", ConfigVersion::INITIAL)
        .expect("R1 read")
        .expect("R1 config");
    second.version = ConfigVersion(2);
    second.retention_policy.retention_days = Some(20);
    assert!(
        store
            .publish_memory_config("other", ConfigVersion::INITIAL, second.clone())
            .is_err(),
        "R2 workspace fence"
    );
    store
        .publish_memory_config("ws", ConfigVersion::INITIAL, second)
        .expect("R3 publish");
    assert_eq!(
        store
            .resolve_memory_store("ws", "memory-1")
            .expect("R3 resolve")
            .version,
        ConfigVersion(2)
    );

    let (repository, initial_repository) = repository();
    store
        .create_repository(repository, initial_repository)
        .expect("R1 create repository");
    let mut second_repository = store
        .repository_config("ws", "repository-1", ConfigVersion::INITIAL)
        .expect("R1 read repository")
        .expect("R1 repository config");
    second_repository.version = ConfigVersion(2);
    second_repository.remote_url = "https://example.invalid/two.git".into();
    assert!(
        store
            .publish_repository_config("other", ConfigVersion::INITIAL, second_repository.clone(),)
            .is_err(),
        "R2 repository workspace fence"
    );
    store
        .publish_repository_config("ws", ConfigVersion::INITIAL, second_repository)
        .expect("R3 publish repository");
    assert_eq!(
        store
            .resolve_repository("ws", "repository-1")
            .expect("R3 resolve repository")
            .version,
        ConfigVersion(2)
    );
    drop(store);

    let reopened = PostgresResourceStore::connect_existing(&url)
        .await
        .expect("R4 verify and reopen");
    assert_eq!(
        reopened
            .resolve_memory_store("ws", "memory-1")
            .expect("R4 durable resolve")
            .version,
        ConfigVersion(2)
    );
    assert_eq!(
        reopened
            .repository_config("ws", "repository-1", ConfigVersion::INITIAL)
            .expect("R4 old repository config read")
            .expect("R4 immutable repository V1")
            .remote_url,
        "https://example.invalid/one.git"
    );
    assert_eq!(
        reopened
            .resolve_repository("ws", "repository-1")
            .expect("R4 durable repository resolve")
            .remote_url,
        "https://example.invalid/two.git"
    );
}
