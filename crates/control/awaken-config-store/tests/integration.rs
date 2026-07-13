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
    AgentConfig, ConfigRegistry, SqliteConfigStore, StoredPublication, compile,
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
