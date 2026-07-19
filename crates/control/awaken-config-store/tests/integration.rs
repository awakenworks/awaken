//! Config-store end to end (ADR-0031): a declarative config compiles into a
//! content-addressed publication, is stored and reloaded, and the runtime
//! installs and executes the produced snapshot. Plus: the `config_*` tables
//! coexist with the runtime's `runtime_*` tables in one SQLite database, isolated
//! by namespace (ADR-0029).

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_config_store::{
    AgentConfig, AuditedConfigWrite, ConfigRegistry, ConfigWrite, DEFAULT_SCOPE,
    ManagementAuditRecord, ManagementEffect, ModelSelection, PublicationState, ScopeId,
    ScopedConfig, ScopedConfigRegistry, SqliteConfigStore, StoredPublication, compile,
};
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

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
        max_steps: 8,
        delegation_limits: Default::default(),
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
    let store = SqliteConfigStore::open_in_memory().expect("store");
    let scope = ScopeId::from("ws");
    let audit = ManagementAuditRecord {
        tool: "admin_draft_agent".into(),
        call_id: "resource-call".into(),
        summary: "draft with resource".into(),
    };
    let effect = ManagementEffect {
        kind: "agent_resource_binding".into(),
        key: "support-agent".into(),
        payload: serde_json::json!({"agent_id":"support-agent","resources":[],"version":1}),
    };
    assert_eq!(
        store
            .put_config_with_audit_effect_scoped(&scope, &agent_config(), &audit, Some(&effect))
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
            .put_config_with_audit_effect_scoped(&scope, &agent_config(), &audit, Some(&effect))
            .await
            .unwrap(),
        AuditedConfigWrite::Replayed
    );
    store
        .complete_management_effect_scoped(&scope, &effect.kind, &effect.key)
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
async fn config_generation_cas_rejects_a_stale_writer_without_lost_update() {
    let store = SqliteConfigStore::open_in_memory().expect("store");
    let mut first = agent_with("cas", "v1");
    assert_eq!(
        store.put_config_if_generation(&first, 0).await.unwrap(),
        ConfigWrite::Applied { generation: 1 }
    );
    let snapshot = store
        .get_config_versioned("cas")
        .await
        .unwrap()
        .expect("created config");
    assert_eq!(snapshot.generation, 1);

    first.instructions = "v2".into();
    assert_eq!(
        store
            .put_config_if_generation(&first, snapshot.generation)
            .await
            .unwrap(),
        ConfigWrite::Applied { generation: 2 }
    );
    let mut stale = snapshot.config;
    stale.instructions = "stale-overwrite".into();
    assert_eq!(
        store
            .put_config_if_generation(&stale, snapshot.generation)
            .await
            .unwrap(),
        ConfigWrite::Conflict {
            current_generation: Some(2)
        }
    );
    let current = store
        .get_config_versioned("cas")
        .await
        .unwrap()
        .expect("current config");
    assert_eq!(current.generation, 2);
    assert_eq!(current.config.instructions, "v2");
}

#[tokio::test]
async fn config_compiles_stores_and_the_runtime_executes_the_snapshot() {
    let config = agent_config();

    // Config domain: compile to a content-addressed publication, then persist.
    let publication = compile(&config, &tool_catalog()).expect("compile");
    let store = SqliteConfigStore::open_in_memory().expect("store");
    store.put_config(&config).await.expect("put config");
    store
        .put_publication(&StoredPublication::published(
            publication.clone(),
            &config.id,
        ))
        .await
        .expect("put publication");

    // Reload the publication by its fingerprint — the durable round trip.
    let loaded = store
        .get_publication(&publication.snapshot().fingerprint.0)
        .await
        .expect("get")
        .expect("publication exists");
    assert_eq!(loaded.fingerprint, publication.snapshot().fingerprint.0);
    assert_eq!(
        store.get_config("support-agent").await.unwrap().as_ref(),
        Some(&config)
    );

    // Runtime: install the produced catalog and execute the produced snapshot.
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));
    runtime
        .install_catalog(install_of(&loaded.install))
        .expect("install");
    runtime.register_snapshot(loaded.snapshot.clone());

    let activation = RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: loaded.snapshot,
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        delegation_origin: None,
        model_ref_override: None,
    };
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    // The snapshot's fingerprints match the installed catalog, so resolution
    // passes and the run completes — config produced what the runtime consumed.
    let state = runtime.execute(activation, ctx).await.expect("execute");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // Per-step durability: the input commits at the first step boundary
    // (under a Running fact), then the text-only terminal step commits
    // through finish.
    assert_eq!(commit.commit_count(), 2);
}

/// Rebuild an owned `RuntimeCatalogInstall` (the contract type is not `Clone`).
fn install_of(install: &RuntimeCatalogInstall) -> RuntimeCatalogInstall {
    RuntimeCatalogInstall {
        publication_id: install.publication_id.clone(),
        fingerprint: install.fingerprint.clone(),
        source_revisions: install.source_revisions.clone(),
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: install.capabilities.catalog_fingerprint.clone(),
            runtime_version: install.capabilities.runtime_version.clone(),
            tools: Vec::new(),
            plugins: Vec::new(),
        },
    }
}

#[tokio::test]
async fn config_and_runtime_tables_coexist_in_one_database() {
    use awaken_store_sqlite::SqliteCommitCoordinator;

    let path =
        std::env::temp_dir().join(format!("awaken_config_coexist_{}.db", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    // Both components migrate into the same file under their own namespaces.
    let config = SqliteConfigStore::open(&path).expect("config store");
    let _commit = SqliteCommitCoordinator::open(&path).expect("commit store");
    config
        .put_config(&agent_config())
        .await
        .expect("put config");

    // A raw view of the schema: both namespaces' tables and ledgers are present,
    // with no collision.
    let conn = rusqlite::Connection::open(&path).expect("open raw");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    for expected in [
        "config_agent",
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

    drop(stmt);
    drop(conn);
    let _ = std::fs::remove_file(&path);
}

// --- CEG 03 / B7 (StoredPublication::published) ------------------------------

fn scoped_agent(id: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: "be helpful".to_string(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("p", "m", "b"),
        tool_ids: Vec::new(),
        ..Default::default()
    }
}

#[test]
fn published_always_stamps_published_and_takes_ids_from_the_runnable() {
    // B7(a): `published` always sets state=Published and lifts the fingerprint and
    // publication id from the runnable itself (never re-derived by the store).
    let cfg = agent_config();
    let runnable = compile(&cfg, &tool_catalog()).expect("compile");
    let want_fingerprint = runnable.snapshot().fingerprint.0.clone();
    let want_pub_id = runnable.install().publication_id.clone();

    let stored = StoredPublication::published(runnable, &cfg.id);
    assert_eq!(stored.state, PublicationState::Published);
    assert_eq!(stored.fingerprint, want_fingerprint);
    assert_eq!(stored.publication_id, want_pub_id);
    assert_eq!(stored.agent_id, cfg.id);
    // The wrapped snapshot/install agree on that same fingerprint.
    assert_eq!(stored.snapshot.fingerprint.0, want_fingerprint);
    assert_eq!(stored.install.fingerprint.0, want_fingerprint);
}

#[test]
fn published_is_idempotent_for_the_same_runnable() {
    // B7(b): the runnable is content-addressed, so wrapping two compiles of the same
    // config yields the same identity (fingerprint + publication id + state).
    let cfg = agent_config();
    let a = StoredPublication::published(compile(&cfg, &tool_catalog()).unwrap(), &cfg.id);
    let b = StoredPublication::published(compile(&cfg, &tool_catalog()).unwrap(), &cfg.id);
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_eq!(a.publication_id, b.publication_id);
    assert_eq!(a.state, b.state);
}

// --- CEG 03 / B8 (ScopedConfig decorator) ------------------------------------

#[tokio::test]
async fn scoped_config_isolates_writes_and_lists_across_scopes() {
    // B8(a)+(b)+(d): the decorator binds one scope and exposes the scope-free port.
    // A write under scope A is invisible to scope B's get *and* list, and B cannot
    // clobber A's row of the same id.
    let store = Arc::new(SqliteConfigStore::open_in_memory().expect("store"));
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

    // (b) B cannot clobber A's row of the same id (conflict guard → no-op).
    b.put_config(&scoped_agent("shared")).await.expect("put b");
    assert!(a.get_config("shared").await.expect("get a").is_some());
    assert!(b.get_config("shared").await.expect("get b").is_none());
}

#[tokio::test]
async fn scoped_config_default_scope_shares_the_owner_with_scope_free_writes() {
    // B8(c): a scope-free write lands under DEFAULT_SCOPE, so a ScopedConfig bound to
    // DEFAULT_SCOPE reads it back (same owner), while a foreign scope cannot.
    let store = Arc::new(SqliteConfigStore::open_in_memory().expect("store"));
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
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("p", "m", "b"),
        tool_ids: Vec::new(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_same_scope_re_put_updates_the_config_data() {
    // put_config_scoped ON CONFLICT … DO UPDATE (same-scope branch): a re-put by the
    // owner replaces the stored data (upsert, not insert-only).
    let store = SqliteConfigStore::open_in_memory().expect("store");
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
async fn a_cross_scope_write_leaves_the_owners_data_intact() {
    // The isolation fence must protect DATA, not merely visibility: a foreign scope's
    // write to the owner's id is a no-op even when it carries different data.
    let store = SqliteConfigStore::open_in_memory().expect("store");
    let a = ScopeId::from("ws_a");
    let b = ScopeId::from("ws_b");
    store
        .put_config_scoped(&a, &agent_with("x", "A-data"))
        .await
        .expect("put a");
    // ws_b tries to hijack id "x" with different data — the conflict guard blocks it.
    store
        .put_config_scoped(&b, &agent_with("x", "B-data"))
        .await
        .expect("put b");
    let owner = store
        .get_config_scoped(&a, "x")
        .await
        .expect("get a")
        .expect("row a");
    assert_eq!(owner.instructions, "A-data"); // untouched by the foreign write
    assert!(
        store
            .get_config_scoped(&b, "x")
            .await
            .expect("get b")
            .is_none()
    );
}

#[tokio::test]
async fn list_configs_scoped_returns_ids_ascending() {
    // list_configs_scoped ORDER BY id ASC, filtered to the scope.
    let store = SqliteConfigStore::open_in_memory().expect("store");
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

// --- CEG: publication scope isolation + idempotency + warm-load list ---------

fn publication_for(cfg: &AgentConfig) -> StoredPublication {
    let runnable = compile(cfg, &tool_catalog()).expect("compile");
    StoredPublication::published(runnable, &cfg.id)
}

#[tokio::test]
async fn a_publication_written_under_scope_a_is_invisible_to_scope_b() {
    // get_publication_scoped carries `AND scope_id = ?`: a publication owned by A is
    // unreadable by B even by its exact fingerprint (the tenancy fence on the
    // compiled artifact, not just the authoring aggregate).
    let store = Arc::new(SqliteConfigStore::open_in_memory().expect("store"));
    let a = ScopedConfig::new(store.clone(), ScopeId::from("ws_a"));
    let b = ScopedConfig::new(store.clone(), ScopeId::from("ws_b"));
    let publication = publication_for(&agent_with("shared", "hello"));
    let fp = publication.fingerprint.clone();
    a.put_publication(&publication).await.expect("put a");
    assert!(a.get_publication(&fp).await.expect("get a").is_some());
    assert!(b.get_publication(&fp).await.expect("get b").is_none());
}

#[tokio::test]
async fn a_publication_fingerprint_belongs_to_its_first_writer_across_scopes() {
    // Fingerprints are global content addresses (the publication PK), so two scopes
    // compiling the same config collide. ON CONFLICT(fingerprint) DO NOTHING means the
    // first writer owns the row: B's identical write is a silent no-op and B still
    // cannot read it — a fail-safe (no cross-tenant leak), never a takeover.
    let store = Arc::new(SqliteConfigStore::open_in_memory().expect("store"));
    let a = ScopedConfig::new(store.clone(), ScopeId::from("ws_a"));
    let b = ScopedConfig::new(store.clone(), ScopeId::from("ws_b"));
    let pub_a = publication_for(&agent_with("shared", "same-bytes"));
    let pub_b = publication_for(&agent_with("shared", "same-bytes"));
    assert_eq!(pub_a.fingerprint, pub_b.fingerprint); // content-addressed → identical
    a.put_publication(&pub_a).await.expect("put a");
    b.put_publication(&pub_b).await.expect("put b (no-op)");
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
            .is_none()
    );
}

#[tokio::test]
async fn put_publication_scoped_is_idempotent_by_fingerprint() {
    // Re-putting the same publication is a no-op (DO NOTHING), not an error.
    let store = SqliteConfigStore::open_in_memory().expect("store");
    let a = ScopeId::from("ws_a");
    let publication = publication_for(&agent_with("agent", "body"));
    store
        .put_publication_scoped(&a, &publication)
        .await
        .expect("put");
    store
        .put_publication_scoped(&a, &publication)
        .await
        .expect("re-put idempotent");
    assert!(
        store
            .get_publication_scoped(&a, &publication.fingerprint)
            .await
            .expect("get")
            .is_some()
    );
}

#[tokio::test]
async fn list_published_returns_only_published_rows_of_the_scope_in_insertion_order() {
    // list_published_scoped: WHERE scope_id AND state='published' ORDER BY insertion.
    // Covers the state filter (compiled excluded), scope isolation, ordering, and the
    // empty-scope case — the warm-install reload contract.
    let store = SqliteConfigStore::open_in_memory().expect("store");
    let a = ScopeId::from("ws_a");
    let b = ScopeId::from("ws_b");

    let p1 = publication_for(&agent_with("a1", "first"));
    let p2 = publication_for(&agent_with("a2", "second"));
    let mut p_compiled = publication_for(&agent_with("a3", "third"));
    p_compiled.state = PublicationState::Compiled; // must be excluded
    let p_b = publication_for(&agent_with("b1", "other-scope"));

    store.put_publication_scoped(&a, &p1).await.expect("p1");
    store.put_publication_scoped(&a, &p2).await.expect("p2");
    store
        .put_publication_scoped(&a, &p_compiled)
        .await
        .expect("p_compiled");
    store.put_publication_scoped(&b, &p_b).await.expect("p_b");

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
    // TASK 1: multiple publications for the SAME agent id (distinct fingerprints via
    // distinct instructions). SQLite orders `list_published_scoped` by `rowid ASC` — a
    // monotonic per-row integer, i.e. insertion order — so a warm-load that folds the
    // oldest-first list into an agent-keyed map DETERMINISTICALLY keeps the last-inserted
    // publication per agent ("latest wins", the warm-install reload contract).
    let store = SqliteConfigStore::open_in_memory().expect("store");
    let a = ScopeId::from("ws_a");

    let v1 = publication_for(&agent_with("dup", "v1"));
    let v2 = publication_for(&agent_with("dup", "v2"));
    let v3 = publication_for(&agent_with("dup", "v3"));
    // Same agent, three distinct content addresses (fingerprint tracks instructions).
    assert_eq!(v1.agent_id, "dup");
    assert_eq!(v3.agent_id, "dup");
    assert_ne!(v1.fingerprint, v2.fingerprint);
    assert_ne!(v2.fingerprint, v3.fingerprint);

    store.put_publication_scoped(&a, &v1).await.expect("v1");
    store.put_publication_scoped(&a, &v2).await.expect("v2");
    store.put_publication_scoped(&a, &v3).await.expect("v3");

    let listed = store.list_published_scoped(&a).await.expect("list");
    // rowid ASC == insertion order, deterministically.
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

    // Warm-load fold: agent-keyed map, oldest-first, last write wins → v3, deterministically.
    let mut warm: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for p in &listed {
        warm.insert(p.agent_id.clone(), p.fingerprint.clone());
    }
    assert_eq!(warm.get("dup"), Some(&v3.fingerprint));
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
        let store = SqliteConfigStore::open(&path).expect("first open");
        store
            .put_config(&agent_with("persist", "keep-me"))
            .await
            .expect("put");
    }
    // Second open re-applies the bundle; idempotent, and the row is still readable.
    let store2 = SqliteConfigStore::open(&path).expect("second open");
    let got = store2
        .get_config("persist")
        .await
        .expect("get")
        .expect("row survives reopen");
    assert_eq!(got.instructions, "keep-me");

    let _ = std::fs::remove_file(&path);
}
