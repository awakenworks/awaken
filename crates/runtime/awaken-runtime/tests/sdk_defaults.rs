//! Public behavior matrix: constructors, explicit continuation limits, admission.

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_runtime::{ConfigurationError, Runtime};
use awaken_runtime_contract::diagnostic::RuntimeErrorKind;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, StopReason,
};
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolExecutionTarget, ToolOutput};
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
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
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
async fn duplicate_configuration_refuses_every_continuation_before_state_or_effects() {
    use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
    use awaken_runtime_contract::{
        activation::RunActivation,
        resolved::CatalogFingerprint,
        resume::{ResumeCommand, ResumeResult},
        snapshot::ExecutableAgentSnapshotId,
    };
    // Cause/effect matrix: duplicate/unique registry × native resume/owned
    // resume/owned recovery. Invalid registry -> same duplicate_tool diagnostic
    // before ticket/history admission, no commit/tool/model. Unique registry ->
    // ordinary missing-ticket/running-history refusal, never duplicate_tool.
    for duplicate in [false, true] {
        let mut runtime = Runtime::new().with_tool(Arc::new(NeverCalledTool));
        if duplicate {
            runtime = runtime.with_tool(Arc::new(NeverCalledTool));
        }
        assert_eq!(
            awaken_runtime::validate_configuration(&runtime).is_err(),
            duplicate
        );
        let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let command = ResumeCommand {
            correlation_id: "c".into(),
            run_id: RunId("run".into()),
            thread_id: ThreadId("thread".into()),
            snapshot_id: ExecutableAgentSnapshotId("s".into()),
            catalog_fingerprint: CatalogFingerprint("f".into()),
            result: ResumeResult::allow(),
            operation_id: None,
            context_messages: vec![],
            now_ms: 0,
        };
        let context = || RuntimeRunContext::new().with_store(store.clone());
        let errors = [
            runtime
                .resume(command.clone(), store.as_ref(), context())
                .await
                .unwrap_err(),
            runtime
                .resume_tool_batch(command, store.as_ref(), context())
                .await
                .unwrap_err(),
            runtime
                .recover_tool_batch(
                    RunActivation::new(
                        RunId("run".into()),
                        ThreadId("thread".into()),
                        snapshot(),
                        vec![],
                    ),
                    context(),
                )
                .await
                .unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.code() == "duplicate_tool", duplicate, "{error}");
        }
        assert_eq!(store.commit_count(), 0);
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
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(ToolDescriptor::pinned(
            "demo",
            "missing",
            "Missing",
            serde_json::json!({"type":"object"}),
        ))
        .build();
    let clock = awaken_time::system_clock();
    let runtime = Runtime::new().with_clock(clock.clone());
    assert_eq!(
        runtime
            .run(&config, "hello", RuntimeRunContext::new())
            .await
            .unwrap_err()
            .code(),
        "missing_tool"
    );
    let store = Arc::new(MemoryCommitCoordinator::new(clock));
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

struct SandboxTool;
#[async_trait::async_trait]
impl RawTool for SandboxTool {
    fn id(&self) -> &str {
        "sandbox"
    }
    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Sandbox
    }
    async fn invoke(&self, _: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("a missing sandbox executor must fail before effects")
    }
}

#[tokio::test]
async fn missing_provider_sandbox_and_writer_fail_before_admission() {
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let runtime = Runtime::new();
    let error = runtime
        .run_with_outcome(
            &snapshot(),
            "hello",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "missing_model_provider");
    let error = runtime
        .conversation(
            &snapshot(),
            "writer",
            RuntimeRunContext::new().with_reader(store.clone()),
        )
        .err()
        .unwrap();
    assert_eq!(error.code(), "missing_commit_coordinator");

    let model = Arc::new(TruncatedModel {
        calls: AtomicUsize::new(0),
        always_truncated: false,
    });
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_tool(Arc::new(SandboxTool));
    let agent = ExecutableAgentSnapshot::builder("sandbox")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(ToolDescriptor::pinned(
            "demo",
            "sandbox",
            "Sandbox",
            serde_json::json!({"type":"object"}),
        ))
        .build();
    let error = runtime
        .run_with_outcome(
            &agent,
            "hello",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "missing_sandbox_executor");
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.commit_count(), 0);
}

struct UnavailableWriter;
#[async_trait::async_trait]
impl awaken_agent_contract::thread::commit::coordinator::Coordinator for UnavailableWriter {
    async fn commit(
        &self,
        _: awaken_agent_contract::thread::commit::staged::ThreadCommit,
    ) -> Result<
        awaken_agent_contract::thread::commit::staged::CommitRecord,
        awaken_agent_contract::thread::commit::coordinator::Error,
    > {
        Err(
            awaken_agent_contract::thread::commit::coordinator::Error::Rejected(
                "storage offline".into(),
            ),
        )
    }
}

#[tokio::test]
async fn commit_failure_retains_the_typed_source_without_claiming_an_outcome() {
    let model = Arc::new(TruncatedModel {
        calls: AtomicUsize::new(0),
        always_truncated: false,
    });
    let error = Runtime::new()
        .with_llm(model.clone())
        .run(
            &snapshot(),
            "hello",
            RuntimeRunContext::new().with_commit(Arc::new(UnavailableWriter)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "commit_failed");
    assert_eq!(error.kind(), RuntimeErrorKind::Internal);
    let mut source: &(dyn std::error::Error + 'static) = &error;
    loop {
        if source.is::<awaken_agent_contract::thread::commit::coordinator::Error>() {
            break;
        }
        source = source
            .source()
            .expect("the coordinator's typed rejection must be retained");
    }
    assert!(source.to_string().contains("storage offline"));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}
