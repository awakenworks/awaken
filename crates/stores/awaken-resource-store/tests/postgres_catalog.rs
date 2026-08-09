//! Live Postgres conformance for the Resources-owned catalog adapter.
#![cfg(feature = "postgres")]

use std::time::{SystemTime, UNIX_EPOCH};

use awaken_resource_contract::{
    ConfigVersion, ExtractionPolicy, MemoryStoreConfigVersion, MemoryStoreDefinition, RecallPolicy,
    ResourceCatalog, ResourceConfigSource, ResourceState, RetentionPolicy,
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
            recall_policy: RecallPolicy::default(),
            extraction_policy: ExtractionPolicy::default(),
            retention_policy: RetentionPolicy::default(),
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn resources_catalog_owns_schema_cas_and_restart_durability() {
    // Cause/effect decision table:
    // R1 fresh Resources schema -> both Resources bundles migrate and V1 data writes.
    // R2 wrong Workspace/CAS predecessor -> fail without changing the aggregate.
    // R3 correct predecessor -> V2 becomes current while V1 remains addressable.
    // R4 reconnect through the same Resources adapter -> the V2 state survives;
    // no Control/Admin store participates in any rule.
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
    second.recall_policy.max_results = 20;
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
}
