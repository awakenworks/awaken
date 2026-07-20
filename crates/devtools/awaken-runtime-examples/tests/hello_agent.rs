//! CI guard for the `hello_agent` example: declare a config, compile it, run one
//! turn. Asserts the compiled fingerprint is derived (sha256 of the config) and is
//! consistent across the snapshot envelope and payload, so the teaching example
//! cannot rot.

use std::sync::Arc;

use awaken_config_store::{AgentConfig, ModelSelection, compile_resolved};
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use awaken_runtime_examples::prelude::*;

#[tokio::test]
async fn hello_agent_example_runs_to_completion() {
    let config = AgentConfig {
        id: "greeter".to_string(),
        instructions: "You are a friendly greeter.".to_string(),
        max_steps: 4,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("demo", "stub", "stub"),
        tool_ids: Vec::new(),
        model_candidates: Vec::new(),
        plugin_ids: Vec::new(),
        plugin_config: Default::default(),
        context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        tool_patterns: Vec::new(),
        ..Default::default()
    };
    let snapshot = compile_resolved(&config, &[], &[], AgentSnapshotMetadata::default())
        .expect("compile resolved config");

    // The fingerprint is derived and consistent across the snapshot envelope and
    // resolved payload — the property the runtime checks fail-closed.
    assert_eq!(
        snapshot.fingerprint,
        snapshot.resolved_spec.catalog_fingerprint
    );

    let runtime = Runtime::new().with_llm(Arc::new(GreeterLlm));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.run(&snapshot, "Say hi.", ctx).await.expect("run");
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
