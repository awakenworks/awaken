//! Live Postgres conformance for the Resources-owned Registry repository.
#![cfg(feature = "postgres")]

use std::time::{SystemTime, UNIX_EPOCH};

use awaken_resource_contract::{
    ClonePolicy, ConfigVersion, InsertOutcome, MemoryStoreAggregate, MemoryStoreConfigVersion,
    MemoryStoreDefinition, ReplaceOutcome, RepositoryAggregate, RepositoryConfigVersion,
    RepositoryDefinition, ResourceRegistryRepository, ResourceState, RetentionPolicy,
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
    let schema = format!("t_resource_registry_{}_{}", std::process::id(), nonce);
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
async fn resource_registry_repository_owns_schema_cas_and_restart_durability() {
    // Repository contract and CAS state model:
    // absent --insert--> revision 1 --replace(1)--> revision 2;
    // replace(1) after revision 2 must be a no-op. Reconnect must recover the
    // exact aggregate and revision. The same rules apply to both aggregate kinds.
    let Some(url) = isolated_url().await else {
        return;
    };
    let store = PostgresResourceStore::connect(&url)
        .await
        .expect("open Resources store");
    let (definition, initial) = memory();
    let aggregate = MemoryStoreAggregate::register(definition, initial)
        .expect("R1 valid MemoryStore aggregate");
    assert_eq!(
        store.insert_memory_store(&aggregate).expect("R1 insert"),
        InsertOutcome::Inserted
    );
    let stored = store
        .load_memory_store("memory-1")
        .expect("R1 load")
        .expect("R1 aggregate");
    let mut updated = stored.aggregate;
    let mut second = updated
        .config(ConfigVersion::INITIAL)
        .expect("R1 initial config")
        .clone();
    second.version = ConfigVersion(2);
    second.retention_policy.retention_days = Some(20);
    updated
        .publish_config("ws", ConfigVersion::INITIAL, second, 2)
        .expect("R2 apply domain transition");
    assert_eq!(
        store
            .replace_memory_store(stored.revision, &updated)
            .expect("R2 replace"),
        ReplaceOutcome::Replaced {
            revision: stored.revision.checked_next().expect("R2 next revision")
        }
    );
    assert_eq!(
        store
            .replace_memory_store(stored.revision, &aggregate)
            .expect("R3 stale replace is classified"),
        ReplaceOutcome::ConcurrentModification
    );

    let (repository, initial_repository) = repository();
    let repository = RepositoryAggregate::register(repository, initial_repository)
        .expect("R4 valid Repository aggregate");
    assert_eq!(
        store
            .insert_repository(&repository)
            .expect("R4 insert repository"),
        InsertOutcome::Inserted
    );
    let stored_repository = store
        .load_repository("repository-1")
        .expect("R4 load repository")
        .expect("R4 repository aggregate");
    let mut updated_repository = stored_repository.aggregate;
    let mut second_repository = updated_repository
        .config(ConfigVersion::INITIAL)
        .expect("R4 initial repository config")
        .clone();
    second_repository.version = ConfigVersion(2);
    second_repository.remote_url = "https://example.invalid/two.git".into();
    updated_repository
        .publish_config("ws", ConfigVersion::INITIAL, second_repository, 2)
        .expect("R5 apply repository transition");
    assert!(matches!(
        store
            .replace_repository(stored_repository.revision, &updated_repository)
            .expect("R5 replace repository"),
        ReplaceOutcome::Replaced { .. }
    ));
    drop(store);

    let reopened = PostgresResourceStore::connect_existing(&url)
        .await
        .expect("R4 verify and reopen");
    assert_eq!(
        reopened
            .load_memory_store("memory-1")
            .expect("R6 durable load")
            .expect("R6 durable aggregate")
            .aggregate
            .definition()
            .current_config_version,
        ConfigVersion(2)
    );
    assert_eq!(
        reopened
            .load_repository("repository-1")
            .expect("R6 durable repository load")
            .expect("R6 durable repository aggregate")
            .aggregate
            .config(ConfigVersion::INITIAL)
            .expect("R6 immutable repository V1")
            .remote_url,
        "https://example.invalid/one.git"
    );
    assert_eq!(
        reopened
            .load_repository("repository-1")
            .expect("R6 durable repository load")
            .expect("R6 durable repository aggregate")
            .aggregate
            .config(ConfigVersion(2))
            .expect("R6 durable repository V2")
            .remote_url,
        "https://example.invalid/two.git"
    );
}
