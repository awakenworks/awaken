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
    ProfileCandidate, ResourceAccess, WebhookDeliveryOutcome, WebhookDeliveryState,
    WebhookEndpointDef, WebhookMutationIntent, WebhookStore,
};
use awaken_credential_vault::{CredentialBinding, SecretRef};
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

fn commit_webhook(store: &dyn WebhookStore, definition: WebhookEndpointDef) {
    let intent = WebhookMutationIntent::create(definition);
    store.begin_mutation(intent.clone()).unwrap();
    store.apply_mutation(&intent).unwrap();
    store.complete_mutation(&intent).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_input_first_writers_have_one_authoritative_winner() {
    // C1 absent row; C2 eight different revision-1 contents; C3 all writers
    // overlap before INSERT. R1 exactly one applies, seven conflict; R2 replay
    // of its exact content succeeds; R3 rejected writes never replace the winner.
    // A transaction-owned gate in a BEFORE INSERT trigger freezes the first
    // writer. Observe all eight database sessions waiting on locks before
    // releasing it: this forces the missing-row race without timing sleeps in
    // production. The fixed adapter queues seven writers BEFORE their SELECT;
    // the broken adapter queues all eight AFTER reading absence.
    let Some(url) = schema_url("t_admin_input_first_writers").await else {
        assert!(
            std::env::var_os("AWAKEN_TEST_DATABASE_URL").is_none(),
            "an explicitly configured database must be reachable"
        );
        return;
    };
    let control = PgPool::connect(&url).await.unwrap();
    let writer_url = format!("{url}&application_name=awaken_input_first_writers");
    let store = std::sync::Arc::new(PostgresAdminStore::connect(&writer_url).unwrap());
    control.execute("CREATE FUNCTION hold_input_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(89142761); RETURN NEW; END $$").await.unwrap();
    control.execute("CREATE TRIGGER hold_input_insert BEFORE INSERT ON admin_agent_resource FOR EACH ROW EXECUTE FUNCTION hold_input_insert()").await.unwrap();
    let mut gate = control.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(89142761)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let writers: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            tokio::task::spawn_blocking(move || {
                let config = AgentInputConfig {
                    agent_id: "agent".into(),
                    environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                        environment_id: format!("env-{i}"),
                        revision: 1,
                    }),
                    inputs: Vec::new(),
                    revision: 1,
                };
                let result = store.put_agent_inputs("workspace", config.clone());
                (config, result)
            })
        })
        .collect();
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name = 'awaken_input_first_writers' AND wait_event_type = 'Lock'")
                .fetch_one(&control).await.unwrap();
            if count == 8 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await;
    // Release even on timeout so fixture failure cannot strand writer threads.
    gate.commit().await.unwrap();
    let mut outcomes = Vec::new();
    for writer in writers {
        outcomes.push(writer.await.unwrap());
    }
    waiting.expect("all eight real database writers must reach the controlled overlap");
    let winners: Vec<_> = outcomes
        .iter()
        .filter(|(_, result)| result.is_ok())
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "first creation must not silently overwrite another revision-1 value"
    );
    let winner = winners[0].0.clone();
    for (_, result) in outcomes.iter().filter(|(_, result)| result.is_err()) {
        assert_eq!(
            result,
            &Err(
                awaken_config_resolver::AgentInputRepositoryError::RevisionConflict {
                    current: 1,
                    attempted: 1
                }
            )
        );
    }
    assert_eq!(
        store.get_agent_inputs("workspace", "agent").unwrap(),
        Some(winner.clone())
    );
    store.put_agent_inputs("workspace", winner.clone()).unwrap();
    assert_eq!(
        store.get_agent_inputs("workspace", "agent").unwrap(),
        Some(winner)
    );
    control.close().await;
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

    // WebhookStore cause/effect decision table: R1 failure below threshold ->
    // durable Active(1); R2 second failure -> atomically Disabled(2); R3 success
    // after operator re-enable -> counter reset (the domain transition is shared
    // with memory/SQLite; this proves the Postgres transaction boundary).
    commit_webhook(
        &store,
        WebhookEndpointDef {
            id: "wh1".into(),
            workspace_id: "ws".into(),
            url: "https://hooks.example/hook".into(),
            event_types: vec![],
            disabled: false,
            consecutive_failures: 0,
            secret_ref: SecretRef("whsec:wh1".into()),
        },
    );
    assert_eq!(
        store
            .record_delivery("wh1", WebhookDeliveryOutcome::Failed, 2)
            .unwrap(),
        WebhookDeliveryState::Active {
            consecutive_failures: 1
        },
        "R1"
    );
    assert_eq!(
        store
            .record_delivery("wh1", WebhookDeliveryOutcome::Failed, 2)
            .unwrap(),
        WebhookDeliveryState::Disabled {
            consecutive_failures: 2
        },
        "R2"
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
        commit_webhook(
            &store,
            WebhookEndpointDef {
                id: "wh1".into(),
                workspace_id: "ws".into(),
                url: "https://hooks.example/hook".into(),
                event_types: vec![],
                disabled: false,
                consecutive_failures: 0,
                secret_ref: SecretRef("whsec:wh1".into()),
            },
        );
        store
            .record_delivery("wh1", WebhookDeliveryOutcome::Failed, 20)
            .unwrap();
        let pending = WebhookMutationIntent::create(WebhookEndpointDef {
            id: "wh_pending".into(),
            workspace_id: "ws".into(),
            url: "https://hooks.example/pending".into(),
            event_types: vec![],
            disabled: false,
            consecutive_failures: 0,
            secret_ref: SecretRef("sec:webhook:pending".into()),
        });
        store.begin_mutation(pending).unwrap();
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
    assert_eq!(
        WebhookStore::get(&store, "wh1")
            .unwrap()
            .unwrap()
            .consecutive_failures,
        1,
        "the failure counter survives a Postgres reconnect"
    );
    let pending = store.pending_mutations().unwrap();
    assert_eq!(pending.len(), 1, "the material intent survives reconnect");
    assert_eq!(pending[0].id().unwrap(), "wh_pending");
}
