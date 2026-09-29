// @formal evidence tool.session_thread_composition
//! A gate Suspend awaits the run on a committed awaiting ticket; a validated
//! resume continues it, and a mismatched or stale resume fails closed (G5/G28).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView as _;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor, ToolKind,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput, ToolOutputSpiller};
use awaken_runtime_contract::{ActiveToolBatch, ToolBatch};
use awaken_store_inmem::{MemoryCommitCoordinator, replay_latest_state};

/// Calls `echo` once, then ends with text on the next inference.
struct ToolThenText {
    calls: AtomicUsize,
    second_tool: bool,
}

#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            let mut calls = vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "ping"}),
            }];
            if self.second_tool {
                calls.push(ToolCall {
                    call_id: "call-2".into(),
                    tool_id: "lookup".into(),
                    arguments: serde_json::json!({"text":"second"}),
                });
            }
            AssistantOutput::from_tool_calls(calls)
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

struct PrefixSpiller;

#[async_trait::async_trait]
impl ToolOutputSpiller for PrefixSpiller {
    async fn spill(
        &self,
        _run_id: &RunId,
        _call_id: &str,
        content: String,
    ) -> Result<String, ToolError> {
        Ok(format!("preview: {content}"))
    }
}

#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(
        &self,
        _ctx: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "ticket-1".to_string(),
        })
    }
}

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";

fn snapshot() -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        metadata: Default::default(),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fingerprint.clone(),
            instructions: String::new(),
            max_steps: Some(16),
            delegation_limits: Default::default(),
            model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                ModelBinding::new("p".to_string(), "m".to_string(), "b".to_string()),
            ),
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
            second_tool: false,
        }))
        .with_tool(Arc::new(EchoTool { ran }))
        .with_gate(Arc::new(SuspendGate));
    runtime.register_snapshot(snapshot());
    runtime
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("run-1".to_string()),
        ThreadId("thread-1".to_string()),
        snapshot(),
        vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
    )
}

fn resume_command(result: ResumeResult) -> ResumeCommand {
    ResumeCommand {
        operation_id: None,
        correlation_id: "ticket-1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result,
        context_messages: Vec::new(),
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
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));

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
            resume_command(ResumeResult::allow()),
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
    // resumed result/batch publication), Ended. Each actual model invocation
    // adds Entered/Returned Running evidence before its output is consumed.
    // The latest fact is the
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
            RunState::Running,
            RunState::Running,
            RunState::Awaiting,
            RunState::Running,
            RunState::Running,
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
async fn inherited_configuration_is_ticket_truth_after_serialization_and_resume() {
    // Causes: C1 activation has inherited settings A; C2 tool suspends; C3 a
    // resume supplies different live settings B, or no process-local settings;
    // C4 the committed ticket crosses serde. Effects: T1 A is retained exactly;
    // T2 B cannot execute/consume the ticket; T3 a bare resume restores A and
    // executes exactly once. This is real Runtime/commit/resume coverage; the
    // deterministic model is not a vendor risk-classification oracle.
    // | Rule | Input | Ticket | Live resume | Effect |
    // | T1 | A | serde round-trip | - | exact A |
    // | T2 | A | A | B | error, zero effects, ticket retained |
    // | T3 | A | A | none | restore A, one effect, ticket cleared |
    use awaken_runtime_contract::activation::InheritedToolsets;
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };
    let source = |domain: &str| {
        InheritedToolsets::with_caller(
            vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::with_optional_configuration(
                    "web_fetch",
                    ToolExecutionPolicy::default(),
                    Some(
                        serde_json::json!({"type":"web_fetch","domains":{"type":"allow","domains":[domain]}}),
                    ),
                )],
            }],
            None,
        )
    };
    let a = source("example.com");
    let b = source("example.net");
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let state = runtime
        .execute(
            activation().with_inherited_toolsets(Some(a.clone())),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .unwrap();
    assert_eq!(state, RunState::Awaiting, "T1");
    let ticket = commit.resume_ticket_for(&RunId("run-1".into())).unwrap();
    let restored: awaken_agent_contract::agent::awaiting::ResumeTicket =
        serde_json::from_slice(&serde_json::to_vec(&ticket).unwrap()).unwrap();
    assert_eq!(
        restored.inherited_toolsets,
        Some(a.clone()),
        "T1 exact frozen source"
    );
    let error = runtime
        .resume(
            resume_command(ResumeResult::allow()),
            commit.as_ref(),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_inherited_toolsets(Some(b)),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("committed ticket"),
        "T2 source conflict"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0, "T2 no tool effect");
    assert_eq!(
        commit
            .resume_ticket_for(&RunId("run-1".into()))
            .unwrap()
            .inherited_toolsets,
        Some(a),
        "T2 ticket not consumed"
    );
    assert_eq!(
        runtime
            .resume(
                resume_command(ResumeResult::allow()),
                commit.as_ref(),
                RuntimeRunContext::new().with_commit(commit.clone())
            )
            .await
            .unwrap(),
        RunState::Ended(EndCause::NaturalEnd),
        "T3 bare context uses ticket"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1, "T3 one effect");
    assert!(
        commit.resume_ticket_for(&RunId("run-1".into())).is_none(),
        "T3 terminal ticket cleared"
    );
}

#[tokio::test]
async fn runtime_convenience_entries_freeze_inherited_sources_before_await() {
    // Causes: C1 caller supplies an explicit v1 source in live context; C2 the
    // entry is one-shot run, start_run, completion, or stable-id completion;
    // C3 the tool suspends. Effects: E1 every committed ticket retains exactly
    // C1; E2 an approved completion executes once. No entry may produce a
    // ticket without C1 and then rely on a live context to repair it.
    // | Rule | Entry | Await exposed | Effects |
    // | R1 | run | yes | E1 |
    // | R2 | start_run | yes | E1 |
    // | R3 | run_to_completion | answered internally | E1,E2 |
    // | R4 | run_to_completion_with_id | answered internally | E1,E2 |
    use awaken_runtime_contract::activation::InheritedToolsets;
    let source = InheritedToolsets::with_caller(Vec::new(), None);
    let context = |commit: &Arc<MemoryCommitCoordinator>| {
        RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit.clone())
            .with_inherited_toolsets(Some(source.clone()))
    };
    for entry in ["run", "start_run"] {
        let ran = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(ran.clone());
        let commit = Arc::new(MemoryCommitCoordinator::new(
            awaken_lease_contract::system_clock(),
        ));
        let run_id = if entry == "run" {
            assert_eq!(
                runtime
                    .run(&snapshot(), "go", context(&commit))
                    .await
                    .unwrap(),
                RunState::Awaiting,
                "R1"
            );
            commit.committed().run_facts.last().unwrap().run_id.clone()
        } else {
            let (id, state) = runtime
                .start_run(&snapshot(), "thread-start", "go", context(&commit))
                .await
                .unwrap();
            assert_eq!(state, RunState::Awaiting, "R2");
            id
        };
        assert_eq!(
            commit
                .resume_ticket_for(&run_id)
                .unwrap()
                .inherited_toolsets,
            Some(source.clone()),
            "R1/R2 E1"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "R1/R2 no side effect before approval"
        );
    }
    for entry in ["run_to_completion", "run_to_completion_with_id"] {
        let ran = Arc::new(AtomicUsize::new(0));
        let runtime = runtime(ran.clone());
        let commit = Arc::new(MemoryCommitCoordinator::new(
            awaken_lease_contract::system_clock(),
        ));
        let mut tickets = 0;
        let mut decide = |ticket: &awaken_agent_contract::agent::awaiting::ResumeTicket| {
            assert_eq!(ticket.inherited_toolsets, Some(source.clone()), "R3/R4 E1");
            tickets += 1;
            ResumeResult::allow()
        };
        let state = if entry == "run_to_completion" {
            runtime
                .run_to_completion(
                    &snapshot(),
                    "thread-complete",
                    "go",
                    context(&commit),
                    &mut decide,
                )
                .await
                .unwrap()
        } else {
            runtime
                .run_to_completion_with_id(
                    &snapshot(),
                    RunId("stable-inherited".into()),
                    "thread-stable",
                    "go",
                    context(&commit),
                    &mut decide,
                )
                .await
                .unwrap()
        };
        assert_eq!(
            state,
            RunState::Ended(EndCause::NaturalEnd),
            "R3/R4 E2 terminal"
        );
        assert_eq!(tickets, 1, "R3/R4 one committed await");
        assert_eq!(ran.load(Ordering::SeqCst), 1, "R3/R4 E2 one effect");
    }
}

#[tokio::test]
async fn activated_resume_rejects_a_different_source_before_tool_effect() {
    // Causes: A committed awaiting ticket carries A; a dispatched resume
    // activation carries B, no source, or A. Effects: B/none reject without
    // changing the ticket or running the tool; A resumes exactly once.
    // | Rule | Ticket | Activation | Effect |
    // | P1 | A | B | reject, zero effect |
    // | P2 | A | none | reject, zero effect |
    // | P3 | A | A | one effect, terminal |
    use awaken_runtime_contract::activation::InheritedToolsets;
    use awaken_runtime_contract::execution::RunAttemptExecutor;
    let a = InheritedToolsets::with_caller(Vec::new(), None);
    let b = InheritedToolsets::with_caller(Vec::new(), Some(&a));
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    assert_eq!(
        runtime
            .execute(
                activation().with_inherited_toolsets(Some(a.clone())),
                RuntimeRunContext::new().with_commit(commit.clone())
            )
            .await
            .unwrap(),
        RunState::Awaiting
    );
    for (rule, source) in [("P1", Some(b)), ("P2", None)] {
        let err = RunAttemptExecutor::resume(
            &runtime,
            activation().with_inherited_toolsets(source),
            resume_command(ResumeResult::allow()),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("committed ticket"), "{rule}");
        assert_eq!(ran.load(Ordering::SeqCst), 0, "{rule} no effect");
        assert_eq!(
            commit
                .resume_ticket_for(&RunId("run-1".into()))
                .unwrap()
                .inherited_toolsets,
            Some(a.clone()),
            "{rule} ticket retained"
        );
    }
    assert_eq!(
        RunAttemptExecutor::resume(
            &runtime,
            activation().with_inherited_toolsets(Some(a)),
            resume_command(ResumeResult::allow()),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone())
        )
        .await
        .unwrap(),
        RunState::Ended(EndCause::NaturalEnd),
        "P3"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1, "P3 one tool effect");
}

#[tokio::test]
async fn deny_resume_feeds_a_blocked_result_without_running_the_tool() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .resume(
            resume_command(ResumeResult::deny(Some("nope".to_string()))),
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
        event.kind == EventKind::PermissionConfirmed && event.payload["result"] == "deny"
    }));
    assert!(
        !commit.committed().events.iter().any(|event| {
            event.kind == EventKind::PermissionDecided && event.payload["decision"] == "denied"
        }),
        "a person's refusal is a confirmation, never a second gate decision"
    );
}

#[tokio::test]
async fn resume_with_wrong_fingerprint_fails_closed() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran);
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    suspend(&commit, &runtime).await;

    let mut command = resume_command(ResumeResult::allow());
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
async fn resume_rejects_a_ticket_batch_payload_conflict_before_tool_execution() {
    // Cause/effect graph: C1 ResumeCommand matches the committed ticket; C2 the
    // active batch is present/readable; C3 its Run/Thread/call/kind/correlation/
    // tool payload all match the ticket. Effects: E1 C1+C2+C3 resumes; E2 an
    // independently valid but payload-conflicting batch fails before any tool
    // effect or resume commit and leaves the Run Awaiting. Rules RB1=all true
    // (covered by the allow test), RB2=C1+C2+!C3=>E2.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    suspend(&commit, &runtime).await;

    let run_id = RunId("run-1".into());
    let thread_id = ThreadId("thread-1".into());
    let ticket = commit
        .resume_ticket_for(&run_id)
        .expect("RB2 durable ticket");
    let state = Store::rebuild(&commit.committed_state(&thread_id));
    let batch = ActiveToolBatch::load(&state)
        .expect("RB2 readable batch")
        .expect("RB2 present batch");
    let mut value = serde_json::to_value(batch).expect("RB2 encode fixture");
    value["calls"][0]["call"]["tool_id"] = serde_json::json!("another-tool");
    let mismatched: ToolBatch =
        serde_json::from_value(value).expect("RB2 mismatch remains internally valid");
    commit
        .commit(
            awaken_agent_contract::thread::commit::staged::ThreadCommit::assemble(
                thread_id.clone(),
                awaken_agent_contract::thread::commit::staged::RunDisposition::awaiting(
                    ticket.clone(),
                ),
                true,
                Vec::new(),
                vec![ActiveToolBatch::write(&Some(mismatched))],
                Vec::new(),
            ),
        )
        .await
        .expect("RB2 commit conflicting durable projection");
    let commits_before = commit.commit_count();

    // RB3: the tool-only handoff shares RB2's validator; it must not turn a
    // payload conflict into approval or execution just because inference is skipped.
    for tool_only in [false, true] {
        let context = RuntimeRunContext::new().with_commit(commit.clone());
        let result = if tool_only {
            runtime
                .resume_tool_batch(
                    resume_command(ResumeResult::allow()),
                    commit.as_ref(),
                    context,
                )
                .await
        } else {
            runtime
                .resume(
                    resume_command(ResumeResult::allow()),
                    commit.as_ref(),
                    context,
                )
                .await
        };
        let error = result.expect_err("RB2/RB3 ticket/batch conflict must fail closed");
        assert!(error.to_string().contains("incoherent tool resume"));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no tool effect");
        assert_eq!(commit.commit_count(), commits_before, "no resume commit");
        assert_eq!(commit.run_state(&run_id), Some(RunState::Awaiting));
    }
}

#[tokio::test]
async fn second_resume_after_completion_is_not_awaiting() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran);
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    runtime
        .resume(
            resume_command(ResumeResult::allow()),
            commit.as_ref(),
            context,
        )
        .await
        .expect("first resume completes");

    // The ticket was consumed; a stale second resume fails closed.
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let err = runtime
        .resume(
            resume_command(ResumeResult::allow()),
            commit.as_ref(),
            context,
        )
        .await
        .expect_err("a stale resume is rejected");
    assert!(err.to_string().contains("not awaiting"));
}

#[tokio::test]
async fn permission_wait_rejects_a_client_result_bypass() {
    // A permission target accepts only an allow/deny decision. Treating an
    // injected ToolResult as equivalent would bypass both authorization and the
    // real tool execution path.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    suspend(&commit, &runtime).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let error = runtime
        .resume(
            resume_command(ResumeResult::ToolResult(ToolOutput::ok(
                "call-1",
                "client-computed",
            ))),
            commit.as_ref(),
            context,
        )
        .await
        .expect_err("a client result cannot answer a permission wait");
    assert!(error.to_string().contains("incompatible"));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert!(
        commit.resume_ticket_for(&RunId("run-1".into())).is_some(),
        "the rejected resume leaves the durable wait intact"
    );
}

#[tokio::test]
async fn declared_client_tool_awaits_external_result_without_entering_host_executor() {
    // Causal graph: a ClientExecuted descriptor creates a durable ExternalEvent
    // wait; the exact ToolResult may carry one stable System Message; C3 durable
    // ingress supplies operation O1. Effects: the Host executor is never entered,
    // Runtime commits System immediately before Tool, and the same resumed
    // ThreadCommit replaces the ticket with ResumeApplied(O1).
    //
    // | Rule | descriptor | resume context | Effect |
    // |---|---|---|---|
    // | C1 | regular | n/a | Host executes tool |
    // | C2 | client-executed | exact System + result | await, then System→Tool; Host never runs |
    // | C3 | C2 + operation O1 | exact durable delivery | ticket absent + ResumeApplied(O1) |
    // Constraints/invariants: the client-executed descriptor is the sole owner;
    // System context and its correlated Tool result commit adjacently and once.
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let mut activation = activation();
    activation.snapshot.resolved_spec.tool_descriptors[0] =
        activation.snapshot.resolved_spec.tool_descriptors[0]
            .clone()
            .with_kind(ToolKind::ClientExecuted);

    let outcome = runtime
        .execute(
            activation,
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("client tool call is admitted");
    assert_eq!(outcome, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".into()))
        .expect("external result ticket is durable");
    assert_eq!(ticket.reason(), AwaitReason::ExternalEvent);
    assert_eq!(ticket.call_id(), Some("call-1"));
    // C4: an external model loop may select the tool-only continuation. It must
    // settle the same batch and return Running without entering Native inference.
    let ticket_correlation_id = ticket.correlation_id.clone();
    let system = Message::text(
        MessageId("session-system-tool-reply".into()),
        Role::System,
        "reply context",
    );

    let outcome = runtime
        .resume_tool_batch(
            ResumeCommand {
                operation_id: Some("client-result-operation-1".into()),
                correlation_id: ticket.correlation_id,
                run_id: RunId("run-1".into()),
                thread_id: ThreadId("thread-1".into()),
                snapshot_id: ExecutableAgentSnapshotId(SNAPSHOT_ID.into()),
                catalog_fingerprint: CatalogFingerprint(FINGERPRINT.into()),
                result: ResumeResult::ToolResult(ToolOutput::ok("call-1", "client-computed")),
                context_messages: vec![system.clone()],
                now_ms: 0,
            },
            commit.as_ref(),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_tool_output_spiller(Arc::new(PrefixSpiller)),
        )
        .await
        .expect("exact client result settles the shared tool batch");
    assert_eq!(
        outcome,
        RunState::Running,
        "C4 returns to the external loop"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert!(
        commit.resume_ticket_for(&RunId("run-1".into())).is_none(),
        "C3 accepted delivery consumes its ticket"
    );
    let committed = commit.committed();
    let receipts = committed
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ResumeApplied)
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1, "C3 emits one durable receipt");
    assert_eq!(
        receipts[0].payload,
        serde_json::json!({
            "operation_id": "client-result-operation-1",
            "correlation_id": ticket_correlation_id,
        }),
        "C3 receipt retains the exact ingress identity"
    );
    assert!(
        committed.messages.windows(2).any(|messages| {
            messages[0] == system
                && messages[1].role == Role::Tool
                && messages[1].text_content() == "preview: client-computed"
        }),
        "C2: stable System is adjacent to and precedes the correlated Tool result"
    );
}

#[tokio::test]
async fn permission_wait_rejects_free_form_input() {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(ran);
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
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

#[tokio::test]
async fn resumed_requested_client_calls_keep_their_frozen_execution_owner() {
    // Cause/effect decision table: C1 first client call leaves a Requested sibling;
    // C2 sibling is client/regular; C3 runtime is warm/reconstructed. R1 C2 client
    // => second ExternalEvent ticket, zero Host invocation, one inference until both
    // results arrive. R2 C2 regular => existing Host permission gate. C3 cannot change
    // either decision. Completed first results stay behind the batch publication barrier.
    for cold in [false, true] {
        for second_client in [false, true] {
            let ran = Arc::new(AtomicUsize::new(0));
            let model = Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
                second_tool: true,
            });
            let mut act = activation();
            act.snapshot.resolved_spec.tool_descriptors[0].kind = ToolKind::ClientExecuted;
            let mut second = act.snapshot.resolved_spec.tool_descriptors[0].clone();
            second.id = "lookup".into();
            second.kind = if second_client {
                ToolKind::ClientExecuted
            } else {
                ToolKind::Regular
            };
            act.snapshot.resolved_spec.tool_descriptors.push(second);
            let frozen = act.snapshot.clone();
            let make_runtime = || {
                let runtime = Runtime::new()
                    .with_llm(model.clone())
                    .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
                    .with_gate(Arc::new(SuspendGate));
                runtime.register_snapshot(frozen.clone());
                runtime
            };
            let mut runtime = make_runtime();
            let commit = Arc::new(MemoryCommitCoordinator::new(
                awaken_lease_contract::system_clock(),
            ));
            assert_eq!(
                runtime
                    .execute(act, RuntimeRunContext::new().with_commit(commit.clone()))
                    .await
                    .unwrap(),
                RunState::Awaiting
            );
            if cold {
                runtime = make_runtime();
            }
            for index in 1..=2 {
                let ticket = commit.resume_ticket_for(&RunId("run-1".into())).unwrap();
                assert_eq!(ticket.reason(), AwaitReason::ExternalEvent);
                assert_eq!(ticket.call_id(), Some(format!("call-{index}").as_str()));
                let command = ResumeCommand {
                    operation_id: Some(format!("reply-{index}")),
                    correlation_id: ticket.correlation_id,
                    run_id: RunId("run-1".into()),
                    thread_id: ThreadId("thread-1".into()),
                    snapshot_id: ExecutableAgentSnapshotId(SNAPSHOT_ID.into()),
                    catalog_fingerprint: CatalogFingerprint(FINGERPRINT.into()),
                    result: ResumeResult::ToolResult(ToolOutput::ok(
                        format!("call-{index}"),
                        format!("result-{index}"),
                    )),
                    context_messages: Vec::new(),
                    now_ms: 0,
                };
                let state = runtime
                    .resume_tool_batch(
                        command,
                        commit.as_ref(),
                        RuntimeRunContext::new().with_commit(commit.clone()),
                    )
                    .await
                    .unwrap();
                assert_eq!(ran.load(Ordering::SeqCst), 0);
                assert_eq!(model.calls.load(Ordering::SeqCst), 1);
                if index == 1 {
                    assert_eq!(state, RunState::Awaiting);
                    let next = commit.resume_ticket_for(&RunId("run-1".into())).unwrap();
                    assert_eq!(next.call_id(), Some("call-2"));
                    assert_eq!(
                        next.reason(),
                        if second_client {
                            AwaitReason::ExternalEvent
                        } else {
                            AwaitReason::ToolPermission
                        }
                    );
                    assert_eq!(
                        commit
                            .committed_messages(&ThreadId("thread-1".into()))
                            .iter()
                            .filter(|m| m.role == Role::Tool)
                            .count(),
                        0
                    );
                    if !second_client {
                        break;
                    }
                } else {
                    assert_eq!(state, RunState::Running);
                    assert!(commit.resume_ticket_for(&RunId("run-1".into())).is_none());
                    let results: Vec<_> = commit
                        .committed_messages(&ThreadId("thread-1".into()))
                        .into_iter()
                        .filter(|m| m.role == Role::Tool)
                        .collect();
                    assert_eq!(results.len(), 2);
                    assert!(results[0].text_content().contains("result-1"));
                    assert!(results[1].text_content().contains("result-2"));
                }
            }
        }
    }
}

struct EchoThenProbe {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for EchoThenProbe {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = match n {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "confirmation".into(),
                tool_id: "echo".into(),
                arguments: serde_json::json!({}),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "after-confirmation".into(),
                tool_id: "probe".into(),
                arguments: serde_json::json!({}),
            }]),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}
struct ConfirmEchoOnly;
#[async_trait::async_trait]
impl ToolGateHook for ConfirmEchoOnly {
    async fn gate(&self, call: &ToolCall, _state: &Store) -> GateDecision {
        GateDecision::plugin_gate(if call.tool_id == "echo" {
            GateOutcome::RequireConfirmation {
                correlation_id: "ticket-1".into(),
            }
        } else {
            GateOutcome::Allow
        })
    }
}
struct OperationProbe(Arc<std::sync::Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl RawTool for OperationProbe {
    fn id(&self) -> &str {
        "probe"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.0.lock().unwrap().push(
            awaken_runtime_contract::tool::current_tool_operation_context()
                .expect("runtime operation")
                .operation_id,
        );
        Ok(ToolOutput::ok(call.call_id, "recorded"))
    }
}

#[tokio::test]
async fn resumed_tool_operation_uses_the_committed_assistant_coordinate() {
    // Cause graph: C1 Runtime emits a tool and awaits confirmation; C2 a
    // validated result resumes the SAME Run; C3 the next model response invokes
    // another physical tool. E1 that message is assistant-1; E2 executor and
    // retained batch use step 1 too; E3 cold projection recovers the same op.
    // R1 C1+C2+C3 => E1+E2+E3. Fresh step 0 is covered by the existing suspend
    // test. This runs the real Runtime resume/drive/dispatch, not a manually
    // aligned Message+ToolBatch fixture that missed the local-step reset.
    let operations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut spec = snapshot();
    spec.resolved_spec
        .tool_descriptors
        .push(ToolDescriptor::pinned(
            "test",
            "probe",
            "Probe",
            serde_json::json!({"type":"object"}),
        ));
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoThenProbe {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(EchoTool {
            ran: Arc::new(AtomicUsize::new(0)),
        }))
        .with_tool(Arc::new(OperationProbe(operations.clone())))
        .with_gate(Arc::new(ConfirmEchoOnly));
    runtime.register_snapshot(spec.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let mut input = activation();
    input.snapshot = spec;
    assert_eq!(
        runtime
            .execute(input, RuntimeRunContext::new().with_commit(commit.clone()))
            .await
            .unwrap(),
        RunState::Awaiting
    );
    let outcome = runtime
        .resume(
            resume_command(ResumeResult::allow()),
            commit.as_ref(),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .unwrap();
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    let history = commit.committed();
    let message = history.messages.iter().find(|message| message.content.iter().any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == "after-confirmation"))).unwrap();
    let run = RunId("run-1".into());
    assert_eq!(message.id.assistant_step_of(&run), Some(1), "E1");
    let expected = ToolBatch::operation_id_for_step(&run, 1, "after-confirmation");
    assert_eq!(
        *operations.lock().unwrap(),
        vec![expected.clone()],
        "E2 executor identity"
    );
    let stored = Store::rebuild(&history.state);
    let batch = ActiveToolBatch::load(&stored).unwrap().unwrap();
    assert_eq!(
        batch.operation_id("after-confirmation"),
        expected,
        "E2 stored identity"
    );
    let cold: ToolBatch = serde_json::from_slice(&serde_json::to_vec(&batch).unwrap()).unwrap();
    assert_eq!(cold.operation_id("after-confirmation"), expected, "E3");
}

#[tokio::test]
async fn resume_does_not_restart_the_same_runs_step_ceiling() {
    // B1 Run ceiling=1 + step 0 awaits; B2 approved resume completes its existing
    // tool; B3 the next inference would be logical step 1. Effect B1+B2+B3:
    // finish MaxSteps without invoking that model or new physical tool. The
    // ceiling belongs to the Run, not to a physical resume drive. This shares
    // the same monotonic logical coordinate as operation identity test R1.
    let model = Arc::new(EchoThenProbe {
        calls: AtomicUsize::new(0),
    });
    let operations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut limited = snapshot();
    limited.resolved_spec.max_steps = Some(1);
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_tool(Arc::new(EchoTool {
            ran: Arc::new(AtomicUsize::new(0)),
        }))
        .with_tool(Arc::new(OperationProbe(operations.clone())))
        .with_gate(Arc::new(ConfirmEchoOnly));
    runtime.register_snapshot(limited.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let mut input = activation();
    input.snapshot = limited;
    assert_eq!(
        runtime
            .execute(input, RuntimeRunContext::new().with_commit(commit.clone()))
            .await
            .unwrap(),
        RunState::Awaiting
    );
    assert_eq!(
        runtime
            .resume(
                resume_command(ResumeResult::allow()),
                commit.as_ref(),
                RuntimeRunContext::new().with_commit(commit.clone())
            )
            .await
            .unwrap(),
        RunState::Ended(EndCause::MaxSteps)
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "B3 no additional model request"
    );
    assert!(
        operations.lock().unwrap().is_empty(),
        "B3 no additional physical effect"
    );
}
