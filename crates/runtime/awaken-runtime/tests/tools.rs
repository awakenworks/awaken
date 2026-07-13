//! A gated tool call runs only when the gate allows; a denied call never
//! executes even though the tool is registered and visible (G9/G21).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::event::Kind as StreamKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolExecutor, ToolOutput};

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

struct ConstGate(GateOutcome);

#[async_trait::async_trait]
impl ToolGateHook for ConstGate {
    async fn gate(
        &self,
        _ctx: &PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        self.0.clone()
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
        .expect("catalog installs");
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
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_identity_ref: "provider-1".to_string(),
                    model_ref: "model-1".to_string(),
                    backend_ref: "backend-1".to_string(),
                },
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
        trace: Default::default(),
    }
}

#[tokio::test]
async fn allowed_tool_call_executes_and_feeds_result_back() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
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
async fn denied_tool_call_never_executes_even_though_visible() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Block {
            reason: "not allowed".to_string(),
        })));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
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
async fn ask_decision_parks_the_run_in_waiting() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool {
            ran: Arc::new(AtomicUsize::new(0)),
        }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Suspend {
            ticket_id: "ticket-1".to_string(),
        })));
    install(&runtime);

    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(outcome, Phase::Waiting);
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
            .any(|m| matches!(m.role, awaken_runtime_contract::llm::ChatRole::Tool))
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

#[tokio::test]
async fn a_wired_tool_executor_replaces_the_in_process_path() {
    // The echo tool is registered and visible, but a run that wires a
    // `ToolExecutor` must route every call through it and never touch the
    // in-process registry (ADR-0044 D1).
    let local_ran = Arc::new(AtomicUsize::new(0));
    let remote_ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText::new()))
        .with_tool(Arc::new(EchoTool {
            ran: local_ran.clone(),
        }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_tool_executor(Arc::new(RecordingExecutor {
            ran: remote_ran.clone(),
        }));

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
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
async fn unknown_tool_yields_a_model_visible_error_result() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsTool("ghost")))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

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
    install(&runtime);

    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
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
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("invalid tool arguments")),
        "invalid arguments must surface as a model-visible error, not abort the run"
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
    install(&runtime);

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
    install(&runtime);

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

#[tokio::test]
async fn a_loop_that_never_ends_naturally_terminates_on_the_step_ceiling() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(AlwaysToolCall))
        .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
        .with_gate(Arc::new(ConstGate(GateOutcome::Allow)));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    // The run ends on the step-ceiling guard, recorded as a single MaxSteps
    // authority — not mislabelled NaturalEnd, and carrying no fault.
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::MaxSteps));
    assert_eq!(
        commit.committed().latest_run.unwrap().phase,
        Phase::Ended(EndCause::MaxSteps)
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
    install(&runtime);

    let context = RuntimeRunContext::new();
    // The loop never ends naturally; it must stop at exactly the configured
    // ceiling (3), not the previously hard-coded 16.
    let outcome = runtime
        .execute(activation_with_steps(3), context)
        .await
        .expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::MaxSteps));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        3,
        "the loop honors the configured step ceiling"
    );
}

/// First turn interleaves a text block and a tool-call block; the second ends.
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
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the interleaved tool call runs"
    );

    // The first assistant turn committed BOTH a text block and a tool-use block,
    // in one message — text and a tool request interleaved.
    let committed = commit.committed();
    let first = committed
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("an assistant turn is committed");
    assert!(
        first
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { .. })),
        "the turn keeps its text block"
    );
    assert!(
        first
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
        "the turn keeps its tool-use block"
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
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    // The tool call surfaced on the live stream as the turn produced it, with
    // its tool id and arguments.
    let streamed: Vec<_> = sink
        .events()
        .into_iter()
        .filter_map(|e| match e.kind {
            StreamKind::ToolCall {
                tool_id, arguments, ..
            } => Some((tool_id, arguments)),
            _ => None,
        })
        .collect();
    assert_eq!(streamed.len(), 1);
    assert_eq!(streamed[0].0, "echo");
    assert_eq!(streamed[0].1, serde_json::json!({"text": "ping"}));
}
