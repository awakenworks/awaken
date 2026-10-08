//! CI guard for the `direct_runtime` example: build a config by hand, run it,
//! assert the allowed tool ran. The example is the readable artifact; this is its
//! smoke test.

use std::sync::Arc;

use awaken_runtime_examples::prelude::*;

#[tokio::test]
async fn direct_runtime_example_runs_to_completion() {
    // Causes: C1 a hand-built snapshot offers echo; C2 the canonical policy
    // allows it; C3 the scripted model requests it then terminates; C4 one
    // MemoryCommitCoordinator is the only durable write port. Rule R1 all =>
    // NaturalEnd plus the committed tool result. The independent SDK consumer
    // compiles this exact case outside the Platform workspace; no second
    // Runtime, dispatcher, credential authority or fixture implementation exists.
    let config = ExecutableAgentSnapshot::builder("assistant")
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
    let policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Allow,
        rules: Vec::new(),
    });
    let runtime = Runtime::new()
        .with_llm(Arc::new(ScriptedLlm::default()))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(PermissionGate::new(Arc::new(policy))));

    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.run(&config, "Say hi.", ctx).await.expect("run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content().contains("echoed: hello")),
        "the allowed tool ran and its result was committed"
    );
}
