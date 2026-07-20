//! ScheduledAction (ADR-0020): a gate `Schedule` awaits the run on a committed
//! ScheduledAction ticket; `perform_scheduled_action` runs the deferred action
//! and commits the resumed outcome; a result for a run not awaiting on a scheduled
//! action fails closed; and cancel makes a late perform fail closed
//! (RS-SCH-001/004, RS-CTRL-001).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
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

/// Defers the tool call as a ScheduledAction instead of running it inline.
struct ScheduleGate;
#[async_trait::async_trait]
impl ToolGateHook for ScheduleGate {
    async fn gate(
        &self,
        _c: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Schedule {
            correlation_id: "sched-1".to_string(),
            action_kind: None,
        }
    }
}

/// Schedules a *kind*-based action whose kind no selected plugin contributes.
struct UnknownKindGate;
#[async_trait::async_trait]
impl ToolGateHook for UnknownKindGate {
    async fn gate(
        &self,
        _c: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Schedule {
            correlation_id: "sched-1".to_string(),
            action_kind: Some("plugin-only-kind".to_string()),
        }
    }
}

struct SuspendGate;
#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(
        &self,
        _c: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: "perm-1".to_string(),
        }
    }
}

const FP: &str = "catalog-a";
const SNAP: &str = "snapshot-1";

fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        metadata: Default::default(),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fp.clone(),
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
        delegation_origin: None,
        model_ref_override: None,
    }
}

fn context(commit: &Arc<MemoryCommitCoordinator>) -> RuntimeRunContext {
    RuntimeRunContext::new().with_commit(commit.clone())
}

#[tokio::test]
async fn scheduled_action_commits_then_perform_runs_it() {
    // RS-SCH-001.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // The run defers the call: it awaits on a committed ScheduledAction ticket.
    let state = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs");
    assert_eq!(state, RunState::Awaiting);
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a scheduled action is committed");
    assert_eq!(ticket.reason, AwaitReason::ScheduledAction);
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the action is deferred, not run"
    );

    // Performing the committed action runs it and the run completes.
    let state = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect("perform");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the deferred action ran once"
    );
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none(),
        "the ticket is cleared once performed"
    );
}

#[tokio::test]
async fn performing_a_scheduled_action_twice_is_idempotent() {
    // RS-SCH-001 (idempotency): a duplicate perform after the action committed is
    // rejected, not run again.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    runtime
        .execute(activation(), context(&commit))
        .await
        .expect("awaits on a scheduled action");
    runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect("first perform");
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // The ticket is cleared, so a second perform finds nothing to do.
    let err = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("duplicate perform rejected");
    assert!(err.to_string().contains("not awaiting"));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the action ran exactly once");
}

#[tokio::test]
async fn scheduling_an_unselected_plugin_action_kind_fails_closed() {
    // RS-SCH-005: a scheduled-action kind absent from the resolved environment
    // (its owning plugin is not selected) fails the run closed at the bound.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(UnknownKindGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let state = runtime
        .execute(activation(), context(&commit))
        .await
        .expect("runs to a terminal");
    assert_eq!(
        state,
        RunState::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::CapabilityBound
        ))
    );
    // No ticket is committed and the action never runs.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none()
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn perform_on_a_non_scheduled_run_fails_closed() {
    // RS-SCH-004: a result for a run not awaiting on a scheduled action is rejected.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran, Arc::new(SuspendGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // No such run: not awaiting.
    let err = runtime
        .perform_scheduled_action(
            &RunId("ghost".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("no run");
    assert!(err.to_string().contains("not awaiting"));

    // A run awaiting on a tool-permission ticket is not a scheduled action.
    runtime
        .execute(activation(), context(&commit))
        .await
        .expect("awaits on permission");
    let err = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("not a scheduled action");
    assert!(
        err.to_string()
            .contains("not awaiting on a scheduled action")
    );
}

#[tokio::test]
async fn an_uncommitted_scheduled_action_is_not_wakeable() {
    // RS-SCH-003/007: a scheduled candidate whose ThreadCommit did not persist is
    // not wakeable — only committed records are dispatch truth.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Execute with no commit boundary: the run reaches an awaiting state, but the
    // candidate ticket is never persisted.
    let state = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(state, RunState::Awaiting);

    // No committed ScheduledAction exists, so there is nothing to perform.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none()
    );
    let err = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("uncommitted candidate is not wakeable");
    assert!(err.to_string().contains("not awaiting"));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_resume_with_a_wrong_fingerprint_for_a_scheduled_action_is_rejected() {
    // RS-SCH-002: a resume targeting a committed ScheduledAction but carrying the
    // wrong catalog fingerprint is rejected without running the action or mutating
    // the committed ticket.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    runtime
        .execute(activation(), context(&commit))
        .await
        .expect("awaits on a scheduled action");

    let stale = ResumeCommand {
        correlation_id: "sched-1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(
            "wrong-fingerprint".to_string(),
        ),
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
        now_ms: 0,
    };
    let err = runtime
        .resume(stale, commit.as_ref(), context(&commit))
        .await
        .expect_err("a mismatched fingerprint is rejected");
    assert!(!err.to_string().is_empty());
    assert_eq!(ran.load(Ordering::SeqCst), 0, "the action did not run");
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_some(),
        "the committed ticket is unchanged"
    );
}

#[tokio::test]
async fn a_stop_policy_makes_a_late_scheduled_result_fail_closed() {
    // RS-CTRL-002: a stop policy commits a terminal stop reason; a deferred result
    // that arrives later is rejected without running the action or mutating facts.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone(), Arc::new(ScheduleGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    runtime
        .execute(activation(), context(&commit))
        .await
        .expect("awaits on a scheduled action");

    // The host stop policy commits a terminal Stopped(reason).
    let state = runtime
        .stop_run(
            RunId("run-1".to_string()),
            ThreadId("thread-1".to_string()),
            "budget exhausted".to_string(),
            context(&commit),
        )
        .await
        .expect("stop");
    assert_eq!(
        state,
        RunState::Ended(EndCause::Stopped("budget exhausted".to_string()))
    );

    // A late scheduled perform is rejected; the action never runs.
    let err = runtime
        .perform_scheduled_action(
            &RunId("run-1".to_string()),
            commit.as_ref(),
            context(&commit),
            0,
        )
        .await
        .expect_err("late perform rejected");
    assert!(err.to_string().contains("not awaiting"));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
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
        .expect("awaits on a scheduled action");

    // Cancel commits a terminal Cancelled and clears the ticket.
    let state = runtime
        .cancel_run(
            RunId("run-1".to_string()),
            ThreadId("thread-1".to_string()),
            context(&commit),
        )
        .await
        .expect("cancel");
    assert_eq!(state, RunState::Ended(EndCause::Cancelled));

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
    assert!(err.to_string().contains("not awaiting"));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}
