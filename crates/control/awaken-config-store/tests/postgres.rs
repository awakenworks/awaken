//! Live Postgres config-store CRUD. Skips when no Postgres is reachable; isolates
//! each test in a fresh schema (the store takes no prefix; ADR-0029/ADR-0031).

use awaken_config_store::{
    AgentConfig, ConfigRegistry, PostgresConfigStore, PublicationState, ScopeId,
    ScopedConfigRegistry, StoredPublication, compile,
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
        ..Default::default()
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

/// Regression: Postgres is a durable store, so `list_published_scoped` must reload
/// published publications (it previously fell through to the empty default trait
/// impl, silently breaking warm-install after a restart on Postgres deployments).
/// Substrate-bound: skips when no Postgres is reachable.
#[tokio::test]
async fn postgres_list_published_reloads_published_rows_of_the_scope() {
    let Some(pool) = schema_pool("t_config_listpub").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");
    let a = ScopeId::from("ws_a");
    let b = ScopeId::from("ws_b");

    let tools = vec![ToolDescriptor::pinned(
        "test",
        "echo",
        "Echo",
        serde_json::json!({"type": "object"}),
    )];
    let publication_for = |id: &str| -> StoredPublication {
        let cfg = AgentConfig {
            id: id.to_string(),
            instructions: format!("body-{id}"),
            max_steps: 8,
            model_binding: awaken_config_store::ModelSelection::pinned("p", "m", "b"),
            tool_ids: vec!["echo".to_string()],
            ..Default::default()
        };
        StoredPublication::published(compile(&cfg, &tools).expect("compile"), &cfg.id)
    };

    let p1 = publication_for("a1");
    let p2 = publication_for("a2");
    let mut p_compiled = publication_for("a3");
    p_compiled.state = PublicationState::Compiled; // excluded from list_published
    let p_b = publication_for("b1");

    store.put_publication_scoped(&a, &p1).await.expect("p1");
    store.put_publication_scoped(&a, &p2).await.expect("p2");
    store
        .put_publication_scoped(&a, &p_compiled)
        .await
        .expect("p_compiled");
    store.put_publication_scoped(&b, &p_b).await.expect("p_b");

    let a_ids: Vec<String> = store
        .list_published_scoped(&a)
        .await
        .expect("list a")
        .into_iter()
        .map(|p| p.fingerprint)
        .collect();
    // Both published A rows are reloaded (the previous empty default would fail here);
    // the compiled row is excluded.
    assert_eq!(a_ids.len(), 2);
    assert!(a_ids.contains(&p1.fingerprint));
    assert!(a_ids.contains(&p2.fingerprint));

    // Scope isolation on the list path.
    let b_list = store.list_published_scoped(&b).await.expect("list b");
    assert_eq!(b_list.len(), 1);
    assert_eq!(b_list[0].fingerprint, p_b.fingerprint);
}

/// TASK 1 (pg equivalent): multiple publications for the SAME agent id. SQLite orders
/// `list_published_scoped` by `rowid ASC` (a monotonic per-row integer), giving a
/// deterministic oldest-first reload and a deterministic "latest per agent" warm-load;
/// Postgres orders by `created_at ASC`, which has NO tie-break key. This test pins the
/// DETERMINISTIC guarantee (every published row of the agent is reloaded — set parity
/// with sqlite) and characterizes the non-deterministic ordering below. Substrate-bound:
/// skips when no Postgres is reachable.
#[tokio::test]
async fn postgres_list_published_warm_load_same_agent_reloads_the_full_set() {
    let Some(pool) = schema_pool("t_config_warmload").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");
    let a = ScopeId::from("ws_a");

    let tools = vec![ToolDescriptor::pinned(
        "test",
        "echo",
        "Echo",
        serde_json::json!({"type": "object"}),
    )];
    // Same agent id "dup", distinct instructions → distinct content-address fingerprints.
    let pub_v = |instr: &str| -> StoredPublication {
        let cfg = AgentConfig {
            id: "dup".to_string(),
            instructions: instr.to_string(),
            max_steps: 8,
            model_binding: awaken_config_store::ModelSelection::pinned("p", "m", "b"),
            tool_ids: vec!["echo".to_string()],
            ..Default::default()
        };
        StoredPublication::published(compile(&cfg, &tools).expect("compile"), &cfg.id)
    };

    let v1 = pub_v("v1");
    let v2 = pub_v("v2");
    let v3 = pub_v("v3");
    store.put_publication_scoped(&a, &v1).await.expect("v1");
    store.put_publication_scoped(&a, &v2).await.expect("v2");
    store.put_publication_scoped(&a, &v3).await.expect("v3");

    let listed = store.list_published_scoped(&a).await.expect("list");
    // Every published row of the agent is reloaded (set parity with sqlite).
    let got: std::collections::BTreeSet<String> =
        listed.iter().map(|p| p.fingerprint.clone()).collect();
    let want: std::collections::BTreeSet<String> = [
        v1.fingerprint.clone(),
        v2.fingerprint.clone(),
        v3.fingerprint.clone(),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want, "pg must reload every published row of the agent");

    // Deterministic tie-break: the monotonic `seq` identity column (migration V0005)
    // gives Postgres a total insertion order — `ORDER BY created_at ASC, seq ASC` —
    // even when same-instant inserts share a `created_at`. So the list comes back in
    // the exact insertion order (v1, v2, v3), the Postgres analogue of SQLite's
    // `rowid ASC`, making the warm-load's "latest publication per agent" (the last
    // element) deterministic and matching across backends.
    let ordered: Vec<String> = listed.iter().map(|p| p.fingerprint.clone()).collect();
    assert_eq!(
        ordered,
        vec![
            v1.fingerprint.clone(),
            v2.fingerprint.clone(),
            v3.fingerprint.clone(),
        ],
        "pg must reload publications in deterministic insertion order (tie-break by seq)"
    );
}
