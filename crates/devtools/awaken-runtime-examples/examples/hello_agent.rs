//! The smallest agent: instructions + a model, **no tools, no permissions**.
//!
//! It declares an `AgentConfig` (external authoring config), compiles it to a
//! `ExecutableAgentSnapshot`, and runs one turn. `compile_resolved()` derives the fingerprint from
//! the config; `runtime.run` executes that exact snapshot in one call. The
//! counterpart `direct_runtime` builds the `ExecutableAgentSnapshot` by hand instead — the
//! `runtime.run` line is the same.
//!
//! `compile_resolved()` is the pure half of `awaken-config-store` (no storage dependency),
//! so an embedded app can borrow just the producer.
//!
//! Run:
//! ```text
//! cargo run -p awaken-runtime-examples --example hello_agent
//! ```
//! It uses a deterministic stub model, so no API key is needed.

use std::sync::Arc;

use awaken_config_store::{AgentConfig, ModelSelection, compile_resolved};
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use awaken_runtime_examples::prelude::*;

#[tokio::main]
async fn main() {
    // 1. Declare the agent as data. No fingerprint (derived), no tools.
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

    // 2. Compile to an executable snapshot — the fingerprint is sha256(config), stamped
    //    into the snapshot and the install for you.
    let snapshot = compile_resolved(&config, &[], &[], AgentSnapshotMetadata::default())
        .expect("compile resolved config");

    // 3. Assemble the runtime and run one turn.
    let runtime = Runtime::new().with_llm(Arc::new(GreeterLlm));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .run(&snapshot, "Say hi.", context)
        .await
        .expect("run");

    // 4. Read the committed transcript.
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    println!("run finished: {state:?}\n--- committed transcript ---");
    for message in commit.committed().messages {
        println!("[{:?}] {}", message.role, message.text_content());
    }
}
