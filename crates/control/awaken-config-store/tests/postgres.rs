//! Live Postgres config-store CRUD. Skips when no Postgres is reachable; isolates
//! each test in a fresh schema (the store takes no prefix; ADR-0029/ADR-0031).

use awaken_agent_config::{
    AgentConfig, AuditedConfigWrite, ConfigRegistry, ConfigWrite, ManagementAuditRecord,
    ManagementEffect, PublicationState, ScopeId, ScopedConfigRegistry, StoredPublication,
    compile_resolved,
};
use awaken_config_store::PostgresConfigStore;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::sync::Arc;

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
    store
        .complete_management_effect_scoped(&scope, effect.kind(), effect.key())
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
    // Concurrent begin decision table:
    // | rule | same scoped call id | record content | outcomes |
    // | B1   | yes                 | exact same     | one Applied + one Replayed |
    // | B2   | yes                 | different      | one Applied + one conflict error |
    // The ON CONFLICT audit-begin port is the only INSERT owner; the final
    // config/effect transaction requires the winning pending row.
    let Some(pool) = schema_pool("t_config_audit_begin_race").await else {
        return;
    };
    let store = Arc::new(PostgresConfigStore::with_pool(pool).await.expect("store"));
    let scope = ScopeId::from("audit-race");
    let exact = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "same".into(),
        summary: "same request".into(),
    };
    let (left, right) = tokio::join!(
        store.record_management_audit_scoped(&scope, &exact),
        store.record_management_audit_scoped(&scope, &exact),
    );
    let mut exact_outcomes = vec![left.unwrap(), right.unwrap()];
    exact_outcomes.sort_by_key(|outcome| match outcome {
        AuditedConfigWrite::Applied => 0,
        AuditedConfigWrite::Replayed => 1,
        AuditedConfigWrite::Conflict { .. } => 2,
    });
    assert_eq!(
        exact_outcomes,
        vec![AuditedConfigWrite::Applied, AuditedConfigWrite::Replayed],
        "B1"
    );

    let first = ManagementAuditRecord {
        call_id: "different".into(),
        summary: "first request".into(),
        ..exact.clone()
    };
    let second = ManagementAuditRecord {
        summary: "second request".into(),
        ..first.clone()
    };
    let (left, right) = tokio::join!(
        store.record_management_audit_scoped(&scope, &first),
        store.record_management_audit_scoped(&scope, &second),
    );
    assert_eq!(
        usize::from(matches!(left, Ok(AuditedConfigWrite::Applied)))
            + usize::from(matches!(right, Ok(AuditedConfigWrite::Applied))),
        1,
        "B2 one winner"
    );
    assert_eq!(
        usize::from(left.is_err()) + usize::from(right.is_err()),
        1,
        "B2"
    );
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
        model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b"),
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
    let stored = StoredPublication::published(publication.clone(), &config.id, "default");
    store
        .put_publication(&stored)
        .await
        .expect("put publication");
    // Idempotent by fingerprint.
    store.put_publication(&stored).await.expect("re-put");

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
            .put_publication_if_config_revision_scoped(&scope, &first, 1)
            .await
            .expect("P2"),
        ConfigWrite::Applied { revision: 1 }
    );
    assert_eq!(
        store
            .put_publication_if_config_revision_scoped(&scope, &second, 1)
            .await
            .expect("P3"),
        ConfigWrite::Conflict {
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
                .put_publication_if_config_revision_scoped(&scope, publication, 1)
                .await
                .expect("targeted publication"),
            ConfigWrite::Applied { revision: 1 }
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
            model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b"),
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

    store.put_publication_scoped(&a, &p1).await.expect("p1");
    store.put_publication_scoped(&a, &p2).await.expect("p2");
    store
        .put_publication_scoped(&a, &p_compiled)
        .await
        .expect("p_compiled");
    store.put_publication_scoped(&b, &p_b).await.expect("p_b");
    store
        .put_publication_scoped(&b, &p1_b)
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
            model_binding: awaken_agent_config::ModelSelection::pinned("p", "m", "b"),
            inference: Default::default(),
            tool_ids: vec!["echo".to_string()],
            ..Default::default()
        };
        StoredPublication::published(compile(&cfg, &tools).expect("compile"), &cfg.id, "ws_a")
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
