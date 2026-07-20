//! ADR-0044 (remote hand) × ADR-0041 (sandbox), end to end: a real `Runtime` (the
//! brain) routes its tool call to a remote hand over a Unix socket, and the hand
//! executes the tool INSIDE a real provisioned sandbox — a process spawned under the
//! provider, its artifact harvested — then the brain commits the sandbox-produced
//! output. This proves the two seams compose: the hand is where tools run, and where
//! the hand runs them is an OS-isolated sandbox.
//!
//! The brain's in-process registry is a trap (panics if touched), so a green run is
//! proof the call went brain → hand → sandbox, not brain → in-process tool.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_connection_plan::{ChannelFactory, ConnectionPlan, TokioChannelFactory, bind_unix};
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
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
use awaken_sandbox_local::LocalProvider;
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

/// A tool that must NEVER run in the brain: it panics if invoked in-process.
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

/// The hand's real tool: it runs `printf` INSIDE a fresh OS sandbox (Workdir tier), so
/// the echoed text is produced by a process the sandbox provider spawned + harvested —
/// not by the hand's own address space.
struct SandboxEcho {
    base: std::path::PathBuf,
    ran: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RawTool for SandboxEcho {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let n = self.ran.fetch_add(1, Ordering::SeqCst);
        let text = call
            .arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let provider = LocalProvider::new(self.base.join(format!("sbx-{n}")));
        let spec = pc::SandboxSpec {
            scope: format!("hand-{n}"),
            isolation: pc::IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: None,
        };
        let run = |e: String| ToolError::Execution(e);
        let sandbox = provider
            .create(&spec)
            .await
            .map_err(|e| run(format!("create: {e:?}")))?;

        // Run the echo as a sandboxed process; pass the arg via an injected env var.
        let mut cmd = pc::Command::new([
            "sh",
            "-c",
            r#"printf 'sandboxed: %s' "$TEXT" > "$AWAKEN_OUTPUTS_DIR/out.txt""#,
        ]);
        cmd.stdio = pc::Stdio::Null;
        cmd.env = vec![pc::EnvVar {
            name: "TEXT".into(),
            value: pc::EnvValue::Inline { value: text },
            visibility: pc::EnvVisibility::Process,
        }];
        let proc = sandbox
            .spawn(cmd)
            .await
            .map_err(|e| run(format!("spawn: {e:?}")))?;
        proc.wait().await.map_err(|e| run(format!("wait: {e:?}")))?;

        let arts = sandbox
            .artifacts()
            .await
            .map_err(|e| run(format!("artifacts: {e:?}")))?;
        let art = arts
            .iter()
            .find(|a| a.path.ends_with("/out.txt"))
            .ok_or_else(|| run("the sandboxed process wrote no artifact".into()))?;
        let bytes = sandbox
            .read_artifact(&art.id)
            .await
            .map_err(|e| run(format!("read: {e:?}")))?;
        sandbox
            .dispose()
            .await
            .map_err(|e| run(format!("dispose: {e:?}")))?;

        Ok(ToolOutput::ok(
            call.call_id,
            String::from_utf8_lossy(&bytes).to_string(),
        ))
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
async fn brain_runs_its_tool_inside_a_sandbox_on_a_remote_hand() {
    let ran = Arc::new(AtomicUsize::new(0));
    let base = std::env::temp_dir().join(format!("awaken-hand-sbx-{}", std::process::id()));
    let path = std::env::temp_dir()
        .join(format!("awaken-hand-sbx-{}.sock", std::process::id()))
        .to_string_lossy()
        .to_string();

    // Hand: bind a Unix listener, accept, serve a session whose only tool is the
    // sandbox-backed echo.
    let listen_plan = ConnectionPlan::unix_listen(&path);
    let listener = bind_unix(&listen_plan).expect("bind unix");
    let hand_ran = ran.clone();
    let hand_base = base.clone();
    let hand = tokio::spawn(async move {
        let channel = listener.accept().await.expect("accept");
        let session = HandSession::new([Arc::new(SandboxEcho {
            base: hand_base,
            ran: hand_ran,
        }) as Arc<dyn RawTool>]);
        let _ = serve_hand(channel, session).await;
    });

    // Brain: dial the hand and route tool calls to it.
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
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the hand ran the sandboxed tool exactly once"
    );

    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("sandboxed: ping")),
        "the brain commits the output produced INSIDE the hand's sandbox"
    );

    hand.abort();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&base);
}
