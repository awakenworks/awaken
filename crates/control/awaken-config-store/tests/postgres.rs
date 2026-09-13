//! Live Postgres config-store CRUD. Skips when no Postgres is reachable; isolates
//! each test in a fresh schema (the store takes no prefix; ADR-0029/ADR-0031).

use awaken_agent_config::{
    AgentConfig, AuditedConfigWrite, ConfigRegistry, ConfigWrite, ManagementAuditRecord,
    ManagementEffect, PublicationState, PublicationWrite, ScopeId, ScopedConfigRegistry,
    StoredPublication, compile_resolved,
};
use awaken_config_store::PostgresConfigStore;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::sync::Arc;

#[path = "support/revision_cas.rs"]
mod revision_cas;

#[path = "support/audit_identity.rs"]
mod audit_identity;

#[path = "support/effect_retirement.rs"]
mod effect_retirement;

#[tokio::test]
async fn postgres_publication_source_admission_preserves_rejected_frame() {
    let Some(pool) = schema_pool("t_publication_source_admission").await else {
        return;
    };
    // Same P1-P4 effects through the real transaction and explicit rollback.
    revision_cas::publication_source_table(&PostgresConfigStore::with_pool(pool).await.unwrap())
        .await;
}

#[tokio::test]
async fn postgres_management_effect_stale_completion_preserves_replacement() {
    let Some(pool) = schema_pool("t_effect_retirement_aba").await else {
        return;
    };
    effect_retirement::stale_completion_preserves_replacement(
        &PostgresConfigStore::with_pool(pool).await.unwrap(),
        &agent_config(),
    )
    .await;
}

#[tokio::test]
async fn postgres_audit_identity_decision_table() {
    let Some(pool) = schema_pool("t_config_audit_identity").await else {
        return;
    };
    audit_identity::decision_table(
        &PostgresConfigStore::with_pool(pool).await.unwrap(),
        &agent_config(),
    )
    .await;
}

#[tokio::test]
async fn postgres_audit_identity_migration_preserves_truth_or_rolls_back() {
    // M1 pending/committed history -> preserve tuple/state/timestamp and reopen.
    // M2 corrupt signed state, malformed or duplicate record -> roll back and
    // omit V11 receipt. M3 index/record drift and corrupt states -> DB rejection.
    for (schema, state, valid_record, duplicate) in [
        ("t_audit_migrate_pending", 0_i64, true, false),
        ("t_audit_migrate_committed", 1, true, false),
        ("t_audit_migrate_bad_state", 2, true, false),
        ("t_audit_migrate_bad_record", 0, false, false),
        ("t_audit_migrate_duplicate", 0, true, true),
    ] {
        let Some(pool) = schema_pool(schema).await else {
            return;
        };
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            "config",
        )
        .unwrap()
        .run_bundle(&audit_identity::compact_history())
        .await
        .unwrap();
        let audit = ManagementAuditRecord {
            tool: "http:POST:a".into(),
            call_id: "b:调用".into(),
            summary: "body_sha256=one".into(),
        };
        let record = if valid_record {
            serde_json::to_value(&audit).unwrap()
        } else {
            serde_json::json!({})
        };
        sqlx::query("INSERT INTO config_management_audit(scope_id,call_id,record,business_committed,created_at) VALUES('ws','http:POST:a:b:调用',$1,$2,'2026-01-01 00:00:00+00')")
            .bind(&record).bind(state).execute(&pool).await.unwrap();
        if duplicate {
            sqlx::query("INSERT INTO config_management_audit(scope_id,call_id,record) VALUES('ws','other-legacy-key',$1)").bind(&record).execute(&pool).await.unwrap();
        }
        let result = PostgresConfigStore::with_pool(pool.clone()).await;
        if state == 2 || !valid_record || duplicate {
            assert!(result.is_err(), "M2");
            let row: (String, serde_json::Value, i64) = sqlx::query_as(
                "SELECT call_id,record,business_committed FROM config_management_audit WHERE call_id='http:POST:a:b:调用'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row, ("http:POST:a:b:调用".into(), record, state));
            let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM config_management_audit")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(rows.0, 1 + i64::from(duplicate));
            let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM config_schema_migrations WHERE bundle_id='awaken.config' AND version=11").fetch_one(&pool).await.unwrap();
            assert_eq!(count.0, 0);
            let shadow: (bool,) = sqlx::query_as(
                "SELECT to_regclass('config_management_audit_identity') IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!shadow.0);
            continue;
        }
        let store = result.unwrap();
        let scope = ScopeId::from("ws");
        let entry = store
            .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.record, audit);
        assert_eq!(entry.business_committed, state == 1);
        assert_eq!(
            store
                .record_management_audit_scoped(&scope, &audit)
                .await
                .unwrap(),
            AuditedConfigWrite::Replayed
        );
        let timestamp: (bool,) = sqlx::query_as("SELECT created_at = '2026-01-01 00:00:00+00'::timestamptz FROM config_management_audit").fetch_one(&pool).await.unwrap();
        assert!(timestamp.0, "M1");
        for sql in [
            "UPDATE config_management_audit SET tool='wrong'",
            "UPDATE config_management_audit SET call_id='wrong'",
            "UPDATE config_management_audit SET business_committed=-1",
            "UPDATE config_management_audit SET record='{}'",
        ] {
            assert!(pool.execute(sql).await.is_err(), "M3 {sql}");
        }
        drop(store);
        PostgresConfigStore::with_existing_pool(pool.clone())
            .await
            .unwrap();
        PostgresConfigStore::with_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn postgres_config_cas_missing_row_and_revision_decision_table() {
    let Some(pool) = schema_pool("t_config_cas_table").await else {
        return;
    };
    // R6 separate adapters share the same schema; PgPool leases independent
    // physical connections to overlapping writes, with no application mutex.
    let store = Arc::new(PostgresConfigStore::with_pool(pool.clone()).await.unwrap());
    let other = Arc::new(PostgresConfigStore::with_existing_pool(pool).await.unwrap());
    revision_cas::decision_table(store.as_ref()).await;
    revision_cas::concurrent_writers(&[store.clone(), other.clone()]).await;
    revision_cas::mixed_unconditional_and_cas_writers(&[store, other]).await;
}

#[tokio::test]
async fn postgres_config_cas_exhaustion_preserves_reopenable_truth() {
    let Some(pool) = schema_pool("t_config_cas_exhausted").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool.clone()).await.unwrap();
    let scope = ScopeId::from("exhausted");
    let config = agent_config();
    store.put_config_scoped(&scope, &config).await.unwrap();
    sqlx::query("UPDATE config_agent SET generation = $1 WHERE scope_id = $2 AND id = $3")
        .bind(i64::MAX)
        .bind(&scope.0)
        .bind(&config.id)
        .execute(&pool)
        .await
        .unwrap();
    revision_cas::exhausted(&store, &config).await;
    drop(store);
    let reopened = PostgresConfigStore::with_existing_pool(pool).await.unwrap();
    let row = reopened
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.revision, i64::MAX as u64);
    assert_eq!(row.config, config);
}

#[tokio::test]
async fn postgres_config_history_insert_failure_rolls_back_all_write_paths() {
    // R8 fault belongs to this test's schema and exact history generation 2.
    // Real PostgreSQL must roll back the statement and the wider audited txn.
    let Some(pool) = schema_pool("t_config_history_fault").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool.clone()).await.unwrap();
    pool.execute("CREATE FUNCTION inject_history_failure() RETURNS TRIGGER AS $$ BEGIN RAISE EXCEPTION 'injected config history failure'; END; $$ LANGUAGE plpgsql").await.unwrap();
    pool.execute("CREATE TRIGGER inject_history_failure BEFORE INSERT ON config_agent_revision FOR EACH ROW WHEN (NEW.generation = 2) EXECUTE FUNCTION inject_history_failure()").await.unwrap();
    revision_cas::history_failure_rolls_back(&store).await;
}

fn compile(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
) -> Result<awaken_agent_config::ExecutableAgentSnapshot, awaken_agent_config::CompileError> {
    compile_resolved(config, tools, AgentSnapshotMetadata::default())
}

fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

#[tokio::test]
async fn postgres_audit_and_config_commit_are_atomic_replay_safe_and_scope_fenced() {
    let Some(pool) = schema_pool("t_config_audit").await else {
        return;
    };
    let effect = ManagementEffect::UpsertAgentInputs {
        config: awaken_agent_config::AgentInputConfig {
            agent_id: "agent-1".into(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");
    let scope = ScopeId::from("ws_a");
    let audit = ManagementAuditRecord {
        tool: "admin_draft_agent".into(),
        call_id: "call_pg_1".into(),
        summary: "draft agent `agent-1`".into(),
    };
    // Begin/final table (PostgreSQL parity): absent final fails closed; exact
    // pending begin applies once; committed exact replay is a no-op. This keeps
    // ON CONFLICT concurrency in the one record_management_audit owner.
    let absent_error = store
        .put_config_with_audit_effect_scoped(&scope, &agent_config(), 0, &audit, Some(&effect))
        .await
        .expect_err("A0 absent audit");
    assert!(absent_error.to_string().contains("pre-recorded"), "A0");
    assert_eq!(
        store
            .record_management_audit_scoped(&scope, &audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied
    );
    let config = agent_config();
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(&scope, &config, 0, &audit, Some(&effect))
            .await
            .unwrap(),
        AuditedConfigWrite::Applied
    );
    let generation = store
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap()
        .revision;
    assert_eq!(
        store
            .put_config_with_audit_scoped(&scope, &config, generation, &audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed
    );
    assert_eq!(
        store
            .get_config_revision_scoped(&scope, &config.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        generation
    );
    assert_eq!(
        store
            .pending_management_effects_scoped(&scope)
            .await
            .unwrap(),
        vec![effect.clone()]
    );
    let ManagementEffect::UpsertAgentInputs { config: applied } = &effect;
    let receipt = effect.confirm_readback(&scope, Some(applied)).unwrap();
    store
        .complete_management_effect_scoped(&receipt)
        .await
        .unwrap();

    let other = ScopeId::from("ws_b");
    let other_audit = ManagementAuditRecord {
        tool: "admin_draft_agent".into(),
        call_id: "call_pg_2".into(),
        summary: "draft agent `agent-1`".into(),
    };
    store
        .record_management_audit_scoped(&other, &other_audit)
        .await
        .unwrap();
    assert_eq!(
        store
            .put_config_with_audit_scoped(&other, &config, 0, &other_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied
    );
    assert_eq!(
        store
            .get_config_scoped(&other, &config.id)
            .await
            .unwrap()
            .as_ref(),
        Some(&config)
    );

    // PostgreSQL parity for the shared generation/replay decision table:
    // | rule | ordering | effect |
    // | G1 | audited commit -> archive -> exact retry | Replayed; archive retained |
    // | G3 | archive r2 -> stale audited r1          | Conflict; audit pending, zero config |
    // | G4 | archive r2 -> stale audit+effect r1     | Conflict; audit pending, zero config/effect |
    let replay_scope = ScopeId::from("pg_audit_replay_order");
    store
        .put_config_scoped(&replay_scope, &config)
        .await
        .unwrap();
    let replay_audit = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "pg-response-lost".into(),
        summary: "patch before archive".into(),
    };
    let mut first_patch = config.clone();
    first_patch.instructions = "audited revision".into();
    assert_eq!(
        store
            .record_management_audit_scoped(&replay_scope, &replay_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "G1 begin"
    );
    assert_eq!(
        store
            .put_config_with_audit_scoped(&replay_scope, &first_patch, 1, &replay_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "G1 initial commit"
    );
    let mut archived = first_patch.clone();
    archived.archived_at = Some("2026-08-30T00:00:00Z".into());
    assert!(matches!(
        store
            .put_config_if_revision_scoped(&replay_scope, &archived, 2)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 3 }
    ));
    assert_eq!(
        store
            .put_config_with_audit_scoped(&replay_scope, &first_patch, 3, &replay_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed,
        "G1 replay must precede lifecycle admission"
    );
    assert!(
        store
            .get_config_revision_scoped(&replay_scope, &config.id)
            .await
            .unwrap()
            .unwrap()
            .config
            .archived_at
            .is_some(),
        "G1"
    );

    let conflict_scope = ScopeId::from("pg_archive_wins");
    store
        .put_config_scoped(&conflict_scope, &config)
        .await
        .unwrap();
    let mut archive_winner = config.clone();
    archive_winner.archived_at = Some("2026-08-30T00:00:00Z".into());
    assert!(matches!(
        store
            .put_config_if_revision_scoped(&conflict_scope, &archive_winner, 1)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 2 }
    ));
    let mut late_patch = config.clone();
    late_patch.instructions = "must not revive".into();
    let late_audit = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "pg-late-audit".into(),
        summary: "must conflict".into(),
    };
    assert_eq!(
        store
            .record_management_audit_scoped(&conflict_scope, &late_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "G3 begin"
    );
    assert_eq!(
        store
            .put_config_with_audit_scoped(&conflict_scope, &late_patch, 1, &late_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Conflict {
            current_revision: Some(2)
        },
        "G3"
    );
    assert!(
        store
            .get_management_audit_scoped(&conflict_scope, &late_audit.tool, &late_audit.call_id,)
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "G3 pending audit intent is not a business commit"
    );
    let effect_audit = ManagementAuditRecord {
        call_id: "pg-late-effect".into(),
        ..late_audit
    };
    assert_eq!(
        store
            .record_management_audit_scoped(&conflict_scope, &effect_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "G4 begin"
    );
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(
                &conflict_scope,
                &late_patch,
                1,
                &effect_audit,
                Some(&effect),
            )
            .await
            .unwrap(),
        AuditedConfigWrite::Conflict {
            current_revision: Some(2)
        },
        "G4"
    );
    assert!(
        store
            .pending_management_effects_scoped(&conflict_scope)
            .await
            .unwrap()
            .is_empty(),
        "G4"
    );
}

#[tokio::test]
async fn postgres_audit_begin_serializes_concurrent_exact_and_conflicting_requests() {
    let Some(pool) = schema_pool("t_config_audit_begin_race").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");
    audit_identity::concurrent_begin(&store, &store).await;
}

async fn schema_pool(schema: &'static str) -> Option<PgPool> {
    let admin = match PgPool::connect(&database_url()).await {
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
    Some(
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
            .expect("reachable PostgreSQL fixture must open its isolated schema"),
    )
}

#[tokio::test]
async fn existing_postgres_store_validates_without_applying_schema() {
    let Some(pool) = schema_pool("t_config_existing").await else {
        return;
    };
    let error = match PostgresConfigStore::with_existing_pool(pool.clone()).await {
        Ok(_) => panic!("an application connection must not create a missing ledger"),
        Err(error) => error,
    };
    assert!(error.to_string().starts_with("schema:"));

    PostgresConfigStore::with_pool(pool.clone())
        .await
        .expect("migration phase");
    PostgresConfigStore::with_existing_pool(pool.clone())
        .await
        .expect("application opens an already-migrated schema");

    sqlx::query(
        "UPDATE config_schema_migrations SET checksum='drift' \
         WHERE bundle_id='awaken.config'",
    )
    .execute(&pool)
    .await
    .expect("corrupt fixture ledger");
    let error = match PostgresConfigStore::with_existing_pool(pool).await {
        Ok(_) => panic!("checksum drift must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("checksum mismatch"));
}

#[tokio::test]
async fn negative_config_generation_is_reported_as_corrupt_storage() {
    // Cause/effect graph: C1 a durable generation is non-negative or negative;
    // C2 the row is read through the config authority. E1 exact revision is
    // returned for C1>=0; E2 C1<0 is a storage error, never a huge u64 revision.
    //
    // | Rule | generation | Effect |
    // | G1 | non-negative | E1 exact revision |
    // | G2 | negative | E2 fail closed |
    let Some(pool) = schema_pool("t_config_negative_generation").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool.clone())
        .await
        .expect("store");
    let scope = ScopeId::from("ws_negative");
    let config = agent_config();
    store
        .put_config_scoped(&scope, &config)
        .await
        .expect("G1 insert");
    sqlx::query("UPDATE config_agent SET generation = -1 WHERE scope_id = $1 AND id = $2")
        .bind(&scope.0)
        .bind(&config.id)
        .execute(&pool)
        .await
        .expect("inject historical corruption");
    let error = store
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .expect_err("G2 negative generation fails closed");
    assert!(error.to_string().contains("negative"), "G2/E2: {error}");
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        id: "agent-1".to_string(),
        instructions: "be helpful".to_string(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b")
            .expect("literal coordinates"),
        inference: Default::default(),
        tool_ids: vec!["echo".to_string()],
        model_fallbacks: Vec::new(),
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
    let stored =
        StoredPublication::published_at_revision(publication.clone(), &config.id, 1, "default");
    assert_eq!(
        store
            .put_publication_if_source_current(&stored)
            .await
            .expect("put publication"),
        PublicationWrite::Committed { source_revision: 1 }
    );
    // Idempotent by fingerprint.
    store
        .put_publication_if_source_current(&stored)
        .await
        .expect("re-put");

    let loaded = store
        .get_publication(&publication.fingerprint.0)
        .await
        .unwrap()
        .expect("publication exists");
    assert_eq!(loaded.fingerprint, publication.fingerprint.0);
    assert_eq!(loaded.snapshot.fingerprint.0, publication.fingerprint.0);
}

#[tokio::test]
async fn postgres_conditional_publication_fences_one_fingerprint_per_source_revision() {
    // Cross-backend cause/effect parity with SQLite: P1 Agent generation 1 is
    // locked; P2 the first resolved fingerprint commits; P3 changed dependency
    // resolution produces different bytes at that same generation. PostgreSQL
    // must conflict inside the same transaction, leaving one durable row.
    let Some(pool) = schema_pool("t_config_publication_revision_fence").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");
    let scope = ScopeId::from("ws_publication_revision_fence");
    let mut source = agent_config();
    source.id = "revision-fenced-agent".into();
    store
        .put_config_scoped(&scope, &source)
        .await
        .expect("source");
    let tools = vec![ToolDescriptor::pinned(
        "test",
        "echo",
        "Echo",
        serde_json::json!({"type": "object"}),
    )];
    let first = StoredPublication::published_at_revision(
        compile(&source, &tools).expect("first compile"),
        &source.id,
        1,
        "runtime-a",
    );
    let mut drifted = source.clone();
    drifted.instructions = "dependency-resolved behavior".into();
    let second = StoredPublication::published_at_revision(
        compile(&drifted, &tools).expect("second compile"),
        &source.id,
        1,
        "runtime-a",
    );
    assert_ne!(first.fingerprint, second.fingerprint, "P3");
    assert_eq!(
        store
            .put_publication_if_source_current_scoped(&scope, &first)
            .await
            .expect("P2"),
        PublicationWrite::Committed { source_revision: 1 }
    );
    assert_eq!(
        store
            .put_publication_if_source_current_scoped(&scope, &second)
            .await
            .expect("P3"),
        PublicationWrite::Conflict {
            current_revision: Some(1)
        }
    );
    let durable = store.list_published_scoped(&scope).await.unwrap();
    assert_eq!(durable.len(), 1, "P3");
    assert_eq!(durable[0].fingerprint, first.fingerprint, "P3");
}

#[tokio::test]
async fn postgres_allows_one_source_revision_in_distinct_execution_workspaces() {
    let Some(pool) = schema_pool("t_config_publication_execution_targets").await else {
        return;
    };
    let store = PostgresConfigStore::with_pool(pool).await.expect("store");
    let scope = ScopeId::from("reserved_authoring_scope");
    let mut source = agent_config();
    source.id = "reserved-assistant".into();
    store
        .put_config_scoped(&scope, &source)
        .await
        .expect("source");
    let tools = vec![ToolDescriptor::pinned(
        "test",
        "echo",
        "Echo",
        serde_json::json!({"type": "object"}),
    )];
    let first = StoredPublication::published_at_revision(
        compile(&source, &tools).expect("first compile"),
        &source.id,
        1,
        "workspace-a",
    );
    let mut drifted = source.clone();
    drifted.instructions = "workspace-b behavior".into();
    let second = StoredPublication::published_at_revision(
        compile(&drifted, &tools).expect("second compile"),
        &source.id,
        1,
        "workspace-b",
    );

    for publication in [&first, &second] {
        assert_eq!(
            store
                .put_publication_if_source_current_scoped(&scope, publication)
                .await
                .expect("targeted publication"),
            PublicationWrite::Committed { source_revision: 1 }
        );
    }
    assert_eq!(store.list_published_scoped(&scope).await.unwrap().len(), 2);
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
    let publication_for = |id: &str, workspace: &str| -> StoredPublication {
        let cfg = AgentConfig {
            id: id.to_string(),
            instructions: format!("body-{id}"),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b")
                .expect("literal coordinates"),
            inference: Default::default(),
            tool_ids: vec!["echo".to_string()],
            ..Default::default()
        };
        StoredPublication::published(compile(&cfg, &tools).expect("compile"), &cfg.id, workspace)
    };

    let p1 = publication_for("a1", "ws_a");
    let p2 = publication_for("a2", "ws_a");
    let mut p_compiled = publication_for("a3", "ws_a");
    p_compiled.state = PublicationState::Compiled; // excluded from list_published
    let p_b = publication_for("b1", "ws_b");
    let p1_b = publication_for("a1", "ws_b");

    store
        .seed_publication_history_for_test(&a, &p1)
        .await
        .expect("p1");
    store
        .seed_publication_history_for_test(&a, &p2)
        .await
        .expect("p2");
    store
        .seed_publication_history_for_test(&a, &p_compiled)
        .await
        .expect("p_compiled");
    store
        .seed_publication_history_for_test(&b, &p_b)
        .await
        .expect("p_b");
    store
        .seed_publication_history_for_test(&b, &p1_b)
        .await
        .expect("the same fingerprint is independently owned by b");

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
    assert_eq!(b_list.len(), 2);
    assert!(
        b_list
            .iter()
            .any(|entry| entry.fingerprint == p_b.fingerprint)
    );
    assert!(
        b_list
            .iter()
            .any(|entry| entry.fingerprint == p1.fingerprint)
    );
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
            delegation_limits: Default::default(),
            model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b")
                .expect("literal coordinates"),
            inference: Default::default(),
            tool_ids: vec!["echo".to_string()],
            ..Default::default()
        };
        StoredPublication::published(compile(&cfg, &tools).expect("compile"), &cfg.id, "ws_a")
    };

    let v1 = pub_v("v1");
    let v2 = pub_v("v2");
    let v3 = pub_v("v3");
    store
        .seed_publication_history_for_test(&a, &v1)
        .await
        .expect("v1");
    store
        .seed_publication_history_for_test(&a, &v2)
        .await
        .expect("v2");
    store
        .seed_publication_history_for_test(&a, &v3)
        .await
        .expect("v3");

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
