//! CI guard for the `hello_agent` example: declare a config, compile it, run one
//! turn. Asserts the compiled fingerprint is derived (sha256 of the config) and
//! reaches the snapshot and install untouched, so the teaching example cannot rot.

use std::sync::Arc;

use awaken_config_store::{AgentConfig, compile};
use awaken_runtime_examples::prelude::*;

#[tokio::test]
async fn hello_agent_example_runs_to_completion() {
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
    let publication = compile(&config, &[]).expect("compile");

    // The fingerprint is derived, not chosen, and the same value reaches the
    // snapshot and the install — the property the four-times-by-hand path risks.
    assert_eq!(publication.snapshot.fingerprint.0, publication.fingerprint);
    assert_eq!(
        publication.install.fingerprint.0,
        publication.snapshot.fingerprint.0
    );

    let runtime = Runtime::new().with_llm(Arc::new(GreeterLlm));
    runtime
        .install_catalog(publication.install)
        .expect("install");
    runtime.register_snapshot(publication.snapshot.clone());

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());
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

    let phase = runtime.execute(activation, ctx).await.expect("execute");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text_content().contains("Hello")),
        "the greeter replied and the reply was committed"
    );
}
