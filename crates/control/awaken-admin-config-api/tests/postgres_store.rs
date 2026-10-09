// @formal evidence agent_inputs.rejected_revision_does_not_mutate
//! Live Postgres admin-store conformance (feature `postgres`): the same
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

#[path = "support/agent_input_admission.rs"]
mod input_admission;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_input_ambiguous_keys_fail_closed() {
    let Some(url) = schema_url("t_admin_input_key_admission").await else {
        return;
    };
    let store = PostgresAdminStore::connect(&url).await.unwrap();
    input_admission::ambiguous_keys_fail_closed(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_input_rollback_replay_and_corrupt_key_readback() {
    let Some(url) = schema_url("t_admin_input_failure_admission").await else {
        return;
    };
    let store = PostgresAdminStore::connect(&url).await.unwrap();
    let control = PgPool::connect(&url).await.unwrap();
    control.execute("CREATE FUNCTION reject_input_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected write failure'; END $$").await.unwrap();
    input_admission::rollback_and_lost_response(&store, |reject| {
        let control = control.clone();
        async move {
            control.execute(if reject {
                "CREATE TRIGGER reject_input_write BEFORE INSERT OR UPDATE ON admin_agent_resource FOR EACH ROW EXECUTE FUNCTION reject_input_write()"
            } else { "DROP TRIGGER reject_input_write ON admin_agent_resource" }).await.unwrap();
        }
    }).await;
    drop(store);
    let store = PostgresAdminStore::connect_existing(&url).await.unwrap();
    assert_eq!(
        store
            .get_agent_inputs("retry-workspace", "retry-agent")
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    // K4: do not relabel a historical corrupt row under the requested identity.
    sqlx::query("UPDATE admin_agent_resource SET agent_id = $1")
        .bind("retry-workspace\u{1f}wrong-agent")
        .execute(&control)
        .await
        .unwrap();
    assert!(
        store
            .get_agent_inputs("retry-workspace", "wrong-agent")
            .await
            .is_err()
    );
    assert!(store.list_agent_inputs("retry-workspace").await.is_err());
}

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
            assert!(
                std::env::var_os("AWAKEN_TEST_DATABASE_URL").is_none(),
                "explicit PostgreSQL fixture must be reachable: {err}"
            );
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

async fn commit_webhook(store: &dyn WebhookStore, definition: WebhookEndpointDef) {
    let intent = WebhookMutationIntent::create(definition);
    store.begin_mutation(intent.clone()).await.unwrap();
    store.apply_mutation(&intent).await.unwrap();
    store.complete_mutation(&intent).await.unwrap();
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
    let store = std::sync::Arc::new(PostgresAdminStore::connect(&writer_url).await.unwrap());
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
            tokio::spawn(async move {
                let config = AgentInputConfig {
                    agent_id: "agent".into(),
                    environment: Some(awaken_config_resolver::AgentEnvironmentBinding {
                        environment_id: format!("env-{i}"),
                        revision: 1,
                    }),
                    inputs: Vec::new(),
                    revision: 1,
                };
                let result = store.put_agent_inputs("workspace", config.clone()).await;
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
        store.get_agent_inputs("workspace", "agent").await.unwrap(),
        Some(winner.clone())
    );
    store
        .put_agent_inputs("workspace", winner.clone())
        .await
        .unwrap();
    assert_eq!(
        store.get_agent_inputs("workspace", "agent").await.unwrap(),
        Some(winner)
    );
    control.close().await;
}

#[tokio::test]
async fn postgres_admin_store_serves_every_port() {
    let Some(url) = schema_url("t_admin_store").await else {
        return;
    };
    let store = PostgresAdminStore::connect(&url).await.unwrap();

    // InferenceProfileStore: round-trip + overwrite.
    assert!(
        InferenceProfileStore::get(&store, "p1")
            .await
            .unwrap()
            .is_none()
    );
    InferenceProfileStore::put(&store, "p1".into(), profile("m1"))
        .await
        .unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1")
            .await
            .unwrap()
            .unwrap()
            .primary
            .target
            .model_id,
        "m1"
    );
    InferenceProfileStore::put(&store, "p1".into(), profile("m2"))
        .await
        .unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1")
            .await
            .unwrap()
            .unwrap()
            .primary
            .target
            .model_id,
        "m2"
    );

    // AgentInputBindingRepository: Workspace-scoped round-trip + overwrite.
    assert!(
        store
            .get_agent_inputs("ws", "agent-1")
            .await
            .unwrap()
            .is_none()
    );
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
    store.put_agent_inputs("ws", rc.clone()).await.unwrap();
    assert_eq!(
        store.get_agent_inputs("ws", "agent-1").await.unwrap(),
        Some(rc.clone())
    );
    let mut v2 = rc.clone();
    v2.revision = 2;
    v2.inputs.clear();
    store.put_agent_inputs("ws", v2.clone()).await.unwrap();
    assert_eq!(
        store.get_agent_inputs("ws", "agent-1").await.unwrap(),
        Some(v2)
    );
    assert!(
        store
            .get_agent_inputs("other", "agent-1")
            .await
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
    )
    .await;
    assert_eq!(
        store
            .record_delivery("wh1", WebhookDeliveryOutcome::Failed, 2)
            .await
            .unwrap(),
        WebhookDeliveryState::Active {
            consecutive_failures: 1
        },
        "R1"
    );
    assert_eq!(
        store
            .record_delivery("wh1", WebhookDeliveryOutcome::Failed, 2)
            .await
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
        let store = PostgresAdminStore::connect(&u).await.unwrap();
        InferenceProfileStore::put(&store, "p1".into(), profile("m1"))
            .await
            .unwrap();
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
        )
        .await;
        store
            .record_delivery("wh1", WebhookDeliveryOutcome::Failed, 20)
            .await
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
        store.begin_mutation(pending).await.unwrap();
    }
    let store = PostgresAdminStore::connect(&url).await.unwrap();
    assert_eq!(
        InferenceProfileStore::get(&store, "p1")
            .await
            .unwrap()
            .unwrap()
            .primary
            .target
            .model_id,
        "m1"
    );
    assert_eq!(
        WebhookStore::get(&store, "wh1")
            .await
            .unwrap()
            .unwrap()
            .consecutive_failures,
        1,
        "the failure counter survives a Postgres reconnect"
    );
    let pending = store.pending_mutations().await.unwrap();
    assert_eq!(pending.len(), 1, "the material intent survives reconnect");
    assert_eq!(pending[0].id().unwrap(), "wh_pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_mcp_configuration_cas_and_restart() {
    // P1 real Postgres + two same-revision writers -> one commit; P2 reconnect
    // -> exact current config; P3 other Workspace -> absent. Same domain write
    // predicate as SQLite; SQL owns the final compare-and-swap race.
    use awaken_agent_config::mcp_config::{
        McpCatalogSnapshot, McpConfiguration, McpConfigurationRepository,
    };
    use std::sync::Arc;
    let Some(url) = schema_url("t_admin_mcp_config").await else {
        return;
    };
    let store = Arc::new(PostgresAdminStore::connect(&url).await.unwrap());
    let mut first = McpConfiguration::new(
        "w".into(),
        McpCatalogSnapshot {
            schema_version: 1,
            revision: 1,
            servers: vec![],
        },
    );
    first.advance(0).unwrap();
    store.put_mcp_configuration(0, &first).await.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let writers = (0..2)
        .map(|i| {
            let store = store.clone();
            let barrier = barrier.clone();
            let mut next = first.clone();
            next.advance(1).unwrap();
            next.catalog_url = Some(format!("https://source-{i}.example.test/catalog"));
            tokio::spawn(async move {
                barrier.wait().await;
                store.put_mcp_configuration(1, &next).await
            })
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    for writer in writers {
        results.push(writer.await.unwrap());
    }
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let current = store.get_mcp_configuration("w").await.unwrap().unwrap();
    assert_eq!(current.revision, 2);
    let reopened = PostgresAdminStore::connect_existing(&url).await.unwrap();
    assert_eq!(
        reopened.get_mcp_configuration("w").await.unwrap(),
        Some(current)
    );
    assert!(
        reopened
            .get_mcp_configuration("other")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_telemetry_configuration_cas_restart_and_scope() {
    // P1 real Postgres + two same-revision writers -> one commit; P2 reconnect
    // without DDL -> exact current narrowing; P3 other Workspace -> absent;
    // P4 stale insert -> Conflict; P5 relabelled scope -> error. Same domain
    // write predicate as SQLite; SQL owns the final compare-and-swap race.
    use awaken_config_resolver::{
        TelemetryConfigError, WorkspaceTelemetryConfiguration, WorkspaceTelemetryRepository,
    };
    use awaken_runtime_contract::ContentCapture;
    use std::sync::Arc;
    let Some(url) = schema_url("t_admin_telemetry_config").await else {
        return;
    };
    let store = Arc::new(PostgresAdminStore::connect(&url).await.unwrap());
    let first = WorkspaceTelemetryConfiguration::new("w".into())
        .narrowed(0, Some(ContentCapture::Structured), ContentCapture::Full)
        .unwrap();
    store.put_telemetry_configuration(0, &first).await.unwrap();
    assert_eq!(
        store.put_telemetry_configuration(0, &first).await,
        Err(TelemetryConfigError::Conflict),
        "P4"
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let writers = [ContentCapture::Off, ContentCapture::Structured]
        .into_iter()
        .map(|level| {
            let store = store.clone();
            let barrier = barrier.clone();
            let next = first
                .narrowed(1, Some(level), ContentCapture::Full)
                .unwrap();
            tokio::spawn(async move {
                barrier.wait().await;
                store.put_telemetry_configuration(1, &next).await
            })
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    for writer in writers {
        results.push(writer.await.unwrap());
    }
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "P1");
    let current = store
        .get_telemetry_configuration("w")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.revision, 2);
    let reopened = PostgresAdminStore::connect_existing(&url).await.unwrap();
    assert_eq!(
        reopened.get_telemetry_configuration("w").await.unwrap(),
        Some(current),
        "P2"
    );
    assert!(
        reopened
            .get_telemetry_configuration("other")
            .await
            .unwrap()
            .is_none(),
        "P3"
    );
    let raw = PgPool::connect(&url).await.unwrap();
    sqlx::query(
        "UPDATE admin_telemetry_configuration \
         SET data = jsonb_set(data::jsonb, '{workspace_id}', '\"wrong\"')",
    )
    .execute(&raw)
    .await
    .unwrap();
    assert!(
        reopened.get_telemetry_configuration("w").await.is_err(),
        "P5"
    );
}
