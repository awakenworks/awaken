//! Public behavior matrix: constructors, explicit continuation limits, admission.

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_runtime::{ConfigurationError, Runtime};
use awaken_runtime_contract::diagnostic::RuntimeErrorKind;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, StopReason,
};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_store_inmem::MemoryCommitCoordinator;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct TruncatedModel {
    calls: AtomicUsize,
    always_truncated: bool,
}

#[async_trait::async_trait]
impl LlmExecutor for TruncatedModel {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let truncated = self.calls.fetch_add(1, Ordering::SeqCst) == 0 || self.always_truncated;
        Ok(ChatResponse {
            output: AssistantOutput::text(if truncated { "partial" } else { "complete" }),
            usage: None,
            stop_reason: Some(if truncated {
                StopReason::MaxTokens
            } else {
                StopReason::NaturalEnd
            }),
        })
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder("defaults")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .max_steps(8)
        .build()
}

async fn run(runtime: Runtime, always_truncated: bool) -> (usize, RunState, Vec<String>) {
    let model = Arc::new(TruncatedModel {
        calls: AtomicUsize::new(0),
        always_truncated,
    });
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let state = runtime
        .with_llm(model.clone())
        .run(
            &snapshot(),
            "hello",
            RuntimeRunContext::new().with_commit(store.clone()),
        )
        .await
        .unwrap();
    let text = store
        .committed()
        .messages
        .iter()
        .map(|message| message.text_content())
        .collect();
    (model.calls.load(Ordering::SeqCst), state, text)
}

#[tokio::test]
async fn new_and_default_have_the_same_committed_behavior() {
    // Constructor choice must not alter model calls, terminal cause or transcript.
    for always_truncated in [false, true] {
        let new = run(Runtime::new(), always_truncated).await;
        let default = run(Runtime::default(), always_truncated).await;
        assert_eq!(new, default);
        assert_eq!(new.0, if always_truncated { 3 } else { 2 });
        if !always_truncated {
            assert_eq!(new.1, RunState::Ended(EndCause::NaturalEnd));
        } else {
            assert!(matches!(new.1, RunState::Ended(EndCause::Error(_))));
        }
    }
}

#[tokio::test]
async fn explicit_continuation_limits_remain_observable() {
    for retries in [0, 2] {
        let (calls, state, text) = run(
            Runtime::default().with_max_continuation_retries(retries),
            true,
        )
        .await;
        assert_eq!(calls, retries + 1);
        assert!(matches!(state, RunState::Ended(EndCause::Error(_))));
        assert!(text.iter().any(|value| value.contains("partial")));
    }
}

struct NeverCalledTool;
#[async_trait::async_trait]
impl RawTool for NeverCalledTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, _: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("invalid configuration must not enter a tool")
    }
}

#[tokio::test]
async fn duplicate_registration_is_reported_before_inference_or_commit() {
    let runtime = Runtime::new()
        .with_tool(Arc::new(NeverCalledTool))
        .with_tool(Arc::new(NeverCalledTool));
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let error = runtime
        .run(
            &snapshot(),
            "hello",
            RuntimeRunContext::new().with_commit(store.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), RuntimeErrorKind::InvalidConfiguration);
    assert_eq!(error.code(), "duplicate_tool");
    assert!(std::error::Error::source(&error).is_some());
    assert!(store.committed().messages.is_empty());
    assert!(
        matches!(Runtime::new().try_with_tool(Arc::new(NeverCalledTool)).unwrap().try_with_tool(Arc::new(NeverCalledTool)), Err(ConfigurationError::DuplicateTool { id }) if id == "echo")
    );
}

#[tokio::test]
async fn missing_tool_and_reader_are_actionable_before_effects() {
    let config = ExecutableAgentSnapshot::builder("missing-tool")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .tool(ToolDescriptor::pinned(
            "demo",
            "missing",
            "Missing",
            serde_json::json!({"type":"object"}),
        ))
        .build();
    let runtime = Runtime::new();
    assert_eq!(
        runtime
            .validate_for(&config, &RuntimeRunContext::new())
            .unwrap_err()
            .code(),
        "missing_tool"
    );
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let error = runtime
        .run_to_completion(
            &snapshot(),
            "thread",
            "hello",
            RuntimeRunContext::new().with_commit(store.clone()),
            |_| unreachable!(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "missing_history_reader");
    assert!(store.committed().messages.is_empty());
}
