//! Run the runtime end to end with **direct configuration** — no config store.
//!
//! This is the first lesson: build the catalog and the executable snapshot by
//! hand, wire the runtime's ports (model, tool, permission gate), execute one
//! run, and read the committed transcript. Example #2 (`config_store_runtime`)
//! shows the config store *producing* the snapshot this example builds by hand.
//!
//! Run:
//! ```text
//! cargo run -p awaken-runtime-examples --example direct_runtime
//! ```
//! It uses a deterministic stub model, so no API key is needed.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_examples::{EchoTool, ScriptedLlm};

#[tokio::main]
async fn main() {
    // 1. Pick the catalog fingerprint. With a config store this is sha256(config);
    //    here we choose it directly. The snapshot, its resolved spec, and the
    //    install must all carry the SAME fingerprint, or resolution fails closed.
    let fingerprint = CatalogFingerprint("demo-v1".to_string());

    // 2. Describe the one tool the agent may use.
    let echo = ToolDescriptor::pinned(
        "demo",
        "echo",
        "Echo back the given text",
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
    );

    // 3. Build the executable snapshot by hand (this is what a config store would
    //    otherwise compile for you).
    let snapshot = ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("assistant".to_string()),
        root_agent_id: AgentId("assistant".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fingerprint.clone(),
            instructions: "You are a concise assistant.".to_string(),
            max_steps: 8,
            model_binding: ModelBinding {
                provider_instance_ref: "demo".to_string(),
                model_ref: "stub".to_string(),
                backend_ref: "stub".to_string(),
            },
            tool_descriptors: vec![echo],
            plugin_ids: Vec::new(),
        },
        fingerprint: fingerprint.clone(),
    };

    // 4. The matching install candidate — same fingerprint as the snapshot.
    let install = RuntimeCatalogInstall {
        publication_id: "demo-pub".to_string(),
        fingerprint: fingerprint.clone(),
        source_revisions: vec!["hand-written".to_string()],
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: fingerprint,
            runtime_version: "demo".to_string(),
            tools: Vec::new(),
            plugins: Vec::new(),
        },
    };

    // 5. A permission policy (Claude-Code-style): allow `echo`, ask for anything
    //    else. The gate is the single authorization path.
    let ruleset = PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Ask,
        mode: Mode::Default,
        rules: vec![PermissionRule::new(
            ToolCallPattern::parse("echo").unwrap(),
            ToolPermissionBehavior::Allow,
        )],
    };
    let gate = PermissionGate::new(Arc::new(RulePermissionPolicy::new(ruleset)));

    // 6. Assemble the runtime from its ports: model + tool + gate. Swap
    //    `ScriptedLlm` for `awaken_provider_genai::GenAiExecutor::new()` to use a
    //    real model (set OPENAI_API_KEY / ANTHROPIC_API_KEY).
    let runtime = Runtime::new()
        .with_llm(Arc::new(ScriptedLlm::default()))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(gate));

    // 7. Install the catalog and register the snapshot for resolution.
    runtime.install_catalog(install).expect("install catalog");
    runtime.register_snapshot(snapshot.clone());

    // 8. The durable commit boundary (in-memory here; swap a SQLite coordinator
    //    to persist across restarts).
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());

    // 9. Build the activation (snapshot + user input) and execute one run.
    let activation = RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot,
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("Say hi.")],
        }],
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    };
    let phase = runtime.execute(activation, context).await.expect("execute");

    // 10. Inspect the committed transcript.
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    println!("run finished: {phase:?}\n--- committed transcript ---");
    for message in commit.committed().messages {
        println!("[{:?}] {}", message.role, message.text_content());
    }
}
