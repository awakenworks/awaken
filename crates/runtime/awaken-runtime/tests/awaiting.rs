//! A gate Suspend awaits the run on a committed awaiting ticket; a validated
//! resume continues it, and a mismatched or stale resume fails closed (G5/G28).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, replay_latest_state};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

/// Calls `echo` once, then ends with text on the next inference.
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
        Ok(ToolOutput::ok(call.call_id, "echoed: ping"))
    }
}

struct SuspendGate;

#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(
        &self,
        _ctx: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: "ticket-1".to_string(),
        }
    }
}

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";

fn snapshot() -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fingerprint.clone(),
            instructions: String::new(),
            max_steps: 16,
            delegation_limits: Default::default(),
            model_binding: ModelBinding {
                provider_identity_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            tool_descriptors: vec![ToolDescriptor::pinned(
                "test",
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn runtime(ran: Arc<AtomicUsize>) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool { ran }))
        .with_gate(Arc::new(SuspendGate));
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
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
    runtime.register_snapshot(snapshot());
    runtime
}

fn activation() -> RunActivation {
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        delegation_origin: None,
        model_ref_override: None,
    }
}

fn resume_command(result: ResumeResult) -> ResumeCommand {
    ResumeCommand {
        correlation_id: "ticket-1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result,
        now_ms: 0,
    }
}

async fn suspend(commit: &Arc<MemoryCommitCoordinator>, runtime: &Runtime) {
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Awaiting);
}

#[tokio::test]
async fn suspend_commits_ticket_then_allow_resume_executes_and_completes() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    suspend(&commit, &runtime).await;

    // A awaiting ticket and a RunAwaiting event were committed; the tool has not run.
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("an awaiting ticket is committed");
    assert_eq!(ticket.correlation_id, "ticket-1");
    assert_eq!(ticket.thread_id, ThreadId("thread-1".to_string()));
    assert!(
        commit
            .committed()
            .events
            .iter()
            .any(|e| e.kind == EventKind::RunAwaiting)
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "tool must not run while awaiting"
    );

    // An allow decision resumes: the pending tool executes and the run completes.
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .resume(
            resume_command(ResumeResult::Decision {
                allow: true,
                note: None,
            }),
            commit.as_ref(),
            context,
        )
        .await
        .expect("resume runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "allow runs the pending tool once"
    );

    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("echoed"))
    );
    // The ticket is cleared once resumed.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none()
    );

    // The fact log keeps the full progression in order — Running (input),
    // Running (the Requested tool batch committed before execution), Awaiting,
    // Running (approved -> Executing, committed before invocation), Running (the
    // resumed result/batch publication), Ended — and the latest fact is the
    // authority replay derives (ADR-0006 D1).
    let states: Vec<_> = committed
        .run_facts
        .iter()
        .filter(|f| f.run_id == RunId("run-1".to_string()))
        .map(|f| f.state.clone())
        .collect();
    assert_eq!(
        states,
        vec![
            RunState::Running,
            RunState::Running,
            RunState::Awaiting,
            RunState::Running,
            RunState::Running,
            RunState::Ended(EndCause::NaturalEnd)
        ]
    );
    assert_eq!(
        replay_latest_state(&committed, &RunId("run-1".to_string())),
        Some(RunState::Ended(EndCause::NaturalEnd))
    );
}

#[tokio::test]
async fn deny_resume_feeds_a_blocked_result_without_running_the_tool() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .resume(
            resume_command(ResumeResult::Decision {
                allow: false,
                note: Some("nope".to_string()),
            }),
            commit.as_ref(),
            context,
        )
        .await
        .expect("resume runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "deny must not run the tool");
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("blocked"))
    );
    assert!(commit.committed().events.iter().any(|event| {
        event.kind == EventKind::PermissionDecided && event.payload["decision"] == "denied"
    }));
}

#[tokio::test]
async fn resume_with_wrong_fingerprint_fails_closed() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    suspend(&commit, &runtime).await;

    let mut command = resume_command(ResumeResult::Decision {
        allow: true,
        note: None,
    });
    command.catalog_fingerprint = awaken_runtime_contract::CatalogFingerprint("wrong".to_string());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let err = runtime
        .resume(command, commit.as_ref(), context)
        .await
        .expect_err("mismatched fingerprint is rejected");
    assert!(err.to_string().contains("fingerprint"));
    // The run stays awaiting: the ticket is untouched.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_some()
    );
}

#[tokio::test]
async fn second_resume_after_completion_is_not_awaiting() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    runtime
        .resume(
            resume_command(ResumeResult::Decision {
                allow: true,
                note: None,
            }),
            commit.as_ref(),
            context,
        )
        .await
        .expect("first resume completes");

    // The ticket was consumed; a stale second resume fails closed.
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let err = runtime
        .resume(
            resume_command(ResumeResult::Decision {
                allow: true,
                note: None,
            }),
            commit.as_ref(),
            context,
        )
        .await
        .expect_err("a stale resume is rejected");
    assert!(err.to_string().contains("not awaiting"));
}

#[tokio::test]
async fn resume_with_a_client_tool_result_is_used_directly() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .resume(
            resume_command(ResumeResult::ToolResult(ToolOutput::ok(
                "call-1",
                "client-computed",
            ))),
            commit.as_ref(),
            context,
        )
        .await
        .expect("resume runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    // The client's result is fed back verbatim; the host tool never ran.
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content() == "client-computed")
    );
}

#[tokio::test]
async fn permission_wait_rejects_free_form_input() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let error = runtime
        .resume(
            resume_command(ResumeResult::Input("the answer is 42".to_string())),
            commit.as_ref(),
            context,
        )
        .await
        .expect_err("approval is a structured decision, not a chat message");
    assert!(error.to_string().contains("result kind"));
    assert_eq!(
        replay_latest_state(&commit.committed(), &RunId("run-1".to_string())),
        Some(RunState::Awaiting)
    );
}
