//! Delegation (ADR-0044): a tool call whose id matches the executor's `tool_id` is
//! routed to the `DelegationExecutor` instead of the tool registry. An `Ended` step folds
//! the delegate's token usage into the parent thread and feeds its reply back; a
//! `Awaiting` step awaits the parent on a `Delegation` ticket carrying the opaque
//! handle; an `Err` feeds a model-visible error. A awaiting delegation resumes
//! through the executor, which may finish, re-await, or fail (RD2/RD3/RD4).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::{DelegationOrigin, DelegationStatus};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationExecutor, DelegationRequest, DelegationResume,
    DelegationStep, RunDelegations,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ThreadUsage, TokenUsage, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolBatchPhase, ToolCallPhase};

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";
const DELEGATE_TOOL: &str = "agent_run";
const CALL_ID: &str = "d1";

struct SuspendGate;

#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(
        &self,
        _context: &PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Suspend {
            ticket_id: "approval-1".to_string(),
        }
    }
}

/// The parent agent: delegates once (a call to `agent_run`), then ends with text —
/// so the delegate's folded result is observable in the next committed turn.
struct DelegateThenText {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for DelegateThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: CALL_ID.to_string(),
                tool_id: DELEGATE_TOOL.to_string(),
                arguments: serde_json::json!({ "agent": "sub", "input": "hi" }),
            }])
        } else {
            AssistantOutput::text("parent done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The delegate step a mock executor should yield.
#[derive(Clone)]
enum Step {
    /// Finished with this reply and (optionally) this many tokens on `delegate-m`.
    Done(String, u64),
    /// Awaiting needing more input, carrying this opaque handle.
    Awaiting(serde_json::Value),
    /// Could not run.
    Fail(String),
}

fn step_to_result(step: &Step) -> Result<DelegationStep, DelegationExecutionError> {
    match step {
        Step::Done(text, tokens) => {
            let mut usage = ThreadUsage::default();
            if *tokens > 0 {
                usage.record(
                    "delegate-m",
                    TokenUsage {
                        prompt_tokens: *tokens,
                        completion_tokens: *tokens,
                        ..Default::default()
                    },
                );
            }
            Ok(DelegationStep::Ended {
                text: text.clone(),
                usage,
            })
        }
        Step::Awaiting(continuation) => Ok(DelegationStep::Awaiting {
            continuation: continuation.clone(),
        }),
        Step::Fail(err) => Err(DelegationExecutionError::new(err.clone())),
    }
}

/// A delegation executor whose `run` and `resume` yield configured [`Step`]s, and
/// which records the `(handle, input)` each resume was called with.
struct MockDelegationExecutor {
    run_step: Step,
    resume_step: Step,
    started: Mutex<Vec<DelegationOrigin>>,
    resumed_with: Mutex<Vec<(serde_json::Value, String)>>,
}

impl MockDelegationExecutor {
    fn new(run_step: Step, resume_step: Step) -> Self {
        Self {
            run_step,
            resume_step,
            started: Mutex::new(Vec::new()),
            resumed_with: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl DelegationExecutor for MockDelegationExecutor {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }
    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<String, DelegationExecutionError> {
        Ok("delegate".into())
    }
    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.started.lock().unwrap().push(request.origin);
        step_to_result(&self.run_step)
    }
    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.resumed_with
            .lock()
            .unwrap()
            .push((request.continuation, request.input));
        step_to_result(&self.resume_step)
    }
}

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
            tool_descriptors: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn runtime(executor: Arc<MockDelegationExecutor>) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(DelegateThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_delegation_executor(executor);
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
        initiator: None,
        model_ref_override: None,
    }
}

/// A resume command correlated to the `Delegation` ticket (its call id is the
/// correlation id), carrying the given user input.
fn resume_command(input: &str) -> ResumeCommand {
    ResumeCommand {
        correlation_id: CALL_ID.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result: ResumeResult::Input(input.to_string()),
        now_ms: 0,
    }
}

// --- CE-4: dispatch of a delegation tool call ---

#[tokio::test]
async fn delegation_done_folds_delegate_usage_and_feeds_the_reply_back() {
    // T2: an allowed delegation that finishes feeds the delegate's reply back as the
    // tool result AND folds the delegate's own token spend into the parent thread's
    // running tally, so a session's usage counts delegated work.
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Done("delegate replied".to_string(), 5),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(executor);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    // The delegate's reply came back as the tool result the parent then saw.
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("delegate replied")),
        "the delegate's reply is fed back as the tool result"
    );
    // The delegate's usage was folded into the parent thread's committed tally.
    let usage = ThreadUsage::from_committed_state(&committed.state);
    assert!(!usage.is_empty(), "the delegate's usage was recorded");
    assert_eq!(
        usage.total().prompt_tokens,
        5,
        "the parent thread's tally counts the delegate's tokens"
    );
}

#[tokio::test]
async fn approved_agent_run_reenters_the_delegation_router() {
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Done("approved delegate replied".to_string(), 0),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(executor.clone()).with_gate(Arc::new(SuspendGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let state = runtime
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("initial run awaits approval");
    assert_eq!(state, RunState::Awaiting);
    assert!(executor.started.lock().unwrap().is_empty());

    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("approval ticket");
    assert_eq!(ticket.reason, AwaitReason::ToolPermission);
    let command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0);
    let state = runtime
        .resume(
            command,
            commit.as_ref(),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("approved delegation runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(executor.started.lock().unwrap().len(), 1);
    assert!(commit.committed().messages.iter().any(|message| {
        message.role == Role::Tool && message.text_content().contains("approved delegate replied")
    }));
}

#[tokio::test]
async fn a_child_run_delegates_with_the_next_depth_and_ordinary_runtime_path() {
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Done("grandchild replied".to_string(), 0),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(executor.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let parent_origin = DelegationOrigin::root(RunId("root-run".into()), "root-call");
    let context = RuntimeRunContext::new().with_commit(commit);
    let mut activation = activation();
    activation.initiator = Some(parent_origin);

    let state = runtime.execute(activation, context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let started = executor.started.lock().unwrap();
    assert_eq!(started.len(), 1);
    let expected =
        DelegationOrigin::nested_for_agent(RunId("run-1".into()), CALL_ID, 1, &[], "agent-1")
            .unwrap();
    assert_eq!(started[0], expected, "the child continues its Run lineage");
}

#[tokio::test]
async fn delegation_awaiting_awaits_the_parent_on_a_delegation_ticket() {
    // T3: a delegate that needs more input awaits the parent on a Delegation ticket
    // carrying the opaque resume handle.
    let handle = serde_json::json!({ "task": "remote-42" });
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Awaiting(handle.clone()),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(executor);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Awaiting);

    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a delegation ticket is committed");
    assert_eq!(ticket.reason, AwaitReason::Delegation);
    assert_eq!(ticket.call_id.as_deref(), Some(CALL_ID));
    assert_eq!(
        ticket
            .pending_tool
            .as_ref()
            .and_then(|p| p.resume_handle.clone()),
        Some(handle),
        "the ticket carries the delegate's opaque handle"
    );
}

#[tokio::test]
async fn cancelling_an_awaiting_parent_atomically_persists_child_cancel_intent() {
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(executor);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    assert_eq!(
        runtime
            .execute(activation(), context.clone())
            .await
            .expect("delegation awaits"),
        RunState::Awaiting
    );
    assert_eq!(
        runtime
            .cancel_run(
                RunId("run-1".to_string()),
                ThreadId("thread-1".to_string()),
                context,
            )
            .await
            .expect("parent cancellation commits"),
        RunState::Ended(EndCause::Cancelled)
    );

    let store = Store::rebuild(&commit.committed().state);
    let registry = RunDelegations::load(&store)
        .expect("valid committed delegation state")
        .expect("delegation registry remains durable");
    assert_eq!(
        registry
            .delegations()
            .next()
            .expect("one child relationship")
            .status,
        DelegationStatus::CancelRequested
    );
    let batch = ActiveToolBatch::load(&store)
        .expect("valid committed tool-batch state")
        .expect("the cancelled batch remains durable");
    assert_eq!(batch.phase, ToolBatchPhase::Finalized);
    assert!(batch.calls.iter().all(|call| matches!(
        call.phase,
        ToolCallPhase::Completed(_) | ToolCallPhase::Indeterminate { .. }
    )));
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none(),
        "the same terminal commit clears the parent's resume ticket"
    );
}

#[tokio::test]
async fn delegation_error_feeds_a_model_visible_error_result() {
    // T4: a delegate that cannot run yields a model-visible error result; the run
    // continues rather than aborting.
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Fail("sub-agent unavailable".to_string()),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(executor);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("sub-agent unavailable")),
        "the delegate error is a model-visible tool result"
    );
}

// --- CE-10: resuming an awaiting delegation ---

/// Await the parent on a Delegation ticket (run step = Awaiting) and return the wired
/// runtime + commit coordinator ready to resume.
async fn await_on_delegation(
    executor: Arc<MockDelegationExecutor>,
) -> (Runtime, Arc<MemoryCommitCoordinator>) {
    let runtime = runtime(executor);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation(), context)
        .await
        .expect("initial run awaits");
    assert_eq!(
        state,
        RunState::Awaiting,
        "the parent awaiting on delegation"
    );
    assert!(
        commit
            .committed()
            .state
            .iter()
            .any(|command| command.key.0 == "runtime.delegations.v1"),
        "the relationship is committed before the child is entered"
    );
    (runtime, commit)
}

#[tokio::test]
async fn resuming_a_delegation_done_folds_the_reply_and_completes() {
    // RD2: resuming an awaiting delegation runs the executor one more step; a Done folds
    // the reply back as the delegate tool's result and the parent drives on.
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Done("delegate finished".to_string(), 0),
    ));
    let (runtime, commit) = await_on_delegation(executor.clone()).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(resume_command("more please"), commit.as_ref(), context)
        .await
        .expect("resume runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // The executor was resumed with the awaiting handle and the user's input.
    let resumed = executor.resumed_with.lock().unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].0, serde_json::json!({ "task": "remote-42" }));
    assert_eq!(resumed[0].1, "more please");
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("delegate finished")),
        "the resumed delegate reply is folded back as the tool result"
    );
    // The ticket is cleared once the delegation completes.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none()
    );
}

#[tokio::test]
async fn resuming_a_delegation_that_awaits_again_uses_the_new_handle() {
    // RD3: a resumed delegate that still needs input re-awaits the parent on a fresh
    // Delegation ticket carrying the NEW handle.
    let new_handle = serde_json::json!({ "task": "remote-99" });
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Awaiting(new_handle.clone()),
    ));
    let (runtime, commit) = await_on_delegation(executor).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(resume_command("still working?"), commit.as_ref(), context)
        .await
        .expect("resume runs");

    assert_eq!(state, RunState::Awaiting, "the delegate re-awaiting");
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a fresh delegation ticket is committed");
    assert_eq!(ticket.reason, AwaitReason::Delegation);
    assert_eq!(
        ticket
            .pending_tool
            .as_ref()
            .and_then(|p| p.resume_handle.clone()),
        Some(new_handle),
        "the re-await carries the delegate's new handle"
    );
}

#[tokio::test]
async fn resuming_a_delegation_error_feeds_an_error_result_and_completes() {
    // RD4: a resumed delegate that fails yields a model-visible error result; the
    // parent drives on rather than aborting.
    let executor = Arc::new(MockDelegationExecutor::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Fail("remote task lost".to_string()),
    ));
    let (runtime, commit) = await_on_delegation(executor).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(resume_command("status?"), commit.as_ref(), context)
        .await
        .expect("resume runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("remote task lost")),
        "the resumed delegate error is a model-visible tool result"
    );
}
