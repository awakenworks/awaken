//! CI guard for the `direct_runtime` example: build a config by hand, run it,
//! assert the allowed tool ran. The example is the readable artifact; this is its
//! smoke test.

use std::sync::Arc;

use awaken_runtime_examples::prelude::*;

#[tokio::test]
async fn direct_runtime_example_runs_to_completion() {
    let config = RunnableConfig::builder("assistant")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .tool(ToolDescriptor::pinned(
            "demo",
            "echo",
            "Echo",
            serde_json::json!({"type": "object"}),
        ))
        .max_steps(8)
        .build();

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

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());

    let phase = runtime.run(&config, "Say hi.", ctx).await.expect("run");
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
