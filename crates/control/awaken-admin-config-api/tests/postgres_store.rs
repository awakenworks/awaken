//! Live Postgres admin-store conformance (feature `postgres`): the same three sync
//! store ports the sqlite backend serves — [`InferenceProfileStore`], [`McpStore`],
//! [`AgentInputBindingRepository`] — exercised against a real Postgres.
//! Isolated in its own schema (baked into the connection URL's `search_path`), so
//! it coexists with any other schema in the test database. Skips when no Postgres
//! is reachable (`AWAKEN_TEST_DATABASE_URL`).
#![cfg(feature = "postgres")]

use awaken_admin_config_api::PostgresAdminStore;
use awaken_config_resolver::{
    AgentInputBindingRepository, AgentMcpConfig, AgentResourceConfig, InferenceProfile,
    InferenceProfileStore, McpServerDef, McpServerId, McpStore, ResourceAccess, ResourceBinding,
    ResourceKind,
};
use awaken_credential_vault::CredentialBinding;
use awaken_resource_contract::{
    ClonePolicy, ConfigVersion, ExtractionPolicy, MemoryStoreConfigVersion, MemoryStoreDefinition,
    RecallPolicy, RepositoryConfigVersion, RepositoryDefinition, ResourceCatalog,
    ResourceCatalogError, ResourceConfigSource, ResourceState, RetentionPolicy,
};
use sqlx::Executor;
use sqlx::postgres::PgPool;

fn base_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

/// Drop+recreate `schema`, then return a connection URL whose `search_path` is
/// that schema (via libpq `options`), or `None` when Postgres is unreachable.
async fn schema_url(schema: &str) -> Option<String> {
    let admin = match PgPool::connect(&base_url()).await {
        Ok(pool) => pool,
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            return None;
        }
    };
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    admin.close().await;
    let sep = if base_url().contains('?') { '&' } else { '?' };
    Some(format!(
        "{}{}options=-c%20search_path%3D{}",
        base_url(),
        sep,
        schema
    ))
}

fn profile(model: &str) -> InferenceProfile {
    InferenceProfile {
        workspace_id: "ws".into(),
        model_id: model.to_string(),
        model_fallbacks: Vec::new(),
        credential_binding: CredentialBinding::None,
        disabled_endpoint_ids: vec![],
    }
}

fn server(id: &str) -> McpServerDef {
    McpServerDef {
        workspace_id: "ws".into(),
        id: McpServerId(id.to_string()),
        display_name: id.to_string(),
        url: format!("http://{id}.example/"),
        credential_binding: CredentialBinding::None,
        version: 1,
    }
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

fn repository() -> (RepositoryDefinition, RepositoryConfigVersion) {
    (
        RepositoryDefinition {
            id: "repo-1".into(),
            workspace_id: "ws".into(),
            name: "Repository".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
        },
        RepositoryConfigVersion {
            repository_id: "repo-1".into(),
            version: ConfigVersion::INITIAL,
            remote_url: "https://example.test/repo.git".into(),
            credential_binding: Some("credential-1".into()),
            initial_branch: None,
            clone_policy: ClonePolicy::default(),
        },
    )
}

#[tokio::test]
async fn postgres_admin_store_serves_every_port() {
    let Some(url) = schema_url("t_admin_store").await else {
        return;
    };
    // `connect` is sync (it composes with the sync ports); run it off the async
    // test thread so its internal `block_on` never nests in this runtime.
    let store = tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&url).unwrap())
        .await
        .unwrap();

    // InferenceProfileStore: round-trip + overwrite.
    assert!(InferenceProfileStore::get(&store, "p1").is_none());
    InferenceProfileStore::put(&store, "p1".into(), profile("m1"));
    assert_eq!(
        InferenceProfileStore::get(&store, "p1").unwrap().model_id,
        "m1"
    );
    InferenceProfileStore::put(&store, "p1".into(), profile("m2"));
    assert_eq!(
        InferenceProfileStore::get(&store, "p1").unwrap().model_id,
        "m2"
    );

    // McpStore: round-trip, sorted list, agent binding.
    store.put_server(server("zeta"));
    store.put_server(server("alpha"));
    assert_eq!(
        store.get_server("zeta").unwrap().url,
        "http://zeta.example/"
    );
    assert!(store.get_server("missing").is_none());
    let ids: Vec<String> = store.list_servers().into_iter().map(|s| s.id.0).collect();
    assert_eq!(ids, vec!["alpha".to_string(), "zeta".to_string()]);
    let cfg = AgentMcpConfig {
        workspace_id: "ws".into(),
        agent_id: "agent-1".into(),
        mcp_server_ids: vec![McpServerId("alpha".into())],
        version: 1,
    };
    store.put_agent_config(cfg.clone());
    assert_eq!(
        store.get_agent_config("agent-1").unwrap().mcp_server_ids,
        cfg.mcp_server_ids
    );
    assert!(store.get_agent_config("agent-2").is_none());

    // AgentInputBindingRepository: Workspace-scoped round-trip + overwrite.
    assert!(store.get_agent_inputs("ws", "agent-1").is_none());
    let rc = AgentResourceConfig {
        agent_id: "agent-1".into(),
        resources: vec![ResourceBinding {
            kind: ResourceKind::MemoryStore,
            resource_id: "memstore-7".into(),
            mount_path: "/mnt/memory/prefs".into(),
            access: ResourceAccess::ReadWrite,
            instructions: Some("user preferences".into()),
        }],
        version: 1,
    };
    store.put_agent_inputs("ws", rc.clone());
    assert_eq!(store.get_agent_inputs("ws", "agent-1").unwrap(), rc);
    let mut v2 = rc.clone();
    v2.version = 2;
    v2.resources.clear();
    store.put_agent_inputs("ws", v2.clone());
    assert_eq!(store.get_agent_inputs("ws", "agent-1").unwrap(), v2);
    assert!(store.get_agent_inputs("other", "agent-1").is_none());

    // ResourceCatalog: resource/config separation, monotonic CAS and Workspace
    // hiding. This port contains no authorization subject or policy input.
    let (definition, initial) = memory();
    store.create_memory_store(definition, initial).unwrap();
    let mut second = store
        .memory_config("ws", "memory-1", ConfigVersion(1))
        .unwrap();
    second.version = ConfigVersion(2);
    second.recall_policy.max_results = 20;
    assert!(matches!(
        store.publish_memory_config("other", ConfigVersion(1), second.clone()),
        Err(ResourceCatalogError::NotFound(_))
    ));
    store
        .publish_memory_config("ws", ConfigVersion(1), second)
        .unwrap();
    assert_eq!(
        store
            .resolve_memory_store("ws", "memory-1")
            .unwrap()
            .config
            .version,
        ConfigVersion(2)
    );
    assert!(store.memory_store("other", "memory-1").is_none());
    let mut updated = store.memory_store("ws", "memory-1").unwrap();
    updated.name = "Renamed".into();
    store.update_memory_store(updated).unwrap();
    assert_eq!(store.list_memory_stores("ws")[0].name, "Renamed");
    assert!(store.list_memory_stores("other").is_empty());

    let (definition, initial) = repository();
    store.create_repository(definition, initial).unwrap();
    store
        .set_repository_state("ws", "repo-1", ResourceState::Suspended)
        .unwrap();
    assert!(matches!(
        store.resolve_repository("ws", "repo-1"),
        Err(ResourceCatalogError::NotActive { .. })
    ));
}

/// Rows survive a fresh store handle on the same schema (durable + idempotent
/// migration), proving persistence across "restarts".
#[tokio::test]
async fn postgres_admin_rows_survive_a_reconnect() {
    let Some(url) = schema_url("t_admin_reconnect").await else {
        return;
    };
    {
        let u = url.clone();
        let store = tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&u).unwrap())
            .await
            .unwrap();
        InferenceProfileStore::put(&store, "p1".into(), profile("m1"));
        store.put_server(server("calc"));
        let (definition, initial) = memory();
        store.create_memory_store(definition, initial).unwrap();
    }
    let store = tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&url).unwrap())
        .await
        .unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1").unwrap().model_id,
        "m1"
    );
    assert_eq!(store.list_servers().len(), 1);
    assert_eq!(
        store
            .resolve_memory_store("ws", "memory-1")
            .unwrap()
            .config
            .version,
        ConfigVersion::INITIAL
    );
}

#[tokio::test]
async fn postgres_owned_legacy_memory_rows_migrate_but_unowned_rows_are_quarantined() {
    let Some(url) = schema_url("t_admin_legacy_memory").await else {
        return;
    };
    {
        let u = url.clone();
        tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&u).unwrap())
            .await
            .unwrap();
    }
    let pool = PgPool::connect(&url).await.unwrap();
    for (id, data) in [
        (
            "legacy-owned",
            serde_json::json!({
                "id": "legacy-owned",
                "workspace_id": "ws",
                "name": "Legacy",
                "description": "old row",
                "metadata": {"source": "v6"},
                "archived": false
            }),
        ),
        (
            "legacy-unowned",
            serde_json::json!({
                "id": "legacy-unowned",
                "name": "Quarantined",
                "archived": false
            }),
        ),
    ] {
        sqlx::query("INSERT INTO admin_memory_store (id, data) VALUES ($1, $2)")
            .bind(id)
            .bind(sqlx::types::Json(data))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;

    let store = tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&url).unwrap())
        .await
        .unwrap();
    assert_eq!(
        store.memory_store("ws", "legacy-owned").unwrap().name,
        "Legacy"
    );
    assert!(store.memory_store("ws", "legacy-unowned").is_none());
}
