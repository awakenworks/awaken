//! ScheduledAction (ADR-0020): a gate `Schedule` parks the run on a committed
//! ScheduledAction ticket; `perform_scheduled_action` runs the deferred action
//! and commits the resumed outcome; a result for a run not parked on a scheduled
//! action fails closed; and cancel makes a late perform fail closed
//! (RS-SCH-001/004, RS-CTRL-001).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingReason;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

struct ToolThenText {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
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

/// Defers the tool call as a ScheduledAction instead of running it inline.
struct ScheduleGate;
#[async_trait::async_trait]
impl ToolGateHook for ScheduleGate {
    async fn gate(&self, _c: &PermissionContext) -> GateOutcome {
        GateOutcome::Schedule {
            correlation_id: "sched-1".to_string(),
        }
    }
}

struct SuspendGate;
#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(&self, _c: &PermissionContext) -> GateOutcome {
        GateOutcome::Suspend {
            ticket_id: "perm-1".to_string(),
        }
    }
}

const FP: &str = "catalog-a";
const SNAP: &str = "snapshot-1";

fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_instance_ref: "p".to_string(),
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
        },
        fingerprint: fp,
    }
}

fn runtime(ran: Arc<AtomicUsize>, gate: Arc<dyn ToolGateHook>) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool { ran }))
        .with_gate(gate);
    let fp = CatalogFingerprint(FP.to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fp.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fp,
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
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    }
}

fn context(commit: &Arc<MemoryCommitCoordinator>) -> RuntimeRunContext {
    RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone())
}

#[tokio::test]
async fn scheduled_action_commits_then_perform_runs_it() {
    // RS-SCH-001.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // The run defers the call: it parks on a committed ScheduledAction ticket.
    let phase = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs");
    assert_eq!(phase, Phase::Waiting);
    let ticket = commit
        .waiting_for(&RunId("run-1".to_string()))
        .expect("a scheduled action is committed");
    assert_eq!(ticket.reason, WaitingReason::ScheduledAction);
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the action is deferred, not run"
    );

    // Performing the committed action runs it and the run completes.
    let phase = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect("perform");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the deferred action ran once"
    );
    assert!(
        commit.waiting_for(&RunId("run-1".to_string())).is_none(),
        "the ticket is cleared once performed"
    );
}

#[tokio::test]
async fn perform_on_a_non_scheduled_run_fails_closed() {
    // RS-SCH-004: a result for a run not parked on a scheduled action is rejected.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran, Arc::new(SuspendGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // No such run: not waiting.
    let err = runtime
        .perform_scheduled_action(
            &RunId("ghost".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("no run");
    assert!(err.to_string().contains("not waiting"));

    // A run parked on a tool-permission ticket is not a scheduled action.
    runtime
        .execute(activation(), context(&commit))
        .await
        .expect("parks on permission");
    let err = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("not a scheduled action");
    assert!(err.to_string().contains("not parked on a scheduled action"));
}

#[tokio::test]
async fn cancel_makes_a_late_scheduled_perform_fail_closed() {
    // RS-CTRL-001.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    runtime
        .execute(activation(), context(&commit))
        .await
        .expect("parks on a scheduled action");

    // Cancel commits a terminal Cancelled and clears the ticket.
    let phase = runtime
        .cancel_run(
            RunId("run-1".to_string()),
            ThreadId("thread-1".to_string()),
            context(&commit),
        )
        .await
        .expect("cancel");
    assert_eq!(phase, Phase::Ended(EndCause::Cancelled));

    // A late perform is rejected without running the action or mutating facts.
    let err = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("late perform rejected");
    assert!(err.to_string().contains("not waiting"));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}
