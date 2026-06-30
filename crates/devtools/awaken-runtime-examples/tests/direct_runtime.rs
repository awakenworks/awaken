//! CI guard for the `direct_runtime` example: the same assembly, asserted, so the
//! teaching example cannot rot. The example file is the readable artifact; this
//! is its smoke test.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_permission::{
    Mode, PermissionRuleset, RulePermissionPolicy, ToolPermissionBehavior,
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

#[tokio::test]
async fn direct_runtime_example_runs_to_completion() {
    let fp = CatalogFingerprint("test-v1".to_string());
    let snapshot = ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("assistant".to_string()),
        root_agent_id: AgentId("assistant".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 8,
            model_binding: ModelBinding {
                provider_instance_ref: "demo".to_string(),
                model_ref: "stub".to_string(),
                backend_ref: "stub".to_string(),
            },
            tool_descriptors: vec![ToolDescriptor::pinned(
                "demo",
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids: Vec::new(),
        },
        fingerprint: fp.clone(),
    };
    let install = RuntimeCatalogInstall {
        publication_id: "p".to_string(),
        fingerprint: fp.clone(),
        source_revisions: vec!["t".to_string()],
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: fp,
            runtime_version: "demo".to_string(),
            tools: Vec::new(),
            plugins: Vec::new(),
        },
    };

    // Allow everything (default behavior) — the example uses a more selective set.
    let policy = RulePermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Allow,
        mode: Mode::Default,
        rules: Vec::new(),
    });
    let runtime = Runtime::new()
        .with_llm(Arc::new(ScriptedLlm::default()))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(PermissionGate::new(Arc::new(policy))));
    runtime.install_catalog(install).expect("install");
    runtime.register_snapshot(snapshot.clone());

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());
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

    let phase = runtime.execute(activation, ctx).await.expect("execute");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content().contains("echoed: hello")),
        "the allowed tool ran and its result was committed"
    );
}
