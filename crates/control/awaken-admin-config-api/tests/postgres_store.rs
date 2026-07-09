//! Live Postgres admin-store conformance (feature `postgres`): the same three sync
//! store ports the sqlite backend serves — [`InferenceProfileStore`], [`McpStore`],
//! [`ResourceStore`] — exercised against a real Postgres.
//! Isolated in its own schema (baked into the connection URL's `search_path`), so
//! it coexists with any other schema in the test database. Skips when no Postgres
//! is reachable (`AWAKEN_TEST_DATABASE_URL`).
#![cfg(feature = "postgres")]

use awaken_admin_config_api::PostgresAdminStore;
use awaken_config_resolver::{
    AgentMcpConfig, AgentResourceConfig, InferenceProfile, InferenceProfileStore, McpServerDef,
    McpServerId, McpStore, ResourceAccess, ResourceBinding, ResourceKind, ResourceStore,
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
        model_id: model.to_string(),
        credential_binding: CredentialBinding::None,
        disabled_endpoint_ids: vec![],
    }
}

fn server(id: &str) -> McpServerDef {
    McpServerDef {
        id: McpServerId(id.to_string()),
        display_name: id.to_string(),
        url: format!("http://{id}.example/"),
        credential_binding: CredentialBinding::None,
        version: 1,
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
    assert!(InferenceProfileStore::get(&store, "p1").is_none());
    store.put("p1".into(), profile("m1"));
    assert_eq!(
        InferenceProfileStore::get(&store, "p1").unwrap().model_id,
        "m1"
    );
    store.put("p1".into(), profile("m2"));
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

    // ResourceStore: agent resource binding round-trip + overwrite.
    assert!(store.get_agent_resource("agent-1").is_none());
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
    ResourceStore::put_agent_resource(&store, rc.clone());
    assert_eq!(store.get_agent_resource("agent-1").unwrap(), rc);
    let mut v2 = rc.clone();
    v2.version = 2;
    v2.resources.clear();
    ResourceStore::put_agent_resource(&store, v2.clone());
    assert_eq!(store.get_agent_resource("agent-1").unwrap(), v2);
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
        store.put("p1".into(), profile("m1"));
        store.put_server(server("calc"));
    }
    let store = tokio::task::spawn_blocking(move || PostgresAdminStore::connect(&url).unwrap())
        .await
        .unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1").unwrap().model_id,
        "m1"
    );
    assert_eq!(store.list_servers().len(), 1);
}
