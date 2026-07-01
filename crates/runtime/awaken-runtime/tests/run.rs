//! `Runtime::run`: one call installs the config's catalog and executes a turn.
//! No separate `install_catalog`/`register_snapshot`, no hand-built activation.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

struct TextLlm(&'static str);

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0.to_string()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn run_installs_and_executes_in_one_call() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi there")));

    // Built by hand — no config store, no fingerprint written.
    let config = RunnableConfig::builder("assistant")
        .instructions("be concise")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .build();

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    // One call: installs the catalog, runs a fresh turn. No prior install_catalog.
    let phase = runtime.run(&config, "Say hi.", ctx).await.expect("run");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    assert_eq!(messages[0].text_content(), "Say hi.");
    assert_eq!(messages[0].role, Role::User);
    assert!(
        messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text_content() == "hi there"),
        "the assistant reply was committed"
    );
}

#[tokio::test]
async fn run_to_completion_drives_an_ungated_run_without_asking() {
    // No gate → no park → `decide` is never called; the run reaches a terminal
    // phase in one shot. (The gated park→resume path is covered by the coding-agent
    // example's tests.)
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("done")));
    let config = RunnableConfig::builder("assistant")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .build();

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    let phase = runtime
        .run_to_completion(&config, "thread-1", "hi", ctx, |_| {
            unreachable!("an ungated run never parks")
        })
        .await
        .expect("run to completion");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content() == "done"),
        "the reply was committed"
    );
}
