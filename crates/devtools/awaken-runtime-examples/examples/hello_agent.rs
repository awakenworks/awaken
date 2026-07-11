//! The smallest agent: instructions + a model, **no tools, no permissions**.
//!
//! It declares an `AgentConfig` (external authoring config), compiles it to a
//! `RunnableConfig`, and runs one turn. `compile()` derives the fingerprint from
//! the config; `runtime.run` installs the catalog and executes in one call. The
//! counterpart `direct_runtime` builds the `RunnableConfig` by hand instead — the
//! `runtime.run` line is the same.
//!
//! `compile()` is the pure half of `awaken-config-store` (no storage dependency),
//! so an embedded app can borrow just the producer.
//!
//! Run:
//! ```text
//! cargo run -p awaken-runtime-examples --example hello_agent
//! ```
//! It uses a deterministic stub model, so no API key is needed.

use std::sync::Arc;

use awaken_config_store::{AgentConfig, ModelSelection, compile};
use awaken_runtime_examples::prelude::*;

#[tokio::main]
async fn main() {
    // 1. Declare the agent as data. No fingerprint (derived), no tools.
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
    };

    // 2. Compile to a runnable config — the fingerprint is sha256(config), stamped
    //    into the snapshot and the install for you.
    let runnable = compile(&config, &[]).expect("compile config");

    // 3. Assemble the runtime and run one turn.
    let runtime = Runtime::new().with_llm(Arc::new(GreeterLlm));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime
        .run(&runnable, "Say hi.", context)
        .await
        .expect("run");

    // 4. Read the committed transcript.
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    println!("run finished: {phase:?}\n--- committed transcript ---");
    for message in commit.committed().messages {
        println!("[{:?}] {}", message.role, message.text_content());
    }
}
