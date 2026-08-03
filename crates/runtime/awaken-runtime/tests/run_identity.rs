//! A caller-supplied Run id changes identity, not Runtime behavior.

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_runtime::Runtime;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_store_inmem::MemoryCommitCoordinator;

struct FixedModel;

#[async_trait::async_trait]
impl LlmExecutor for FixedModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("same result"),
            usage: None,
            stop_reason: None,
        })
    }
}

fn config() -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder("worker")
        .model(ModelBinding::new("test", "fixed", "local"))
        .max_steps(4)
        .build()
}

fn context(store: &Arc<MemoryCommitCoordinator>) -> RuntimeRunContext {
    RuntimeRunContext::new()
        .with_commit(store.clone())
        .with_reader(store.clone())
}

#[tokio::test]
async fn explicit_identity_preserves_the_ordinary_run_lifecycle() {
    let direct_store = Arc::new(MemoryCommitCoordinator::new());
    let direct = Runtime::new().with_llm(Arc::new(FixedModel));
    let direct_state = direct
        .run_to_completion(
            &config(),
            "direct-thread",
            "work",
            context(&direct_store),
            |_| ResumeResult::allow(),
        )
        .await
        .expect("direct Run");

    let explicit_store = Arc::new(MemoryCommitCoordinator::new());
    let explicit = Runtime::new().with_llm(Arc::new(FixedModel));
    let explicit_id = RunId("stable-explicit-run".into());
    let explicit_state = explicit
        .run_to_completion_with_id(
            &config(),
            explicit_id.clone(),
            "explicit-thread",
            "work",
            context(&explicit_store),
            |_| ResumeResult::allow(),
        )
        .await
        .expect("explicit Run");

    assert_eq!(direct_state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(explicit_state, direct_state);

    let direct_truth = direct_store.committed();
    let explicit_truth = explicit_store.committed();
    let direct_messages: Vec<_> = direct_truth
        .messages
        .iter()
        .map(|message| (message.role, message.text_content()))
        .collect();
    let explicit_messages: Vec<_> = explicit_truth
        .messages
        .iter()
        .map(|message| (message.role, message.text_content()))
        .collect();
    assert_eq!(explicit_messages, direct_messages);
    assert_eq!(explicit_truth.run_facts.len(), direct_truth.run_facts.len());
    assert!(
        explicit_truth
            .run_facts
            .iter()
            .all(|fact| fact.run_id == explicit_id),
        "every fact uses the caller-supplied first-class identity"
    );
}
