//! Live Postgres admin-store conformance (feature `postgres`): the same sync
//! store ports the sqlite backend serves, exercised against a real Postgres.
//! Isolated in its own schema (baked into the connection URL's `search_path`), so
//! it coexists with any other schema in the test database. Skips when no Postgres
//! is reachable (`AWAKEN_TEST_DATABASE_URL`).
#![cfg(feature = "postgres")]

use awaken_admin_config_api::PostgresAdminStore;
use awaken_config_resolver::{
    AgentInputBindingRepository, AgentInputConfig, BindingId, InferenceProfile,
    InferenceProfileStore, InputBinding, InputResourceId, MemoryStoreId, ModelTarget,
    ProfileCandidate, ResourceAccess,
};
use awaken_credential_vault::CredentialBinding;
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
        primary: ProfileCandidate {
            target: ModelTarget::unqualified(model),
            credential_binding: CredentialBinding::None,
        },
        fallbacks: Vec::new(),
        disabled_endpoint_ids: vec![],
    }
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
    assert!(InferenceProfileStore::get(&store, "p1").unwrap().is_none());
    InferenceProfileStore::put(&store, "p1".into(), profile("m1")).unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1")
            .unwrap()
            .unwrap()
            .primary
            .target
            .model_id,
        "m1"
    );
    InferenceProfileStore::put(&store, "p1".into(), profile("m2")).unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1")
            .unwrap()
            .unwrap()
            .primary
            .target
            .model_id,
        "m2"
    );

    // AgentInputBindingRepository: Workspace-scoped round-trip + overwrite.
    assert!(store.get_agent_inputs("ws", "agent-1").unwrap().is_none());
    let rc = AgentInputConfig {
        agent_id: "agent-1".into(),
        environment: None,
        inputs: vec![InputBinding {
            binding_id: BindingId::from("memory"),
            target: InputResourceId::MemoryStore(MemoryStoreId::from("memstore-7")),
            mount_path: "/mnt/memory/prefs".into(),
            access: ResourceAccess::ReadWrite,
            instructions: Some("user preferences".into()),
        }],
        revision: 1,
    };
    store.put_agent_inputs("ws", rc.clone()).unwrap();
    assert_eq!(
        store.get_agent_inputs("ws", "agent-1").unwrap(),
        Some(rc.clone())
    );
    let mut v2 = rc.clone();
    v2.revision = 2;
    v2.inputs.clear();
    store.put_agent_inputs("ws", v2.clone()).unwrap();
    assert_eq!(store.get_agent_inputs("ws", "agent-1").unwrap(), Some(v2));
    assert!(
        store
            .get_agent_inputs("other", "agent-1")
            .unwrap()
            .is_none()
    );
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
        InferenceProfileStore::put(&store, "p1".into(), profile("m1")).unwrap();
    }
    let store = tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&url).unwrap())
        .await
        .unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1")
            .unwrap()
            .unwrap()
            .primary
            .target
            .model_id,
        "m1"
    );
}
