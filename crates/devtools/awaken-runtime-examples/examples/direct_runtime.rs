//! Run the runtime end to end with a **hand-built config** — no config store.
//!
//! Build an `ExecutableAgentSnapshot` directly with the builder, wire the runtime's ports
//! (model, tool, permission gate), and run one turn. No fingerprint is written by
//! hand — the builder stamps a consistent one (a compiler would stamp sha256).
//! Example #2 (`hello_agent`) compiles the same kind of config from a config store;
//! the `runtime.run` call is identical.
//!
//! Run:
//! ```text
//! cargo run -p awaken-runtime-examples --example direct_runtime
//! ```
//! It uses a deterministic stub model, so no API key is needed.

use std::sync::Arc;

use awaken_runtime_examples::prelude::*;

#[tokio::main]
async fn main() {
    // 1. Build the executable snapshot directly. The builder stamps one
    //    fingerprint into its envelope and resolved payload.
    let config = ExecutableAgentSnapshot::builder("assistant")
        .instructions("You are a concise assistant.")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .tool(ToolDescriptor::pinned(
            "demo",
            "echo",
            "Echo back the given text",
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        ))
        .max_steps(8)
        .build();

    // 2. A permission policy (Claude-Code-style): allow `echo`, ask for anything
    //    else. The gate is the single authorization path.
    let ruleset = PermissionRuleset {
        default_behavior: ToolPermissionBehavior::RequireConfirmation,
        mode: Mode::Default,
        rules: vec![PermissionRule::new(
            ToolCallPattern::parse("echo").unwrap(),
            ToolPermissionBehavior::Allow,
        )],
    };
    let gate = PermissionGate::new(Arc::new(RuleBasedToolPermissionPolicy::new(ruleset)));

    // 3. Assemble the runtime from its ports: model + tool + gate. Swap
    //    `ScriptedLlm` for `awaken_provider_genai::GenAiExecutor::new()` to use a
    //    real model (set OPENAI_API_KEY / ANTHROPIC_API_KEY).
    let runtime = Runtime::new()
        .with_llm(Arc::new(ScriptedLlm::default()))
        .with_tool(Arc::new(EchoTool))
        .with_gate(Arc::new(gate));

    // 4. Run one turn. `run` installs the config's catalog and executes — no
    //    separate install/register, no hand-built activation. Swap the in-memory
    //    commit for a SQLite coordinator to persist across restarts.
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.run(&config, "Say hi.", context).await.expect("run");

    // 5. Inspect the committed transcript.
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    println!("run finished: {state:?}\n--- committed transcript ---");
    for message in commit.committed().messages {
        println!("[{:?}] {}", message.role, message.text_content());
    }
}
