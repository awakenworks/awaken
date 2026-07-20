//! ADR-0044 + ADR-0045 end-to-end: a real `Runtime` (the brain) runs its tool
//! calls on a separate hand — first over an in-process pair, then over a Unix
//! socket dialed from a `ConnectionPlan` — and commits the hand's output. The
//! brain's in-process registry is never touched.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_connection_plan::{ChannelFactory, ConnectionPlan, TokioChannelFactory, bind_unix};
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_tool_relay::{HandSession, RemoteToolExecutor, serve_hand};

/// First inference asks for `echo`; the second ends with text.
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

/// A tool that must NEVER run in the brain: it panics if invoked in-process, so
/// the test proves the call went to the hand.
struct BrainSideTrap;

#[async_trait::async_trait]
impl RawTool for BrainSideTrap {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("the brain's in-process tool must not run when a remote hand is wired");
    }
}

/// The hand's real tool: echoes its `text` and records that it ran.
struct HandEcho {
    ran: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RawTool for HandEcho {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        let text = call
            .arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(ToolOutput::ok(call.call_id, format!("hand echoed: {text}")))
    }
}

fn brain() -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(BrainSideTrap));
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
    runtime
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
        delegation_origin: None,
        model_ref_override: None,
    }
}

#[tokio::test]
async fn brain_runs_its_tool_on_an_in_process_hand() {
    let ran = Arc::new(AtomicUsize::new(0));
    let session = HandSession::new([Arc::new(HandEcho { ran: ran.clone() }) as Arc<dyn RawTool>]);

    // The degenerate topology: an in-memory pair. Hand served on a task.
    let (brain_end, hand_end) = awaken_connection_plan::in_process_pair();
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_tool_executor(Arc::new(RemoteToolExecutor::new(brain_end)));

    let outcome = brain().execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the hand ran the tool exactly once"
    );

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("hand echoed: ping")),
        "the hand's output is committed as the brain's tool result"
    );

    drop(hand);
}

#[tokio::test]
async fn brain_runs_its_tool_on_a_unix_socket_hand() {
    let ran = Arc::new(AtomicUsize::new(0));
    let path = std::env::temp_dir()
        .join(format!("awaken-relay-e2e-{}.sock", std::process::id()))
        .to_string_lossy()
        .to_string();

    // Hand: bind a Unix listener from a ConnectionPlan, accept, serve.
    let listen_plan = ConnectionPlan::unix_listen(&path);
    let listener = bind_unix(&listen_plan).expect("bind unix");
    let hand_ran = ran.clone();
    let hand = tokio::spawn(async move {
        let channel = listener.accept().await.expect("accept");
        let session = HandSession::new([Arc::new(HandEcho { ran: hand_ran }) as Arc<dyn RawTool>]);
        let _ = serve_hand(channel, session).await;
    });

    // Brain: dial the hand from a ConnectionPlan and route tool calls to it.
    let brain_channel = TokioChannelFactory
        .connect(&ConnectionPlan::unix_dial(&path))
        .await
        .expect("dial hand");
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_tool_executor(Arc::new(RemoteToolExecutor::new(brain_channel)));

    let outcome = brain().execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("hand echoed: ping"))
    );

    hand.abort();
    let _ = std::fs::remove_file(&path);
}
