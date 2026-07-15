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
            stop_reason: None,
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
async fn per_run_model_executor_override_wins_over_the_runtime_default() {
    // ADR-0004: a run whose context carries a `model_executor` (the executor the host
    // resolved from the run's model ref) routes its inference through that executor,
    // NOT the runtime's bound default. This is the provider seam a database-less
    // worker uses to run each run's own configured model.
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("DEFAULT")));
    let config = RunnableConfig::builder("assistant")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .build();

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        // The per-run override: this attempt must use OVERRIDE, not DEFAULT.
        .with_model_executor(Arc::new(TextLlm("OVERRIDE")));

    let phase = runtime.run(&config, "go", ctx).await.expect("run");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    assert!(
        messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text_content() == "OVERRIDE"),
        "the run used the per-run override executor, not the runtime default"
    );
    assert!(
        !messages.iter().any(|m| m.text_content() == "DEFAULT"),
        "the runtime's bound default executor was never invoked"
    );
}

#[tokio::test]
async fn without_an_override_the_run_uses_the_runtime_default() {
    // The override is opt-in: absent it, the runtime's bound executor drives the run
    // exactly as before (no regression for the common single-model path).
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("DEFAULT")));
    let config = RunnableConfig::builder("assistant")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .build();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());

    runtime.run(&config, "go", ctx).await.expect("run");
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content() == "DEFAULT"),
        "the runtime default drives the run when no per-run override is set"
    );
}

#[tokio::test]
async fn with_neither_an_override_nor_a_bound_default_the_run_fails_closed() {
    // The provider seam is fail-closed: a run with no per-run `model_executor` AND a
    // runtime that has no bound default has NO model to reach, so it errors instead of
    // silently doing nothing. This is the E5 corner of the resolve matrix — the case a
    // secretless worker hits when the resolver declines the ref and no host default
    // exists.
    let runtime = Runtime::new(); // no with_llm → no bound default
    let config = RunnableConfig::builder("assistant")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .build();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone()); // no with_model_executor

    let err = runtime
        .run(&config, "go", ctx)
        .await
        .expect_err("a run with no reachable model must fail closed");
    assert!(
        err.to_string().contains("no model provider configured"),
        "unexpected error: {err}"
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
