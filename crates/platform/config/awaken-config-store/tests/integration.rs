// @formal evidence audit.business_action_follows_audit_record
// @formal evidence audit.business_commit_and_audit_transition_are_atomic
// @formal evidence audit.call_identity_is_stable
// @formal evidence audit.stable_call_replay_does_not_repeat_business_write
// @formal evidence config.generation_is_monotonic
// @formal evidence config.stale_writer_never_applies
// @formal evidence config_activation.stale_publication_never_installs
// @formal evidence resource_binding.draft_audit_and_effect_are_atomic
// @formal evidence resource_binding.pending_effect_survives_until_readback
// @formal evidence resource_binding.replay_is_idempotent_and_identity_stable
//! Config-store end to end (ADR-0031): a declarative config compiles into a
//! content-addressed publication, is stored and reloaded, and the runtime
//! executes the produced snapshot. Plus: the `config_*` tables
//! coexist with the runtime's `runtime_*` tables in one SQLite database, isolated
//! by namespace (ADR-0029).

use std::sync::Arc;

use awaken_agent_config::catalog_snapshot::AgentCatalogSnapshotRepository;
use awaken_agent_config::personal_selection::PersonalAgentSelectionRepository;
use awaken_agent_config::{
    AgentConfig, AuditedConfigWrite, ConfigRegistry, ConfigWrite, DEFAULT_SCOPE,
    ManagementAuditRecord, ManagementEffect, ModelSelection, PublicationState, PublicationWrite,
    ScopeId, ScopedConfig, ScopedConfigRegistry, StoredPublication, compile_resolved,
};
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_config_store::SqliteConfigStore;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use awaken_sqlite_pool::EmbeddedSqlite;
use awaken_store_inmem::MemoryCommitCoordinator;

#[path = "support/revision_cas.rs"]
mod revision_cas;

#[path = "support/audit_identity.rs"]
mod audit_identity;

#[path = "support/effect_retirement.rs"]
mod effect_retirement;

#[tokio::test]
async fn sqlite_publication_source_admission_preserves_rejected_frame() {
    // P1-P4 share one actual adapter decision table; no substitute CAS model.
    revision_cas::publication_source_table(&SqliteConfigStore::open_in_memory().await.unwrap())
        .await;
}

#[tokio::test]
async fn sqlite_management_effect_stale_completion_preserves_replacement() {
    effect_retirement::stale_completion_preserves_replacement(
        &SqliteConfigStore::open_in_memory().await.unwrap(),
        &agent_config(),
    )
    .await;
}

#[tokio::test]
async fn sqlite_audit_identity_decision_table() {
    audit_identity::decision_table(
        &SqliteConfigStore::open_in_memory().await.unwrap(),
        &agent_config(),
    )
    .await;
}

#[tokio::test]
async fn sqlite_audit_identity_legacy_replay_and_cross_connection_begin() {
    // M1 historical noncanonical JSON with colon/Unicode identity -> migrate
    // once and replay by semantic equality. M2 independent store connections
    // -> shared B1/B2 unique-key arbitration, not a fixture's shared mutex.
    // M3 pending exact legacy intent -> atomic business+audit commit.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-audit-identity-{}-{nonce}.db",
        std::process::id()
    ));
    let conn = EmbeddedSqlite::file(&path).open().await.unwrap();
    awaken_scoped_migration::sqlite::SqliteMigrationRunner::with_prefix(
        conn.sqlx().clone(),
        "config",
    )
    .unwrap()
    .run_bundle(&audit_identity::compact_history())
    .await
    .unwrap();
    let record = r#"{ "summary":"body_sha256=one", "call_id":"b:调用", "tool":"http:POST:a" }"#;
    sqlx::query("INSERT INTO config_management_audit(scope_id,call_id,record) VALUES('ws','http:POST:a:b:调用',?1)")
        .bind(record)
        .execute(&conn)
        .await
        .unwrap();
    conn.close().await;
    let audit: ManagementAuditRecord = serde_json::from_str(record).unwrap();
    let scope = ScopeId::from("ws");
    let first = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    let second = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        first
            .record_management_audit_scoped(&scope, &audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed,
        "M1"
    );
    audit_identity::concurrent_begin(&first, &second).await;
    assert_eq!(
        first
            .put_config_with_audit_scoped(&scope, &agent_config(), 0, &audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "M3"
    );
    assert!(
        second
            .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
            .await
            .unwrap()
            .unwrap()
            .business_committed
    );
    drop((first, second));
    EmbeddedSqlite::file(&path).settled().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn sqlite_config_cas_missing_row_and_revision_decision_table() {
    // R6 uses separate file-backed SQLite connections; a shared store mutex
    // cannot substitute for database arbitration. Reopen verifies durability.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-config-cas-race-{}-{nonce}.db",
        std::process::id()
    ));
    let first = Arc::new(
        SqliteConfigStore::open(path.to_str().unwrap())
            .await
            .unwrap(),
    );
    let second = Arc::new(
        SqliteConfigStore::open(path.to_str().unwrap())
            .await
            .unwrap(),
    );
    revision_cas::decision_table(first.as_ref()).await;
    revision_cas::concurrent_writers(&[first.clone(), second.clone()]).await;
    revision_cas::mixed_unconditional_and_cas_writers(&[first.clone(), second.clone()]).await;
    let scope = ScopeId::from("cas-concurrent");
    let before_reopen = revision_cas::frame(first.as_ref(), &scope, "race").await;
    drop((first, second));
    EmbeddedSqlite::file(&path).settled().await;
    let reopened = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        revision_cas::frame(&reopened, &scope, "race").await,
        before_reopen,
        "R6 reopen preserves complete winner and history"
    );
    drop(reopened);
    EmbeddedSqlite::file(&path).settled().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn sqlite_config_cas_exhaustion_preserves_reopenable_truth() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-config-cas-{}-{nonce}.db",
        std::process::id()
    ));
    let scope = ScopeId::from("exhausted");
    let config = agent_config();
    let store = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    store.put_config_scoped(&scope, &config).await.unwrap();
    let raw = EmbeddedSqlite::file(&path).open().await.unwrap();
    sqlx::query("UPDATE config_agent SET generation = ?1 WHERE scope_id = ?2 AND id = ?3")
        .bind(i64::MAX)
        .bind(&scope.0)
        .bind(&config.id)
        .execute(&raw)
        .await
        .unwrap();
    raw.close().await;
    revision_cas::exhausted(&store, &config).await;
    drop(store);
    EmbeddedSqlite::file(&path).settled().await;
    let reopened = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    let row = reopened
        .get_config_revision_scoped(&scope, &config.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.revision, i64::MAX as u64);
    assert_eq!(row.config, config);
    drop(reopened);
    EmbeddedSqlite::file(&path).settled().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn sqlite_config_history_insert_failure_rolls_back_all_write_paths() {
    // R8 inject only history generation 2; migration and seed generation 1 stay
    // real. The sole shared suite checks all three write/transaction callers.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-config-history-fault-{}-{nonce}.db",
        std::process::id()
    ));
    let store = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    let conn = EmbeddedSqlite::file(&path).open().await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER inject_history_failure BEFORE INSERT ON config_agent_revision WHEN NEW.generation = 2 BEGIN SELECT RAISE(ABORT, 'injected config history failure'); END")
        .execute(&conn)
        .await
        .unwrap();
    revision_cas::history_failure_rolls_back(&store).await;
    conn.close().await;
    drop(store);
    EmbeddedSqlite::file(&path).settled().await;
    std::fs::remove_file(path).unwrap();
}

fn compile(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
) -> Result<awaken_agent_config::ExecutableAgentSnapshot, awaken_agent_config::CompileError> {
    compile_resolved(config, tools, AgentSnapshotMetadata::default(), |_| true)
}

struct TextLlm;
#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        id: "support-agent".to_string(),
        instructions: "be helpful".to_string(),
        max_steps: Some(8),
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

fn tool_catalog() -> Vec<ToolDescriptor> {
    vec![ToolDescriptor::pinned(
        "test",
        "echo",
        "Echo",
        serde_json::json!({"type": "object"}),
    )]
}

#[tokio::test]
async fn audited_config_and_external_effect_are_journaled_atomically_and_replay_safely() {
    // Begin/final decision table:
    // | rule | durable audit begin | final audited transaction | effect |
    // | A0   | absent              | attempted                 | fail closed; zero config/effect/audit |
    // | A1   | exact pending       | attempted                 | config/effect/committed atomically |
    // | A2   | exact committed     | replay                    | Replayed; no second write |
    // `record_management_audit_scoped` is the sole concurrent begin owner.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let scope = ScopeId::from("ws");
    let audit = ManagementAuditRecord {
        tool: "admin_draft_agent".into(),
        call_id: "resource-call".into(),
        summary: "draft with resource".into(),
    };
    let effect = ManagementEffect::UpsertAgentInputs {
        config: awaken_agent_config::AgentInputConfig {
            agent_id: "support-agent".into(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    let absent_error = store
        .put_config_with_audit_effect_scoped(&scope, &agent_config(), 0, &audit, Some(&effect))
        .await
        .expect_err("A0");
    assert!(absent_error.to_string().contains("pre-recorded"), "A0");
    assert!(
        store
            .get_management_audit_scoped(&scope, &audit.tool, &audit.call_id)
            .await
            .unwrap()
            .is_none(),
        "A0"
    );
    assert!(
        store
            .get_config_scoped(&scope, "support-agent")
            .await
            .unwrap()
            .is_none(),
        "A0"
    );
    assert_eq!(
        store
            .record_management_audit_scoped(&scope, &audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "A1 begin"
    );
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(&scope, &agent_config(), 0, &audit, Some(&effect),)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied
    );
    assert_eq!(
        store
            .pending_management_effects_scoped(&scope)
            .await
            .unwrap(),
        vec![effect.clone()]
    );
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(&scope, &agent_config(), 0, &audit, Some(&effect),)
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed
    );
    let ManagementEffect::UpsertAgentInputs { config: applied } = &effect;
    let receipt = effect.confirm_readback(&scope, Some(applied)).unwrap();
    store
        .complete_management_effect_scoped(&receipt)
        .await
        .unwrap();
    assert!(
        store
            .pending_management_effects_scoped(&scope)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn mutable_writes_and_archive_are_ordered_by_one_generation_fence() {
    // Cause/effect graph: C1 an audited call commits before response loss; C2 the
    // Agent is archived before that exact call retries; C3 archive commits after
    // an ordinary/audited/effect writer captured the prior generation; C4 an
    // audited writer commits first. Effects: E1 exact call-id replay wins before
    // mutable lifecycle admission; E2 every late first-apply writer conflicts and
    // writes no config/effect/business commit while its pre-recorded audit remains
    // pending; E3 a later archive retries at the new revision
    // and terminates the Agent.
    //
    // Decision table:
    // | rule | ordering | writer | audit state | result |
    // | G1 | audited commit -> archive -> retry | audited | committed | Replayed, archived |
    // | G2 | capture r1 -> archive r2 -> apply | ordinary | none | Conflict(r2) |
    // | G3 | capture r1 -> archive r2 -> apply | audited | pending | Conflict; audit pending, no config |
    // | G4 | capture r1 -> archive r2 -> apply | audit+effect | pending | Conflict; audit pending, no config/effect |
    // | G5 | audited r2 -> stale archive r1 -> retry r2 | both | committed | archive r3 |
    let store = SqliteConfigStore::open_in_memory().await.expect("store");

    let replay_scope = ScopeId::from("audit-replay-order");
    let original = agent_config();
    store
        .put_config_scoped(&replay_scope, &original)
        .await
        .unwrap();
    let replay_audit = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "response-lost".into(),
        summary: "patch before archive".into(),
    };
    let mut first_patch = original.clone();
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
        "G1/E1 replay precedes archived lifecycle admission"
    );
    assert!(
        store
            .get_config_revision_scoped(&replay_scope, &original.id)
            .await
            .unwrap()
            .unwrap()
            .config
            .archived_at
            .is_some(),
        "G1/E1"
    );

    let conflict_scope = ScopeId::from("archive-wins");
    store
        .put_config_scoped(&conflict_scope, &original)
        .await
        .unwrap();
    let mut late_patch = original.clone();
    late_patch.instructions = "must not revive".into();
    let mut archive_winner = original.clone();
    archive_winner.archived_at = Some("2026-08-30T00:00:00Z".into());
    assert!(matches!(
        store
            .put_config_if_revision_scoped(&conflict_scope, &archive_winner, 1)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 2 }
    ));
    assert_eq!(
        store
            .put_config_if_revision_scoped(&conflict_scope, &late_patch, 1)
            .await
            .unwrap(),
        ConfigWrite::Conflict {
            current_revision: Some(2)
        },
        "G2/E2"
    );
    let late_audit = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "late-audit".into(),
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
        "G3/E2"
    );
    assert!(
        store
            .get_management_audit_scoped(&conflict_scope, &late_audit.tool, &late_audit.call_id,)
            .await
            .unwrap()
            .is_some_and(|entry| !entry.business_committed),
        "G3/E2 pending audit intent is not a business commit"
    );
    let effect_audit = ManagementAuditRecord {
        call_id: "late-effect".into(),
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
    let late_effect = ManagementEffect::UpsertAgentInputs {
        config: awaken_agent_config::AgentInputConfig {
            agent_id: original.id.clone(),
            environment: None,
            inputs: Vec::new(),
            revision: 1,
        },
    };
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(
                &conflict_scope,
                &late_patch,
                1,
                &effect_audit,
                Some(&late_effect),
            )
            .await
            .unwrap(),
        AuditedConfigWrite::Conflict {
            current_revision: Some(2)
        },
        "G4/E2"
    );
    assert!(
        store
            .pending_management_effects_scoped(&conflict_scope)
            .await
            .unwrap()
            .is_empty(),
        "G4/E2"
    );

    let authoring_scope = ScopeId::from("authoring-wins");
    store
        .put_config_scoped(&authoring_scope, &original)
        .await
        .unwrap();
    let winner_audit = ManagementAuditRecord {
        tool: "admin_patch_agent".into(),
        call_id: "authoring-first".into(),
        summary: "authoring wins generation two".into(),
    };
    assert_eq!(
        store
            .record_management_audit_scoped(&authoring_scope, &winner_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "G5 begin"
    );
    assert_eq!(
        store
            .put_config_with_audit_scoped(&authoring_scope, &first_patch, 1, &winner_audit)
            .await
            .unwrap(),
        AuditedConfigWrite::Applied,
        "G5"
    );
    assert_eq!(
        store
            .put_config_if_revision_scoped(&authoring_scope, &archive_winner, 1)
            .await
            .unwrap(),
        ConfigWrite::Conflict {
            current_revision: Some(2)
        },
        "G5 stale archive"
    );
    let mut terminal = first_patch;
    terminal.archived_at = Some("2026-08-30T00:00:01Z".into());
    assert!(matches!(
        store
            .put_config_if_revision_scoped(&authoring_scope, &terminal, 2)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 3 }
    ));
}

#[tokio::test]
async fn config_generation_cas_rejects_a_stale_writer_without_lost_update() {
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let mut first = agent_with("cas", "v1");
    assert_eq!(
        store.put_config_if_revision(&first, 0).await.unwrap(),
        ConfigWrite::Applied { revision: 1 }
    );
    let snapshot = store
        .get_config_revision("cas")
        .await
        .unwrap()
        .expect("created config");
    assert_eq!(snapshot.revision, 1);

    first.instructions = "v2".into();
    assert_eq!(
        store
            .put_config_if_revision(&first, snapshot.revision)
            .await
            .unwrap(),
        ConfigWrite::Applied { revision: 2 }
    );
    let mut stale = snapshot.config;
    stale.instructions = "stale-overwrite".into();
    assert_eq!(
        store
            .put_config_if_revision(&stale, snapshot.revision)
            .await
            .unwrap(),
        ConfigWrite::Conflict {
            current_revision: Some(2)
        }
    );
    let current = store
        .get_config_revision("cas")
        .await
        .unwrap()
        .expect("current config");
    assert_eq!(current.revision, 2);
    assert_eq!(current.config.instructions, "v2");
}

#[tokio::test]
async fn config_compiles_stores_and_the_runtime_executes_the_snapshot() {
    let config = agent_config();

    // Config domain: compile to a content-addressed publication, then persist.
    let publication = compile(&config, &tool_catalog()).expect("compile");
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    store.put_config(&config).await.expect("put config");
    let stored =
        StoredPublication::published_at_revision(publication.clone(), &config.id, 1, DEFAULT_SCOPE);
    assert_eq!(
        store
            .put_publication_if_source_current(&stored)
            .await
            .expect("put publication"),
        PublicationWrite::Committed { source_revision: 1 }
    );

    // Reload the publication by its fingerprint — the durable round trip.
    let loaded = store
        .get_publication(&publication.fingerprint.0)
        .await
        .expect("get")
        .expect("publication exists");
    assert_eq!(loaded.fingerprint, publication.fingerprint.0);
    assert_eq!(
        store.get_config("support-agent").await.unwrap().as_ref(),
        Some(&config)
    );

    // Runtime executes the exact published snapshot without reconstructing a
    // second node-local catalog object.
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));
    runtime.register_snapshot(loaded.snapshot.clone());

    let activation = RunActivation::new(
        RunId("run-1".to_string()),
        ThreadId("thread-1".to_string()),
        loaded.snapshot,
        vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
    );
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    // The snapshot is internally consistent, so config produced exactly what the
    // runtime consumed.
    let state = runtime.execute(activation, ctx).await.expect("execute");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // C1 persisted exact publication + C2 one successful model response =>
    // E1 input/Running, model enter, model return, terminal commits; E2 one
    // input and one answer. Model receipts are independent of message count.
    assert_eq!(commit.commit_count(), 4);
    let messages = commit.committed().messages;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text_content(), "hi");
    assert_eq!(messages[1].text_content(), "done");
}

#[tokio::test]
async fn legacy_publication_inherits_its_durable_configuration_scope() {
    // Causes: L1 a valid immutable publication row predates the explicit
    // execution_workspace field; L2 its SQL row remains owned by one scope.
    // Effects: E1 reload succeeds through the shared store codec; E2 the exact
    // row scope becomes its execution target. Rule D1=L1+L2=>E1+E2. Current
    // explicit targets and malformed records are covered by the codec table.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-config-legacy-workspace-{}-{nonce}.db",
        std::process::id()
    ));
    let scope = ScopeId::from("workspace-legacy-owner");
    let publication = compile(&agent_config(), &tool_catalog()).expect("compile");
    let fingerprint = publication.fingerprint.0.clone();
    {
        let store = SqliteConfigStore::open(path.to_str().unwrap())
            .await
            .expect("store");
        store
            .seed_publication_history_for_test(
                &scope,
                &StoredPublication::published(publication, "support-agent", "old-target"),
            )
            .await
            .expect("seed current record");
    }
    let raw = EmbeddedSqlite::file(&path).open().await.unwrap();
    sqlx::query(
        "UPDATE config_publication \
         SET record=json_remove(record,'$.execution_workspace') \
         WHERE scope_id=?1 AND fingerprint=?2",
    )
    .bind(&scope.0)
    .bind(&fingerprint)
    .execute(&raw)
    .await
    .expect("project historical bytes");
    raw.close().await;
    let store = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .expect("reopen");
    let loaded = store
        .get_publication_scoped(&scope, &fingerprint)
        .await
        .expect("E1")
        .expect("publication");
    assert_eq!(loaded.execution_workspace, scope, "E2");
    std::fs::remove_file(path).ok();
}

#[tokio::test]
async fn config_and_runtime_tables_coexist_in_one_database() {
    use awaken_store_sqlite::SqliteCommitCoordinator;

    let path =
        std::env::temp_dir().join(format!("awaken_config_coexist_{}.db", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    // Both components migrate into the same file under their own namespaces.
    let config = SqliteConfigStore::open(&path).await.expect("config store");
    let _commit = SqliteCommitCoordinator::open(&path)
        .await
        .expect("commit store");
    config
        .put_config(&agent_config())
        .await
        .expect("put config");

    // A raw view of the schema: both namespaces' tables and ledgers are present,
    // with no collision.
    let conn = EmbeddedSqlite::file(&path).open().await.expect("open raw");
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&conn)
            .await
            .unwrap();

    for expected in [
        "config_agent",
        "config_agent_revision",
        "config_publication",
        "config_schema_migrations",
        "runtime_commit",
        "runtime_schema_migrations",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected table {expected} in {names:?}"
        );
    }

    conn.close().await;
    let _ = std::fs::remove_file(&path);
}

// --- CEG 03 / B7 (StoredPublication::published) ------------------------------

fn scoped_agent(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: "be helpful".to_string(),
        max_steps: Some(8),
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("p", "m", "b").expect("literal coordinates"),
        inference: Default::default(),
        tool_ids: Vec::new(),
        ..Default::default()
    }
}

#[test]
fn published_always_stamps_published_and_takes_ids_from_the_snapshot() {
    // B7(a): `published` always sets state=Published and lifts the fingerprint and
    // publication id from the snapshot itself (never re-derived by the store).
    let cfg = agent_config();
    let snapshot = compile(&cfg, &tool_catalog()).expect("compile");
    let want_fingerprint = snapshot.fingerprint.0.clone();
    let want_pub_id = snapshot.fingerprint.0.clone();

    let stored = StoredPublication::published(snapshot, &cfg.id, DEFAULT_SCOPE);
    assert_eq!(stored.state, PublicationState::Published);
    assert_eq!(stored.fingerprint, want_fingerprint);
    assert_eq!(stored.publication_id, want_pub_id);
    assert_eq!(stored.agent_id, cfg.id);
    // The stored envelope and snapshot agree on that same fingerprint.
    assert_eq!(stored.snapshot.fingerprint.0, want_fingerprint);
    let wire = serde_json::to_value(&stored).expect("publication serializes");
    assert!(
        wire.get("install").is_none(),
        "a publication must not persist a duplicate runtime catalog install"
    );
}

#[test]
fn published_is_idempotent_for_the_same_snapshot() {
    // B7(b): the snapshot is content-addressed, so wrapping two compiles of the same
    // config yields the same identity (fingerprint + publication id + state).
    let cfg = agent_config();
    let a = StoredPublication::published(
        compile(&cfg, &tool_catalog()).unwrap(),
        &cfg.id,
        DEFAULT_SCOPE,
    );
    let b = StoredPublication::published(
        compile(&cfg, &tool_catalog()).unwrap(),
        &cfg.id,
        DEFAULT_SCOPE,
    );
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_eq!(a.publication_id, b.publication_id);
    assert_eq!(a.state, b.state);
}

// --- CEG 03 / B8 (ScopedConfig decorator) ------------------------------------

#[tokio::test]
async fn scoped_config_isolates_portable_same_id_rows_across_scopes() {
    // B8(a)+(b)+(d): the decorator binds one scope and exposes the scope-free port.
    // Before B writes, A's row is invisible to B. B may then own the same portable
    // id independently without clobbering A's data.
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.expect("store"));
    let a = ScopedConfig::new(store.clone(), ScopeId::from("ws_a"));
    let b = ScopedConfig::new(store.clone(), ScopeId::from("ws_b"));

    a.put_config(&scoped_agent("shared")).await.expect("put a");

    // (a) list isolation: A sees its row, B's list is empty.
    let a_list: Vec<String> = a
        .list_configs()
        .await
        .expect("list a")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(a_list, vec!["shared".to_string()]);
    assert!(b.list_configs().await.expect("list b").is_empty());
    // (d) get isolation.
    assert!(b.get_config("shared").await.expect("get b").is_none());

    // (b) The composite scope/id identity creates B's independent row.
    b.put_config(&agent_with("shared", "B-data"))
        .await
        .expect("put b");
    assert_eq!(
        a.get_config("shared")
            .await
            .expect("get a")
            .expect("row a")
            .instructions,
        "be helpful"
    );
    assert_eq!(
        b.get_config("shared")
            .await
            .expect("get b")
            .expect("row b")
            .instructions,
        "B-data"
    );
}

#[tokio::test]
async fn scoped_config_default_scope_shares_the_owner_with_scope_free_writes() {
    // B8(c): a scope-free write lands under DEFAULT_SCOPE, so a ScopedConfig bound to
    // DEFAULT_SCOPE reads it back (same owner), while a foreign scope cannot.
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.expect("store"));
    // Scope-free write via the raw store's ConfigRegistry impl (DEFAULT_SCOPE).
    store.put_config(&scoped_agent("d")).await.expect("put");

    let default = ScopedConfig::new(store.clone(), ScopeId::from(DEFAULT_SCOPE));
    let other = ScopedConfig::new(store.clone(), ScopeId::from("ws_other"));
    assert!(
        default
            .get_config("d")
            .await
            .expect("get default")
            .is_some()
    );
    assert!(other.get_config("d").await.expect("get other").is_none());
    assert_eq!(default.scope(), &ScopeId::from(DEFAULT_SCOPE));
}

// --- CEG: scoped agent CRUD data-integrity + ordering ------------------------

/// An authoring aggregate with a chosen id and (fingerprint-affecting) instructions.
fn agent_with(id: &str, instructions: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: instructions.to_string(),
        max_steps: Some(8),
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("p", "m", "b").expect("literal coordinates"),
        inference: Default::default(),
        tool_ids: Vec::new(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_same_scope_re_put_updates_the_config_data() {
    // put_config_scoped ON CONFLICT … DO UPDATE (same-scope branch): a re-put by the
    // owner replaces the stored data (upsert, not insert-only).
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let a = ScopeId::from("ws_a");
    store
        .put_config_scoped(&a, &agent_with("x", "v1"))
        .await
        .expect("put v1");
    store
        .put_config_scoped(&a, &agent_with("x", "v2"))
        .await
        .expect("put v2");
    let got = store
        .get_config_scoped(&a, "x")
        .await
        .expect("get")
        .expect("row");
    assert_eq!(got.instructions, "v2");
}

#[tokio::test]
async fn scoped_migration_captures_immutable_agent_revisions() {
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let workspace = ScopeId::from("ws_a");
    store
        .put_config_scoped(&workspace, &agent_with("x", "v1"))
        .await
        .expect("put v1");
    store
        .put_config_scoped(&workspace, &agent_with("x", "v2"))
        .await
        .expect("put v2");

    let revisions = store
        .list_config_revisions_scoped(&workspace, "x")
        .await
        .expect("history");
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].revision, 1);
    assert_eq!(revisions[0].config.instructions, "v1");
    assert_eq!(revisions[1].revision, 2);
    assert_eq!(revisions[1].config.instructions, "v2");
    // Cause/effect rules: every durable row exposes one first-write time and one
    // exact-revision time; all revisions share the former and revision time is
    // monotonic. SQLite timestamps have one-second precision, so equality is valid.
    assert!(revisions[0].created_at_unix_ms.is_some());
    assert_eq!(
        revisions[0].created_at_unix_ms,
        revisions[1].created_at_unix_ms
    );
    assert!(
        revisions[1].updated_at_unix_ms >= revisions[0].updated_at_unix_ms,
        "revision write time is monotonic"
    );
    assert!(
        store
            .list_config_revisions_scoped(&ScopeId::from("ws_b"), "x")
            .await
            .expect("foreign history")
            .is_empty()
    );
}

#[tokio::test]
async fn same_agent_id_has_independent_data_and_revision_history_per_scope() {
    // The isolation fence protects DATA while allowing a portable id in each
    // scope. Updating B must not modify A or share A's generation history.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let a = ScopeId::from("ws_a");
    let b = ScopeId::from("ws_b");
    store
        .put_config_scoped(&a, &agent_with("x", "A-data"))
        .await
        .expect("put a");
    // ws_b authors its own id "x" with different data.
    store
        .put_config_scoped(&b, &agent_with("x", "B-data"))
        .await
        .expect("put b");
    let owner = store
        .get_config_scoped(&a, "x")
        .await
        .expect("get a")
        .expect("row a");
    assert_eq!(owner.instructions, "A-data");
    let other = store
        .get_config_scoped(&b, "x")
        .await
        .expect("get b")
        .expect("row b");
    assert_eq!(other.instructions, "B-data");
    assert_eq!(
        store
            .list_config_revisions_scoped(&a, "x")
            .await
            .expect("a revisions")
            .len(),
        1
    );
    assert_eq!(
        store
            .list_config_revisions_scoped(&b, "x")
            .await
            .expect("b revisions")
            .len(),
        1
    );
}

#[tokio::test]
async fn list_configs_scoped_returns_ids_ascending() {
    // list_configs_scoped ORDER BY id ASC, filtered to the scope.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let a = ScopeId::from("ws_a");
    for id in ["c", "a", "b"] {
        store
            .put_config_scoped(&a, &agent_with(id, "x"))
            .await
            .expect("put");
    }
    // A row under another scope must not leak into A's list.
    store
        .put_config_scoped(&ScopeId::from("ws_b"), &agent_with("aaa", "x"))
        .await
        .expect("put b");
    let ids: Vec<String> = store
        .list_configs_scoped(&a)
        .await
        .expect("list")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[tokio::test]
async fn reconciliation_scope_inventory_is_distinct_sorted_and_secret_free() {
    // Cause/effect decision table:
    // S1 no configs -> no scopes; S2 several configs in one scope -> one owner;
    // S3 configs in several scopes -> distinct owners sorted by opaque ScopeId.
    // The result contains only ownership coordinates; per-scope config reads
    // remain fenced by list_configs_scoped.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    assert!(store.list_config_scopes().await.unwrap().is_empty(), "S1");
    store
        .put_config_scoped(&ScopeId::from("scope-z"), &agent_with("z-1", "x"))
        .await
        .unwrap();
    store
        .put_config_scoped(&ScopeId::from("scope-z"), &agent_with("z-2", "x"))
        .await
        .unwrap();
    assert_eq!(
        store.list_config_scopes().await.unwrap(),
        vec![ScopeId::from("scope-z")],
        "S2"
    );
    store
        .put_config_scoped(&ScopeId::from("scope-a"), &agent_with("a-1", "x"))
        .await
        .unwrap();
    assert_eq!(
        store.list_config_scopes().await.unwrap(),
        vec![ScopeId::from("scope-a"), ScopeId::from("scope-z")],
        "S3"
    );
}

// --- CEG: publication scope isolation + idempotency + warm-load list ---------

fn publication_for(cfg: &AgentConfig, execution_workspace: &str) -> StoredPublication {
    let snapshot = compile(cfg, &tool_catalog()).expect("compile");
    StoredPublication::published_at_revision(snapshot, &cfg.id, 1, execution_workspace)
}

#[tokio::test]
async fn a_publication_written_under_scope_a_is_invisible_to_scope_b() {
    // get_publication_scoped carries `AND scope_id = ?`: a publication owned by A is
    // unreadable by B even by its exact fingerprint (the tenancy fence on the
    // compiled artifact, not just the authoring aggregate).
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.expect("store"));
    let a = ScopedConfig::new(store.clone(), ScopeId::from("ws_a"));
    let b = ScopedConfig::new(store.clone(), ScopeId::from("ws_b"));
    let publication = publication_for(&agent_with("shared", "hello"), "ws_a");
    let fp = publication.fingerprint.clone();
    store
        .put_config_scoped(&ScopeId::from("ws_a"), &agent_with("shared", "hello"))
        .await
        .expect("source a");
    assert_eq!(
        a.put_publication_if_source_current(&publication)
            .await
            .expect("put a"),
        PublicationWrite::Committed { source_revision: 1 }
    );
    assert!(a.get_publication(&fp).await.expect("get a").is_some());
    assert!(b.get_publication(&fp).await.expect("get b").is_none());
}

#[tokio::test]
async fn the_same_publication_fingerprint_can_be_owned_independently_by_each_scope() {
    // A fingerprint is a content address, not a tenancy coordinate. Two scopes may
    // publish the same immutable bytes; the composite key keeps both ownership rows
    // isolated while preserving idempotency within each scope.
    let store = Arc::new(SqliteConfigStore::open_in_memory().await.expect("store"));
    let a = ScopedConfig::new(store.clone(), ScopeId::from("ws_a"));
    let b = ScopedConfig::new(store.clone(), ScopeId::from("ws_b"));
    let pub_a = publication_for(&agent_with("shared", "same-bytes"), "ws_a");
    let pub_b = publication_for(&agent_with("shared", "same-bytes"), "ws_b");
    assert_eq!(pub_a.fingerprint, pub_b.fingerprint); // content-addressed → identical
    let source = agent_with("shared", "same-bytes");
    store
        .put_config_scoped(&ScopeId::from("ws_a"), &source)
        .await
        .expect("source a");
    store
        .put_config_scoped(&ScopeId::from("ws_b"), &source)
        .await
        .expect("source b");
    a.put_publication_if_source_current(&pub_a)
        .await
        .expect("put a");
    b.put_publication_if_source_current(&pub_b)
        .await
        .expect("put b");
    assert!(
        a.get_publication(&pub_a.fingerprint)
            .await
            .expect("get a")
            .is_some()
    );
    assert!(
        b.get_publication(&pub_b.fingerprint)
            .await
            .expect("get b")
            .is_some()
    );
}

#[tokio::test]
async fn source_fenced_publication_is_idempotent_by_fingerprint() {
    // Causes: C1 first source-fenced insert, C2 exact fingerprint replay,
    // C3 get versus ordered history read. Effects: E1 one durable row owns the
    // first publication sequence and instant; E2 replay replaces neither;
    // E3 both reads return the same coordinate; E4 row metadata stays out of
    // Control JSON. Rules C1=>E1+E4, C1+C2=>E2, C1+C3=>E3.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let a = ScopeId::from("ws_a");
    let source = agent_with("agent", "body");
    store.put_config_scoped(&a, &source).await.expect("source");
    let publication = publication_for(&source, "ws_a");
    store
        .put_publication_if_source_current_scoped(&a, &publication)
        .await
        .expect("put");
    let first = store
        .get_publication_scoped(&a, &publication.fingerprint)
        .await
        .expect("first read")
        .expect("first publication");
    assert!(first.published_at_unix_ms.is_some(), "E1");
    assert!(
        first.publication_order_verified,
        "E1 SQLite writer is serialized"
    );
    assert!(
        serde_json::to_value(&first)
            .unwrap()
            .get("publication_order_verified")
            .is_none(),
        "E4 row metadata does not change Control JSON"
    );
    assert!(
        first
            .publication_sequence
            .is_some_and(|sequence| sequence > 0),
        "E1"
    );
    store
        .put_publication_if_source_current_scoped(&a, &publication)
        .await
        .expect("re-put idempotent");
    let replay = store
        .get_publication_scoped(&a, &publication.fingerprint)
        .await
        .expect("get")
        .expect("publication");
    assert_eq!(
        replay.published_at_unix_ms, first.published_at_unix_ms,
        "E2"
    );
    assert_eq!(
        replay.publication_sequence, first.publication_sequence,
        "E2"
    );
    assert_eq!(
        store.list_published_scoped(&a).await.expect("history")[0].published_at_unix_ms,
        first.published_at_unix_ms,
        "E3"
    );
    assert_eq!(
        store.list_published_scoped(&a).await.expect("history")[0].publication_sequence,
        first.publication_sequence,
        "E3"
    );
}

#[tokio::test]
async fn conditional_publication_fences_a_second_fingerprint_at_one_source_revision() {
    // R1 the reviewed Agent remains at revision 1; R2 the first fingerprint is
    // committed; R3 dependency re-resolution produces different executable
    // bytes without a new authoring revision. R3 must conflict before a second
    // durable row can later poison executable-registration recovery.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let scope = ScopeId::from("ws_publication_revision_fence");
    let first_config = agent_with("agent", "first behavior");
    let second_config = agent_with("agent", "dependency-resolved behavior");
    store
        .put_config_scoped(&scope, &first_config)
        .await
        .expect("draft");
    let first = StoredPublication::published_at_revision(
        compile(&first_config, &tool_catalog()).expect("first compile"),
        "agent",
        1,
        "runtime-a",
    );
    let second = StoredPublication::published_at_revision(
        compile(&second_config, &tool_catalog()).expect("second compile"),
        "agent",
        1,
        "runtime-a",
    );
    assert_ne!(first.fingerprint, second.fingerprint);
    assert_eq!(
        store
            .put_publication_if_source_current_scoped(&scope, &first)
            .await
            .expect("first publication"),
        PublicationWrite::Committed { source_revision: 1 }
    );
    assert_eq!(
        store
            .put_publication_if_source_current_scoped(&scope, &second)
            .await
            .expect("revision fence"),
        PublicationWrite::Conflict {
            current_revision: Some(1)
        }
    );
    let durable = store.list_published_scoped(&scope).await.unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].fingerprint, first.fingerprint);
}

#[tokio::test]
async fn conditional_publication_allows_one_source_revision_in_distinct_execution_workspaces() {
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let scope = ScopeId::from("reserved_authoring_scope");
    let first_config = agent_with("assistant", "workspace-a behavior");
    let second_config = agent_with("assistant", "workspace-b behavior");
    store
        .put_config_scoped(&scope, &first_config)
        .await
        .expect("draft");
    let first = StoredPublication::published_at_revision(
        compile(&first_config, &tool_catalog()).expect("first compile"),
        "assistant",
        1,
        "workspace-a",
    );
    let second = StoredPublication::published_at_revision(
        compile(&second_config, &tool_catalog()).expect("second compile"),
        "assistant",
        1,
        "workspace-b",
    );
    assert_ne!(first.fingerprint, second.fingerprint);

    for publication in [&first, &second] {
        assert_eq!(
            store
                .put_publication_if_source_current_scoped(&scope, publication)
                .await
                .expect("targeted publication"),
            PublicationWrite::Committed { source_revision: 1 }
        );
    }
    let durable = store.list_published_scoped(&scope).await.unwrap();
    assert_eq!(durable.len(), 2);
    assert!(
        durable
            .iter()
            .any(|publication| { publication.targets_execution_workspace("workspace-a") })
    );
    assert!(
        durable
            .iter()
            .any(|publication| { publication.targets_execution_workspace("workspace-b") })
    );
}

#[tokio::test]
async fn list_published_returns_only_published_rows_of_the_scope_in_insertion_order() {
    // list_published_scoped: WHERE scope_id AND state='published' ORDER BY insertion.
    // Covers the state filter (compiled excluded), scope isolation, ordering, and the
    // empty-scope case — the warm-install reload contract.
    let store = SqliteConfigStore::open_in_memory().await.expect("store");
    let a = ScopeId::from("ws_a");
    let b = ScopeId::from("ws_b");

    let p1 = publication_for(&agent_with("a1", "first"), "ws_a");
    let p2 = publication_for(&agent_with("a2", "second"), "ws_a");
    let mut p_compiled = publication_for(&agent_with("a3", "third"), "ws_a");
    p_compiled.state = PublicationState::Compiled; // must be excluded
    let p_b = publication_for(&agent_with("b1", "other-scope"), "ws_b");

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

    // A: only the two published rows, oldest-first (insertion order).
    let a_fps: Vec<String> = store
        .list_published_scoped(&a)
        .await
        .expect("list a")
        .into_iter()
        .map(|p| p.fingerprint)
        .collect();
    assert_eq!(a_fps, vec![p1.fingerprint.clone(), p2.fingerprint.clone()]);

    // B: only its own published row (scope isolation on the list path).
    let b_list = store.list_published_scoped(&b).await.expect("list b");
    assert_eq!(b_list.len(), 1);
    assert_eq!(b_list[0].fingerprint, p_b.fingerprint);

    // An untouched scope reloads nothing.
    assert!(
        store
            .list_published_scoped(&ScopeId::from("ws_empty"))
            .await
            .expect("list empty")
            .is_empty()
    );
}

#[tokio::test]
async fn list_published_warm_load_latest_per_agent_is_deterministic_on_sqlite() {
    // Causes: W1 v1/v2/v3 are inserted in order; W2 v3's durable clock is
    // rolled back before v1; W3 warm load reads one scope. Effects: E1 every
    // row survives; E2 rowid/sequence order remains v1/v2/v3; E3 latest per
    // Agent is v3 while first publication time remains v1's row time.
    // Rule W1+W2+W3=>E1+E2+E3; SQLite rowid and PostgreSQL seq must agree.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-config-publication-order-{}-{nonce}.db",
        std::process::id()
    ));
    let store = SqliteConfigStore::open(path.to_str().unwrap())
        .await
        .expect("store");
    let a = ScopeId::from("ws_a");

    let v1 = publication_for(&agent_with("dup", "v1"), "ws_a");
    let v2 = publication_for(&agent_with("dup", "v2"), "ws_a");
    let v3 = publication_for(&agent_with("dup", "v3"), "ws_a");
    // Same agent, three distinct content addresses (fingerprint tracks instructions).
    assert_eq!(v1.agent_id, "dup");
    assert_eq!(v3.agent_id, "dup");
    assert_ne!(v1.fingerprint, v2.fingerprint);
    assert_ne!(v2.fingerprint, v3.fingerprint);

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

    let connection = EmbeddedSqlite::file(&path)
        .open()
        .await
        .expect("historical row fixture");
    for (fingerprint, time) in [
        (&v1.fingerprint, "2025-01-01 00:00:00"),
        (&v2.fingerprint, "2026-01-01 00:00:00"),
        (&v3.fingerprint, "2024-01-01 00:00:00"),
    ] {
        sqlx::query("UPDATE config_publication SET created_at = ?1 WHERE fingerprint = ?2")
            .bind(time)
            .bind(fingerprint)
            .execute(&connection)
            .await
            .expect("historical row clock fixture");
    }
    connection.close().await;

    let listed = store.list_published_scoped(&a).await.expect("list");
    // rowid ASC == insertion order despite the reversed clock.
    let order: Vec<String> = listed.iter().map(|p| p.fingerprint.clone()).collect();
    assert_eq!(
        order,
        vec![
            v1.fingerprint.clone(),
            v2.fingerprint.clone(),
            v3.fingerprint.clone(),
        ],
        "sqlite must reload oldest-first by rowid"
    );
    assert!(
        listed
            .windows(2)
            .all(|pair| pair[0].publication_sequence < pair[1].publication_sequence),
        "E2"
    );
    assert_eq!(
        listed[0].published_at_unix_ms,
        Some(1_735_689_600_000),
        "E3"
    );
    assert_eq!(
        listed[2].published_at_unix_ms,
        Some(1_704_067_200_000),
        "E3"
    );
    assert!(
        listed.iter().all(|row| row.publication_order_verified),
        "E3"
    );

    // Warm-load fold: agent-keyed map, oldest-first, last write wins → v3, deterministically.
    let mut warm: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for p in &listed {
        warm.insert(p.agent_id.clone(), p.fingerprint.clone());
    }
    assert_eq!(warm.get("dup"), Some(&v3.fingerprint));
    drop(store);
    EmbeddedSqlite::file(&path).settled().await;
    std::fs::remove_file(path).expect("remove task-owned SQLite fixture");
}

// --- TASK 2: PublicationState serde round-trip -------------------------------

#[test]
fn publication_state_round_trips_through_serde_for_every_variant() {
    // Every variant serializes and deserializes back to an equal value. The `state`
    // column and record carry this enum; a lossy round-trip would silently reclassify
    // a publication's lifecycle (and the SQL filter `state = 'published'` depends on the
    // exact lowercase token).
    for state in [PublicationState::Compiled, PublicationState::Published] {
        let json = serde_json::to_string(&state).expect("serialize");
        let back: PublicationState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, back, "PublicationState {state:?} did not round-trip");
    }
    // Pin the lowercase wire tokens the schema's `state` column and SQL filter depend on.
    assert_eq!(
        serde_json::to_string(&PublicationState::Compiled).unwrap(),
        "\"compiled\""
    );
    assert_eq!(
        serde_json::to_string(&PublicationState::Published).unwrap(),
        "\"published\""
    );
}

// --- CEG: migration idempotency ----------------------------------------------

#[tokio::test]
async fn reopening_the_same_file_reruns_migrations_idempotently_and_keeps_data() {
    // Opening a store runs the migration bundle; the ledger makes a second open a
    // no-op (no error) and the previously written config survives the reopen.
    let path =
        std::env::temp_dir().join(format!("awaken_config_migidem_{}.db", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    {
        let store = SqliteConfigStore::open(&path).await.expect("first open");
        store
            .put_config(&agent_with("persist", "keep-me"))
            .await
            .expect("put");
    }
    // Second open re-applies the bundle; idempotent, and the row is still readable.
    let store2 = SqliteConfigStore::open(&path).await.expect("second open");
    let got = store2
        .get_config("persist")
        .await
        .expect("get")
        .expect("row survives reopen");
    assert_eq!(got.instructions, "keep-me");

    let _ = std::fs::remove_file(&path);
}

#[path = "support/personal_selection.rs"]
mod personal_selection;

#[tokio::test]
async fn sqlite_personal_agent_selection_preserves_original_outcome_and_owner_partition() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("personal-agent-concurrency.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    let other = SqliteConfigStore::open(path).await.unwrap();
    personal_selection::original_outcome(&store).await;
    personal_selection::concurrent_cas(&store, &other).await;
}

#[tokio::test]
async fn sqlite_personal_agent_selection_receipt_failure_rolls_back_the_preference() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("personal-agent-selection.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    sqlx::query("CREATE TRIGGER reject_selection_receipt BEFORE INSERT ON config_personal_agent_selection_receipt BEGIN SELECT RAISE(ABORT, 'receipt unavailable'); END")
        .execute(&pool).await.unwrap();
    let scope = ScopeId::from("fault-ws");
    let owner = personal_selection::owner("https://issuer.example", "a");
    let intent = personal_selection::intent("fault", 0, true);
    assert!(
        store
            .set_personal_agent_selection(&scope, &owner, "agent", &intent)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .personal_agent_selection(&scope, &owner, "agent")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .personal_agent_selection_outcome(&scope, &owner, "agent", "fault")
            .await
            .unwrap(),
        None
    );
}

#[path = "support/catalog_snapshot.rs"]
mod catalog_snapshot;

#[tokio::test]
async fn sqlite_catalog_snapshot_binds_exact_publication_owner_and_native_bound() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-frame.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    catalog_snapshot::read_frame(&store).await;
}

#[tokio::test]
async fn sqlite_catalog_epoch_survives_aba_and_rolls_back_with_its_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-epoch.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    catalog_snapshot::read_frame(&store).await;
    let scope = ScopeId::from("catalog");
    let before = store.agent_catalog_epoch(&scope).await.unwrap();
    // Every input of the snapshot participates, including immutable-history
    // corruption and the owner preference. No-op SQL updates still count as
    // database mutations and prevent delete/reinsert or same-value ABA reuse.
    for statement in [
        "UPDATE config_agent SET data=data WHERE scope_id='catalog'",
        "UPDATE config_agent_revision SET data=data WHERE scope_id='catalog'",
        "UPDATE config_publication SET record=record WHERE scope_id='catalog'",
        "UPDATE config_personal_agent_selection SET selected=selected WHERE scope_id='catalog'",
    ] {
        let prior = store.agent_catalog_epoch(&scope).await.unwrap();
        sqlx::query(statement).execute(&pool).await.unwrap();
        assert!(store.agent_catalog_epoch(&scope).await.unwrap() > prior);
    }
    let stable = store.agent_catalog_epoch(&scope).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE config_agent SET data=data WHERE scope_id='catalog'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        store.agent_catalog_epoch(&scope).await.unwrap(),
        stable,
        "rolled-back epoch must not escape"
    );
    let original: String =
        sqlx::query_scalar("SELECT data FROM config_agent WHERE scope_id='catalog' AND id='agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("UPDATE config_agent SET data=json_set(data, '$.name', 'ABA') WHERE scope_id='catalog' AND id='agent'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE config_agent SET data=?1 WHERE scope_id='catalog' AND id='agent'")
        .bind(&original)
        .execute(&pool)
        .await
        .unwrap();
    let restored: String =
        sqlx::query_scalar("SELECT data FROM config_agent WHERE scope_id='catalog' AND id='agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        restored, original,
        "byte/value restoration does not restore the epoch"
    );
    assert!(store.agent_catalog_epoch(&scope).await.unwrap() > stable);
    // Removing/reinserting the same head cannot recycle its original token.
    sqlx::query("DELETE FROM config_agent_revision WHERE scope_id='catalog' AND id='agent'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM config_agent WHERE scope_id='catalog' AND id='agent'")
        .execute(&pool)
        .await
        .unwrap();
    store
        .put_config_scoped(
            &scope,
            &catalog_snapshot::config("agent", "Other target name"),
        )
        .await
        .unwrap();
    assert!(store.agent_catalog_epoch(&scope).await.unwrap() > before);
}

#[tokio::test]
async fn sqlite_catalog_missing_epoch_is_corruption_not_an_empty_scope() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-missing.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    let scope = ScopeId::from("catalog");
    store
        .put_config_scoped(&scope, &catalog_snapshot::config("agent", "Published"))
        .await
        .unwrap();
    sqlx::query("DELETE FROM config_agent_catalog_epoch WHERE scope_id='catalog'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.agent_catalog_epoch(&scope).await.is_err());
    assert!(
        store
            .agent_catalog_snapshot(&scope, &catalog_snapshot::owner(), 10)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sqlite_catalog_epoch_overflow_rejects_the_whole_config_write() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-overflow.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    let scope = ScopeId::from("catalog");
    let original = catalog_snapshot::config("agent", "Published");
    store.put_config_scoped(&scope, &original).await.unwrap();
    sqlx::query(
        "UPDATE config_agent_catalog_epoch SET epoch=9223372036854775807 WHERE scope_id='catalog'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        store
            .put_config_scoped(&scope, &catalog_snapshot::config("agent", "Not committed"))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_config_scoped(&scope, "agent")
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        store
            .list_config_revisions_scoped(&scope, "agent")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store.agent_catalog_epoch(&scope).await.unwrap(),
        Some(i64::MAX as u64)
    );
}

#[tokio::test]
async fn sqlite_catalog_corrupt_publication_is_not_a_usable_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-publication.sqlite");
    let path = path.to_str().unwrap();
    let store = SqliteConfigStore::open(path).await.unwrap();
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    catalog_snapshot::read_frame(&store).await;
    let scope = ScopeId::from("catalog");
    let owner = catalog_snapshot::owner();
    let original:String=sqlx::query_scalar("SELECT record FROM config_publication WHERE scope_id='catalog' AND COALESCE(json_extract(record, '$.execution_workspace'), scope_id)='catalog'").fetch_one(&pool).await.unwrap();
    for (field, value) in [
        ("state", "compiled"),
        ("agent_id", "second"),
        ("fingerprint", "wrong-fingerprint"),
        ("publication_id", "wrong-publication"),
    ] {
        let mut corrupt: serde_json::Value = serde_json::from_str(&original).unwrap();
        corrupt[field] = serde_json::json!(value);
        sqlx::query("UPDATE config_publication SET record=?1 WHERE scope_id='catalog' AND COALESCE(json_extract(record, '$.execution_workspace'), scope_id)='catalog'").bind(corrupt.to_string()).execute(&pool).await.unwrap();
        assert!(
            store
                .agent_catalog_snapshot(&scope, &owner, 2)
                .await
                .is_err(),
            "corrupt {field} must not be projected as a real publication"
        );
        sqlx::query("UPDATE config_publication SET record=?1 WHERE scope_id='catalog' AND COALESCE(json_extract(record, '$.execution_workspace'), scope_id)='catalog'").bind(&original).execute(&pool).await.unwrap();
        assert!(
            store
                .agent_catalog_snapshot(&scope, &owner, 2)
                .await
                .is_ok(),
            "exact restore of {field} preserves valid native rows"
        );
    }
}

#[tokio::test]
async fn sqlite_owner_selection_snapshot_retains_bounded_orphans_and_original_receipt() {
    let store = SqliteConfigStore::open_in_memory().await.unwrap();
    catalog_snapshot::owner_preferences_without_definition(&store).await;
}
