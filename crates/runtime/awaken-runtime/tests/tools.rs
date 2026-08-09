//! A gated tool call runs only when the gate allows; a denied call never
//! executes even though the tool is registered and visible (G9/G21).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolError, ToolExecutionTarget, ToolExecutor, ToolOutput, ToolOutputSpiller,
};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink};

/// First inference asks for a tool call; the second ends with text.
struct ToolThenText {
    calls: AtomicUsize,
}

impl ToolThenText {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
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

/// Records whether it actually ran, so denial can be proven.
struct EchoTool {
    ran: Arc<AtomicUsize>,
}

struct MultimodalTool;

struct OperationProbe {
    seen: Arc<Mutex<Option<String>>>,
}

struct SpillProbe {
    fail: bool,
    seen: Arc<Mutex<Vec<(String, String, String)>>>,
}

#[async_trait::async_trait]
impl ToolOutputSpiller for SpillProbe {
    async fn spill(
        &self,
        run_id: &RunId,
        call_id: &str,
        content: String,
    ) -> Result<String, ToolError> {
        self.seen
            .lock()
            .unwrap()
            .push((run_id.0.clone(), call_id.to_string(), content.clone()));
        if self.fail {
            Err(ToolError::Execution("spill unavailable".into()))
        } else {
            Ok(format!("preview: {content}"))
        }
    }
}

#[async_trait::async_trait]
impl RawTool for OperationProbe {
    fn id(&self) -> &str {
        "echo"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        *self.seen.lock().unwrap() = awaken_runtime_contract::tool::current_tool_operation_id();
        Ok(ToolOutput::ok(call.call_id, "ok"))
    }
}

#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(
            call.call_id,
            format!("echoed: {}", call.arguments),
        ))
    }
}

#[async_trait::async_trait]
impl RawTool for MultimodalTool {
    fn id(&self) -> &str {
        "echo"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok_blocks(
            call.call_id,
            vec![
                ContentBlock::text("pixels"),
                ContentBlock::image_base64("image/png", "iVBORw0KGgo="),
            ],
        ))
    }
}

struct SandboxEcho(EchoTool);

#[async_trait::async_trait]
impl RawTool for SandboxEcho {
    fn id(&self) -> &str {
        self.0.id()
    }

    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Sandbox
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.0.invoke(call).await
    }
}

struct ConstGate(GateOutcome);

#[async_trait::async_trait]
impl ToolGateHook for ConstGate {
    async fn gate(
        &self,
        _ctx: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        self.0.clone()
    }
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "provider-1".to_string(),
                        model_ref: "model-1".to_string(),
                        backend_ref: "backend-1".to_string(),
                    },
                ),
                tool_descriptors: vec![ToolDescriptor::pinned(
                    "test",
                    "echo",
                    "Echo the text argument back",
                    serde_json::json!({
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"],
                    }),
                )],
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("message-1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("please echo")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

#[tokio::test]
async fn allowed_tool_call_executes_and_feeds_result_back() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "tool must run exactly once");

    let committed = commit.committed();
    let tool_results: Vec<_> = committed
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert!(tool_results[0].text_content().contains("echoed"));
}

#[tokio::test]
async fn native_tool_results_use_the_bound_spiller_and_fail_closed() {
    // Cause-effect graph:
    // C1=Native executor produced a result; C2=text-only; C3=spiller succeeds;
    // C4=spiller fails; C5=result contains image blocks.
    // C1+C2+C3 -> transformed content is the only durable/model-visible result.
    // C1+C2+C4 -> Run errors before a Tool result or completed payload commits.
    // C1+C5 -> bypass text spiller and preserve exact structured blocks.
    //
    // | Rule | Native result | spill | Expected effect |
    // | N1 | text | success | one call with stable run/call; preview committed |
    // | N2 | text | failure | execution error; no Tool-role result committed |
    // | N3 | image | any | spiller untouched; ordered pixels committed |
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool {
            ran: Arc::new(AtomicUsize::new(0)),
        }));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    runtime
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_tool_output_spiller(Arc::new(SpillProbe {
                    fail: false,
                    seen: seen.clone(),
                })),
        )
        .await
        .expect("N1");
    assert_eq!(seen.lock().unwrap().len(), 1, "N1");
    assert_eq!(seen.lock().unwrap()[0].0, "run-1", "N1 stable run id");
    assert_eq!(seen.lock().unwrap()[0].1, "call-1", "N1 stable call id");
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|message| message.role == Role::Tool
                && message.text_content().starts_with("preview: echoed:")),
        "N1"
    );

    let failed_commit = Arc::new(MemoryCommitCoordinator::new());
    let failed = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool {
            ran: Arc::new(AtomicUsize::new(0)),
        }))
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(failed_commit.clone())
                .with_tool_output_spiller(Arc::new(SpillProbe {
                    fail: true,
                    seen: Arc::new(Mutex::new(Vec::new())),
                })),
        )
        .await
        .expect_err("N2");
    assert!(failed.to_string().contains("spill unavailable"), "N2");
    assert!(
        failed_commit
            .committed()
            .messages
            .iter()
            .all(|message| message.role != Role::Tool),
        "N2"
    );

    let multimodal_seen = Arc::new(Mutex::new(Vec::new()));
    let multimodal_commit = Arc::new(MemoryCommitCoordinator::new());
    Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(MultimodalTool))
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(multimodal_commit.clone())
                .with_tool_output_spiller(Arc::new(SpillProbe {
                    fail: true,
                    seen: multimodal_seen.clone(),
                })),
        )
        .await
        .expect("N3");
    assert!(multimodal_seen.lock().unwrap().is_empty(), "N3");
    let image = multimodal_commit
        .committed()
        .messages
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .and_then(|message| match message.content.as_slice() {
            [ContentBlock::ToolResult { content, .. }] => content.get(1).cloned(),
            _ => None,
        });
    assert_eq!(
        image,
        Some(ContentBlock::image_base64("image/png", "iVBORw0KGgo=")),
        "N3"
    );
}

#[tokio::test]
async fn executor_receives_run_and_step_scoped_operation_identity() {
    let seen = Arc::new(Mutex::new(None));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(OperationProbe { seen: seen.clone() }));

    runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");

    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some("tool-batch:run-1:0:call-1")
    );
}

#[tokio::test]
async fn denied_tool_call_never_executes_even_though_visible() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Block {
            reason: "not allowed".to_string(),
        })));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "a registered/visible tool must NOT run without an allow decision"
    );

    let committed = commit.committed();
    let blocked = committed
        .messages
        .iter()
        .any(|m| m.role == Role::Tool && m.text_content().contains("blocked"));
    assert!(blocked, "a blocked result must be staged for the model");
}

#[tokio::test]
async fn ask_decision_puts_the_run_in_awaiting() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool {
            ran: Arc::new(AtomicUsize::new(0)),
        }))
        .with_gate(Arc::new(ConstGate(GateOutcome::RequireConfirmation {
            correlation_id: "ticket-1".to_string(),
        })));

    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Awaiting);
}

/// A model that requests a specific tool id once, then ends with text.
struct CallsTool(&'static str);

#[async_trait::async_trait]
impl LlmExecutor for CallsTool {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        // End once a tool result (a Tool-role message) is in the transcript.
        let answered = request
            .messages
            .iter()
            .any(|m| matches!(m.role, awaken_agent_contract::agent::message::Role::Tool))
            || request.messages.len() > 2;
        let output = if answered {
            AssistantOutput::text("done".to_string())
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: self.0.to_string(),
                arguments: serde_json::json!({}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A `ToolExecutor` that records it ran and returns a canned output, standing in
/// for a remote hand (ADR-0044 D1). Placement is invisible to the kernel.
struct RecordingExecutor {
    ran: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolExecutor for RecordingExecutor {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(&call.call_id, "from remote executor"))
    }
}

// Per-tool placement decision table:
// registered Brain tool -> local invocation regardless of executor;
// registered Sandbox tool + executor -> executor only;
// registered Sandbox tool without executor -> model-visible fail-closed result,
// never local fallback; unknown id -> model-visible unknown-tool result.
#[tokio::test]
async fn sandbox_tool_uses_the_wired_executor() {
    // The echo tool is registered and visible, but a run that wires a
    // `ToolExecutor` must route every call through it and never touch the
    // in-process registry (ADR-0044 D1).
    let local_ran = Arc::new(AtomicUsize::new(0));
    let remote_ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(SandboxEcho(EchoTool {
            ran: local_ran.clone(),
        })))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_tool_executor(Arc::new(RecordingExecutor {
            ran: remote_ran.clone(),
        }));

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        local_ran.load(Ordering::SeqCst),
        0,
        "the in-process tool must NOT run when an executor is wired"
    );
    assert_eq!(
        remote_ran.load(Ordering::SeqCst),
        1,
        "the wired executor handled the call"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("from remote executor")),
        "the executor's output is committed as the tool result"
    );
}

#[tokio::test]
async fn brain_tool_bypasses_the_wired_sandbox_executor() {
    let brain_ran = Arc::new(AtomicUsize::new(0));
    let sandbox_ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool {
            ran: brain_ran.clone(),
        }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));
    let context = RuntimeRunContext::new().with_tool_executor(Arc::new(RecordingExecutor {
        ran: sandbox_ran.clone(),
    }));

    runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(brain_ran.load(Ordering::SeqCst), 1);
    assert_eq!(sandbox_ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sandbox_tool_without_executor_fails_closed_instead_of_running_in_brain() {
    let brain_ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(SandboxEcho(EchoTool {
            ran: brain_ran.clone(),
        })))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    runtime
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("run records model-visible tool error");
    assert_eq!(brain_ran.load(Ordering::SeqCst), 0);
    assert!(commit.committed().messages.iter().any(|message| {
        message.role == Role::Tool
            && message
                .text_content()
                .contains("sandbox executor unavailable")
    }));
}

#[tokio::test]
async fn unknown_tool_yields_a_model_visible_error_result() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsTool("ghost")))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("unknown tool"))
    );
}

#[tokio::test]
async fn without_a_gate_an_authorized_tool_runs() {
    let ran = Arc::new(AtomicUsize::new(0));
    // No `.with_gate(...)`: the default is Allow, so the tool executes.
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }));

    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

/// A tool that validates its arguments against the declared schema and rejects
/// missing fields with `InvalidArguments` (the serde-validation path, A3).
struct ValidatingEcho;

#[async_trait::async_trait]
impl RawTool for ValidatingEcho {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let text = call
            .arguments
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing 'text'".to_string()))?;
        Ok(ToolOutput::ok(call.call_id, format!("echoed: {text}")))
    }
}

#[tokio::test]
async fn invalid_arguments_yield_a_model_visible_error_result() {
    // `CallsTool` invokes `echo` with empty arguments, which fail validation.
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsTool("echo")))
        .with_tool(Arc::new(ValidatingEcho))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("invalid tool arguments")),
        "invalid arguments must surface as a model-visible error, not abort the run"
    );
}

/// A tool whose invocation fails at runtime — not unknown, not bad-args, but a genuine
/// `ToolError::Execution` (the class a real tool raises when its work fails). It must
/// reach the model as an error result and let the run continue, exactly like the
/// unknown/invalid-args paths (the engine's `Err → ToolOutput::error` seam, A2).
struct FailingExecTool;

#[async_trait::async_trait]
impl RawTool for FailingExecTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Execution("exec boom".to_string()))
    }
}

#[tokio::test]
async fn an_execution_error_yields_a_model_visible_error_result_and_continues() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsTool("echo")))
        .with_tool(Arc::new(FailingExecTool))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("exec boom")),
        "an Execution error must surface as a model-visible tool error, not abort the run"
    );
}

/// A tool that PANICS (an impl bug / a stray `.unwrap()`), not one that returns an
/// error. A runtime that hosts arbitrary third-party tools (MCP/plugin/skill) must
/// isolate that panic to a model-visible error and keep the run alive — never let one
/// tool crash the whole agent run.
struct PanickingTool;

#[async_trait::async_trait]
impl RawTool for PanickingTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("tool impl bug: a stray unwrap");
    }
}

#[tokio::test]
async fn a_panicking_tool_is_isolated_to_a_model_visible_error_and_the_run_continues() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsTool("echo")))
        .with_tool(Arc::new(PanickingTool))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().to_lowercase().contains("panic")),
        "a panicking tool must surface as a model-visible error, not crash the run"
    );
}

/// Captures the tool schemas the model receives so we can assert the real
/// descriptor schema (not a placeholder) reaches inference (A2).
struct CaptureTools {
    seen: Arc<std::sync::Mutex<Vec<awaken_runtime_contract::resolved::ToolDescriptor>>>,
}

#[async_trait::async_trait]
impl LlmExecutor for CaptureTools {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        *self.seen.lock().unwrap() = request.tools.clone();
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn real_tool_schema_is_projected_to_the_model() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = Runtime::new().with_llm(Arc::new(CaptureTools { seen: seen.clone() }));

    runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");

    let tools = seen.lock().unwrap().clone();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].id, "echo");
    assert_eq!(tools[0].description, "Echo the text argument back");
    assert_eq!(tools[0].parameters["properties"]["text"]["type"], "string");
}

#[tokio::test]
async fn gate_set_result_skips_execution_and_stages_supplied_result() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::SetResult(
            awaken_runtime_contract::tool::ToolOutput::ok("call-1", "injected"),
        ))));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "set-result must skip execution"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content() == "injected")
    );
}

/// Always asks for a tool call and never ends with text, so the loop runs to
/// its step ceiling.
struct AlwaysToolCall;

#[async_trait::async_trait]
impl LlmExecutor for AlwaysToolCall {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-loop".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "again"}),
            }]),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Uses tools while they are offered, then obeys the runtime's reserved final
/// request and returns a natural response.
struct ToolUntilFinalRequest {
    calls: AtomicUsize,
    saw_final_request: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ToolUntilFinalRequest {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if request.tools.is_empty() {
            self.saw_final_request.fetch_add(1, Ordering::SeqCst);
            assert!(request.messages.iter().any(|message| {
                message.role == Role::System
                    && extract_text(&message.content).contains("final inference step")
            }));
            AssistantOutput::text("final evidence")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("call-final-{n}"),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "collect"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn a_loop_that_never_ends_naturally_terminates_on_the_step_ceiling() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(AlwaysToolCall))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    // The run ends on the step-ceiling guard, recorded as a single MaxSteps
    // authority — not mislabelled NaturalEnd, and carrying no fault.
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::MaxSteps));
    assert_eq!(
        commit.committed().latest_run.unwrap().state,
        RunState::Ended(EndCause::MaxSteps)
    );
    assert!(
        ran.load(Ordering::SeqCst) >= 1,
        "the tool did run each step"
    );
}

/// Same fixture as `activation`, but with the agent's step ceiling overridden.
fn activation_with_steps(max_steps: usize) -> RunActivation {
    let mut activation = activation();
    activation.snapshot.resolved_spec.max_steps = max_steps;
    activation
}

#[tokio::test]
async fn the_configured_step_ceiling_is_honored() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(AlwaysToolCall))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let context = RuntimeRunContext::new();
    // The loop never ends naturally; it must stop at exactly the configured
    // ceiling (3), not the previously hard-coded 16.
    let outcome = runtime
        .execute(activation_with_steps(3), context)
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::MaxSteps));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        2,
        "the final inference allowance never executes another tool"
    );
}

#[tokio::test]
async fn the_last_step_is_reserved_for_a_natural_final_response() {
    let ran = Arc::new(AtomicUsize::new(0));
    let llm = Arc::new(ToolUntilFinalRequest {
        calls: AtomicUsize::new(0),
        saw_final_request: AtomicUsize::new(0),
    });
    let runtime = Runtime::new()
        .with_llm(llm.clone())
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let outcome = runtime
        .execute(activation_with_steps(3), RuntimeRunContext::new())
        .await
        .expect("runs");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(llm.calls.load(Ordering::SeqCst), 3);
    assert_eq!(llm.saw_final_request.load(Ordering::SeqCst), 1);
    assert_eq!(ran.load(Ordering::SeqCst), 2);
}

/// First response interleaves a text block and a tool-call block; the second ends.
struct InterleavedThenText {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for InterleavedThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_blocks(vec![
                ContentBlock::text("let me check that"),
                ContentBlock::tool_use("call-1", "echo", serde_json::json!({"text": "ping"})),
            ])
        } else {
            AssistantOutput::text("all done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn an_assistant_turn_interleaves_text_and_a_tool_call() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(InterleavedThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the interleaved tool call runs"
    );

    // The first assistant response committed BOTH a text block and a tool-use block,
    // in one message — text and a tool request interleaved.
    let committed = commit.committed();
    let first = committed
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant response is committed");
    assert!(
        first
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { .. })),
        "the response keeps its text block"
    );
    assert!(
        first
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
        "the response keeps its tool-use block"
    );
    assert_eq!(first.text_content(), "let me check that");
}

#[tokio::test]
async fn a_tool_call_streams_to_the_live_sink() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(InterleavedThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    // The tool call surfaced on the live stream as the run produced it, as
    // `ToolCallDelta` fragments whose args concatenate to the full input JSON.
    let streamed: Vec<_> = sink
        .events()
        .into_iter()
        .filter_map(|e| match e.kind {
            AgentEvent::Delta(Delta::ToolCallDelta {
                name, args_delta, ..
            }) => Some((name, args_delta)),
            _ => None,
        })
        .collect();
    assert!(!streamed.is_empty());
    assert!(streamed.iter().all(|(id, _)| id == "echo"));
    let joined: String = streamed.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
        serde_json::json!({"text": "ping"})
    );
}
