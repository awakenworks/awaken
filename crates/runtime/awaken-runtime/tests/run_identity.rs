//! A caller-supplied Run id changes identity, not Runtime behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_runtime::Runtime;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_store_inmem::MemoryCommitCoordinator;

struct FixedModel;

struct CountingModel(AtomicUsize);

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

#[async_trait::async_trait]
impl LlmExecutor for CountingModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
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

#[tokio::test]
async fn explicit_terminal_identity_cannot_move_to_another_thread() {
    // Cause/effect graph: C1 a stable Run id is absent or already terminal;
    // C2 the requested Thread matches committed ownership. E1 execute once;
    // E2 exact replay returns committed truth; E3 foreign Thread fails without
    // inference. Decision table: R1 !C1 -> E1; R2 C1 && C2 -> E2;
    // R3 C1 && !C2 -> E3. The contract projection owns C1/C2 for Runtime and Host.
    let store = Arc::new(MemoryCommitCoordinator::new());
    let model = Arc::new(CountingModel(AtomicUsize::new(0)));
    let runtime = Runtime::new().with_llm(model.clone());
    let run_id = RunId("thread-bound-run".into());

    let first = runtime
        .run_to_completion_with_id(
            &config(),
            run_id.clone(),
            "thread-a",
            "work",
            context(&store),
            |_| ResumeResult::allow(),
        )
        .await
        .expect("R1/E1");
    assert_eq!(first, RunState::Ended(EndCause::NaturalEnd));

    let replay = runtime
        .run_to_completion_with_id(
            &config(),
            run_id.clone(),
            "thread-a",
            "ignored replay input",
            context(&store),
            |_| ResumeResult::allow(),
        )
        .await
        .expect("R2/E2");
    assert_eq!(replay, first, "R2/E2");

    let error = runtime
        .run_to_completion_with_id(
            &config(),
            run_id,
            "thread-b",
            "must not execute",
            context(&store),
            |_| ResumeResult::allow(),
        )
        .await
        .expect_err("R3/E3");
    assert!(error.to_string().contains("belongs to another Thread"));
    assert_eq!(model.0.load(Ordering::SeqCst), 1, "R3/E3");
}
