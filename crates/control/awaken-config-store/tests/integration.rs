//! Config-store end to end (ADR-0031): a declarative config compiles into a
//! content-addressed publication, is stored and reloaded, and the runtime
//! installs and executes the produced snapshot. Plus: the `config_*` tables
//! coexist with the runtime's `runtime_*` tables in one SQLite database, isolated
//! by namespace (ADR-0029).

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_config_store::{
    AgentConfig, ConfigRegistry, DEFAULT_SCOPE, ModelSelection, PublicationState, ScopeId,
    ScopedConfig, SqliteConfigStore, StoredPublication, compile,
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
        trace: Default::default(),
        model_access: Default::default(),
    };
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    // The snapshot's fingerprints match the installed catalog, so resolution
    // passes and the run completes — config produced what the runtime consumed.
    let phase = runtime.execute(activation, ctx).await.expect("execute");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
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
