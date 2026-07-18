//! CI guard for the `hello_agent` example: declare a config, compile it, run one
//! turn. Asserts the compiled fingerprint is derived (sha256 of the config) and is
//! consistent across the snapshot and install, so the teaching example cannot rot.

use std::sync::Arc;

use awaken_config_store::{AgentConfig, ModelSelection, compile};
use awaken_runtime_examples::prelude::*;

#[tokio::test]
async fn hello_agent_example_runs_to_completion() {
    let config = AgentConfig {
        id: "greeter".to_string(),
        instructions: "You are a friendly greeter.".to_string(),
        max_steps: 4,
        model_binding: ModelSelection::pinned("demo", "stub", "stub"),
        tool_ids: Vec::new(),
        model_candidates: Vec::new(),
        plugin_ids: Vec::new(),
        plugin_config: Default::default(),
        context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        tool_patterns: Vec::new(),
        ..Default::default()
    };
    let runnable = compile(&config, &[]).expect("compile");

    // The fingerprint is derived and consistent across snapshot and install — the
    // property the runtime checks on resolution (fail-closed).
    assert_eq!(
        runnable.snapshot().fingerprint.0,
        runnable.install().fingerprint.0
    );

    let runtime = Runtime::new().with_llm(Arc::new(GreeterLlm));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.run(&runnable, "Say hi.", ctx).await.expect("run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text_content().contains("Hello")),
        "the greeter replied and the reply was committed"
    );
}
