//! The smallest agent: instructions + a model, **no tools, no permissions**.
//!
//! It is the counterpart to `direct_runtime`. Where that example builds the
//! snapshot by hand — and pays for it by writing the catalog fingerprint four
//! times — this one declares the agent as *data* and lets the config domain's
//! pure `compile()` derive the fingerprint once. You never write or align a
//! fingerprint by hand; the producer stamps it into the snapshot and the install.
//!
//! `compile()` is the pure half of `awaken-config-store`: the cloud uses it to
//! auto-publish configs, but it has no storage dependency, so an embedded app can
//! borrow just the producer. The same derivation runs in both places — one hash,
//! no drift.
//!
//! Run:
//! ```text
//! cargo run -p awaken-runtime-examples --example hello_agent
//! ```
//! It uses a deterministic stub model, so no API key is needed.

use std::sync::Arc;

use awaken_config_store::{AgentConfig, compile};
use awaken_runtime_examples::prelude::*;

#[tokio::main]
async fn main() {
    // 1. Declare the agent as data. Note what is *absent*: no fingerprint — that
    //    is derived, not chosen — and no tools.
    let config = AgentConfig {
        id: "greeter".to_string(),
        instructions: "You are a friendly greeter.".to_string(),
        max_steps: 4,
        model_binding: ModelBinding {
            provider_instance_ref: "demo".to_string(),
            model_ref: "stub".to_string(),
            backend_ref: "stub".to_string(),
        },
        tool_ids: Vec::new(),
    };

    // 2. Compile: config → content-addressed publication. The fingerprint is
    //    sha256(config), computed once here and stamped into both the snapshot and
    //    the install — the runtime later re-checks they agree (fail-closed).
    let publication = compile(&config, &[]).expect("compile config");

    // 3. Assemble the runtime and install the compiled catalog + snapshot.
    let runtime = Runtime::new().with_llm(Arc::new(GreeterLlm));
    runtime
        .install_catalog(publication.install)
        .expect("install catalog");
    runtime.register_snapshot(publication.snapshot.clone());

    // 4. Run one turn against the durable commit boundary (in-memory here).
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());
    let activation = RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: publication.snapshot,
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

    // 5. Read the committed transcript.
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    println!("run finished: {phase:?}\n--- committed transcript ---");
    for message in commit.committed().messages {
        println!("[{:?}] {}", message.role, message.text_content());
    }
}
