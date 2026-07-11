//! Live Postgres config-store CRUD. Skips when no Postgres is reachable; isolates
//! each test in a fresh schema (the store takes no prefix; ADR-0029/ADR-0031).

use awaken_config_store::{
    AgentConfig, ConfigRegistry, PostgresConfigStore, StoredPublication, compile,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};

fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

async fn schema_pool(schema: &'static str) -> Option<PgPool> {
    let admin = match PgPool::connect(&database_url()).await {
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
    PgPoolOptions::new()
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                conn.execute(format!("SET search_path = {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url())
        .await
        .ok()
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        id: "agent-1".to_string(),
        instructions: "be helpful".to_string(),
        max_steps: 8,
        model_binding: awaken_config_store::ModelSelection::pinned("p", "m", "b"),
        tool_ids: vec!["echo".to_string()],
        model_candidates: Vec::new(),
        plugin_ids: Vec::new(),
        plugin_config: Default::default(),
        context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        tool_patterns: Vec::new(),
    }
}

#[tokio::test]
async fn postgres_config_store_round_trips_config_and_publication() {
    let Some(pool) = schema_pool("t_config").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");

    let config = agent_config();
    store.put_config(&config).await.expect("put config");
    assert_eq!(
        store.get_config("agent-1").await.unwrap().as_ref(),
        Some(&config)
    );
    assert!(store.get_config("missing").await.unwrap().is_none());

    let tools = vec![ToolDescriptor::pinned(
        "test",
        "echo",
        "Echo",
        serde_json::json!({"type": "object"}),
    )];
    let publication = compile(&config, &tools).expect("compile");
    let stored = StoredPublication::published(publication.clone(), &config.id);
    store
        .put_publication(&stored)
        .await
        .expect("put publication");
    // Idempotent by fingerprint.
    store.put_publication(&stored).await.expect("re-put");

    let loaded = store
        .get_publication(&publication.snapshot().fingerprint.0)
        .await
        .unwrap()
        .expect("publication exists");
    assert_eq!(loaded.fingerprint, publication.snapshot().fingerprint.0);
    assert_eq!(
        loaded.snapshot.fingerprint.0,
        publication.snapshot().fingerprint.0
    );
}
