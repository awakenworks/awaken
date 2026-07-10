//! Structure-only metrics (#2): the engine records an inference metric at the
//! `chat` chokepoint for every model call — with the model id and an outcome
//! class (`ok` or the error code), never content — and a tool metric per tool
//! execution. A `SpyRecorder` captures what the engine emits.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{LlmRetryPolicy, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error as LlmError, LlmExecutor, StopReason,
    ToolCall,
};
use awaken_runtime_contract::metrics::{InferenceMetric, MetricsRecorder};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

/// Captures every metric the engine emits, so a test can assert on the labels.
#[derive(Default)]
struct SpyRecorder {
    inferences: Mutex<Vec<(String, String, Option<u64>, Option<u64>)>>,
    tools: Mutex<Vec<(String, String)>>,
}

impl MetricsRecorder for SpyRecorder {
    fn record_inference(&self, m: InferenceMetric<'_>) {
        self.inferences.lock().unwrap().push((
            m.model.to_string(),
            m.outcome.to_string(),
            m.input_tokens,
            m.output_tokens,
        ));
    }
    fn record_tool(&self, tool: &str, outcome: &str, _duration: std::time::Duration) {
        self.tools
            .lock()
            .unwrap()
            .push((tool.to_string(), outcome.to_string()));
    }
}

/// One text turn with reported usage.
struct OkLlm;
#[async_trait::async_trait]
impl LlmExecutor for OkLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: Some(awaken_runtime_contract::llm::TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            stop_reason: Some(StopReason::EndTurn),
        })
    }
}

/// A permanent authentication failure.
struct UnauthorizedLlm;
#[async_trait::async_trait]
impl LlmExecutor for UnauthorizedLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Err(LlmError::Unauthorized("bad key".to_string()))
    }
}

fn install(runtime: &Runtime) {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_instance_ref: "p".to_string(),
                    model_ref: "gpt-test".to_string(),
                    backend_ref: "b".to_string(),
                },
                model_candidates: Vec::new(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        trace: Default::default(),
    }
}

fn no_retries() -> LlmRetryPolicy {
    LlmRetryPolicy {
        max_retries: 0,
        backoff_base_ms: 0,
        overloaded_backoff_base_ms: 0,
    }
}

#[tokio::test]
async fn a_successful_turn_records_an_ok_inference_metric_with_model_and_tokens() {
    let spy = Arc::new(SpyRecorder::default());
    let runtime = Runtime::new()
        .with_llm(Arc::new(OkLlm))
        .with_metrics(spy.clone());
    install(&runtime);

    let context = RuntimeRunContext::new().with_commit(Arc::new(MemoryCommitCoordinator::new()));
    runtime.execute(activation(), context).await.expect("runs");

    let inferences = spy.inferences.lock().unwrap();
    assert_eq!(inferences.len(), 1, "one chat call → one metric");
    let (model, outcome, input, output) = &inferences[0];
    assert_eq!(model, "gpt-test");
    assert_eq!(outcome, "ok");
    assert_eq!(*input, Some(11));
    assert_eq!(*output, Some(7));
}

/// One tool call to `echo`, then a closing text turn.
struct ToolThenText {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "ping"}),
            }])
        } else {
            AssistantOutput::text("all done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct EchoTool;
#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "echoed"))
    }
}

#[tokio::test]
async fn a_tool_call_records_a_tool_metric_with_id_and_outcome() {
    let spy = Arc::new(SpyRecorder::default());
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool))
        .with_metrics(spy.clone());
    install(&runtime);

    let context = RuntimeRunContext::new().with_commit(Arc::new(MemoryCommitCoordinator::new()));
    runtime.execute(activation(), context).await.expect("runs");

    // The tool chokepoint recorded exactly one `echo` execution, outcome `ok`.
    let tools = spy.tools.lock().unwrap();
    assert_eq!(&*tools, &[("echo".to_string(), "ok".to_string())]);
    // Two inference calls were metered too (the tool turn, then the text turn).
    assert_eq!(spy.inferences.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_permanent_failure_records_the_error_class_as_the_outcome() {
    let spy = Arc::new(SpyRecorder::default());
    let runtime = Runtime::new()
        .with_llm(Arc::new(UnauthorizedLlm))
        .with_metrics(spy.clone())
        .with_retry_policy(no_retries());
    install(&runtime);

    let context = RuntimeRunContext::new().with_commit(Arc::new(MemoryCommitCoordinator::new()));
    runtime.execute(activation(), context).await.expect("runs");

    let inferences = spy.inferences.lock().unwrap();
    assert_eq!(inferences.len(), 1);
    let (model, outcome, input, output) = &inferences[0];
    assert_eq!(model, "gpt-test");
    // The outcome is the stable classification code, not content.
    assert_eq!(outcome, "unauthorized");
    // A failure reports no usage.
    assert_eq!(*input, None);
    assert_eq!(*output, None);
}
