//! Delegation (ADR-0044): a tool call whose id matches the executor's `tool_id` is
//! routed to `RunDelegationService` instead of the tool registry. An `Ended` step folds
//! the delegate's token usage into the parent thread and feeds its reply back; a
//! `Awaiting` step awaits the parent on a `Delegation` ticket carrying the opaque
//! handle; an `Err` feeds a model-visible error. A awaiting delegation resumes
//! through the executor, which may finish, re-await, or fail (RD2/RD3/RD4).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::{
    ChildRunCancellation, DelegationOrigin, DelegationStatus,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    PendingChildRunResults, RunDelegationService, RunDelegations,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ThreadUsage, TokenUsage, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor, ToolKind,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, RuntimeRunContext,
};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolConcurrency, ToolError, ToolOutput, ToolRecoveryPolicy,
};
use awaken_runtime_contract::tool_batch::{ActiveToolBatch, ToolBatchPhase, ToolCallPhase};
use awaken_store_inmem::MemoryCommitCoordinator;

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";
const DELEGATE_TOOL: &str = "agent_run";
const CALL_ID: &str = "d1";

struct SuspendGate;

#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(
        &self,
        _context: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: "approval-1".to_string(),
        }
    }
}

/// The parent agent: delegates once (a call to `agent_run`), then ends with text —
/// so the delegate's folded result is observable in the next committed Step.
struct DelegateThenText {
    calls: AtomicUsize,
}

struct ParallelDelegatesThenText {
    calls: AtomicUsize,
}

struct MixedCallsThenText {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ParallelDelegatesThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![
                ToolCall {
                    call_id: "parallel-1".into(),
                    tool_id: DELEGATE_TOOL.into(),
                    arguments: serde_json::json!({"agent_id": "researcher", "input": "a"}),
                },
                ToolCall {
                    call_id: "parallel-2".into(),
                    tool_id: DELEGATE_TOOL.into(),
                    arguments: serde_json::json!({"agent_id": "writer", "input": "b"}),
                },
            ])
        } else {
            AssistantOutput::text("parent done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for MixedCallsThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![
                ToolCall {
                    call_id: "mixed-delegation".into(),
                    tool_id: DELEGATE_TOOL.into(),
                    arguments: serde_json::json!({"agent_id": "researcher", "input": "a"}),
                },
                ToolCall {
                    call_id: "mixed-regular".into(),
                    tool_id: "external_tool".into(),
                    arguments: serde_json::json!({}),
                },
            ])
        } else {
            AssistantOutput::text("parent done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
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
/// which records the `(handle, typed result)` each resume was called with.
struct MockRunDelegationService {
    run_step: Step,
    resume_step: Step,
    started: Mutex<Vec<DelegationOrigin>>,
    resumed_with: Mutex<Vec<(serde_json::Value, ResumeResult)>>,
    cancelled: Mutex<Vec<ChildRunCancellation>>,
}

#[derive(Clone, Copy)]
enum OwnershipDecision {
    Current,
    Lost,
    Unavailable,
}

struct ScriptedOwnership {
    decisions: Mutex<VecDeque<OwnershipDecision>>,
}

impl ScriptedOwnership {
    fn new(decisions: impl IntoIterator<Item = OwnershipDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
        }
    }
}

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for ScriptedOwnership {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        match self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(OwnershipDecision::Current)
        {
            OwnershipDecision::Current => Ok(()),
            OwnershipDecision::Lost => Err(AttemptOwnershipError::Lost),
            OwnershipDecision::Unavailable => {
                Err(AttemptOwnershipError::Unavailable("authority down".into()))
            }
        }
    }
}

struct ConcurrentRunDelegationService {
    barrier: tokio::sync::Barrier,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
}

#[derive(Default)]
struct ConcurrencyProbe {
    active: AtomicUsize,
    maximum_active: AtomicUsize,
}

impl ConcurrencyProbe {
    async fn observe(&self) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum_active.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ObservationRunDelegationService {
    probe: Arc<ConcurrencyProbe>,
    supports_parallel: bool,
}

struct ObservedRawTool {
    probe: Arc<ConcurrencyProbe>,
    concurrency: ToolConcurrency,
}

struct LateResultRunDelegationService {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct CrashOnceRunDelegationService {
    starts: AtomicUsize,
}

#[async_trait::async_trait]
impl RunDelegationService for CrashOnceRunDelegationService {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }

    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "recoverable-child".into(),
        ))
    }

    async fn start(
        &self,
        _request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        if self.starts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(DelegationExecutionError::retryable(
                "child owner crashed before reaching a boundary",
            ))
        } else {
            Ok(DelegationStep::Ended {
                text: "recovered child result".into(),
                usage: ThreadUsage::default(),
            })
        }
    }

    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("the recovered child ends")
    }
}

#[async_trait::async_trait]
impl RunDelegationService for LateResultRunDelegationService {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }

    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "remote-child".into(),
        ))
    }

    async fn start(
        &self,
        _request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.reached.notify_one();
        self.release.notified().await;
        Ok(DelegationStep::Ended {
            text: "too late".into(),
            usage: ThreadUsage::default(),
        })
    }

    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("the child never awaits")
    }
}

impl ConcurrentRunDelegationService {
    fn new(children: usize) -> Self {
        Self {
            barrier: tokio::sync::Barrier::new(children),
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl RunDelegationService for ConcurrentRunDelegationService {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }

    fn supports_parallel_completion(&self, _arguments: &serde_json::Value) -> bool {
        true
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum_active.fetch_max(active, Ordering::SeqCst);
        self.barrier.wait().await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(DelegationStep::Ended {
            text: format!("{} done", request.origin.parent_call_id),
            usage: ThreadUsage::default(),
        })
    }

    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("terminal-only children never resume")
    }
}

#[async_trait::async_trait]
impl RunDelegationService for ObservationRunDelegationService {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }

    fn supports_parallel_completion(&self, _arguments: &serde_json::Value) -> bool {
        self.supports_parallel
    }

    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "serial-child".into(),
        ))
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.probe.observe().await;
        Ok(DelegationStep::Ended {
            text: format!("{} done", request.origin.parent_call_id),
            usage: ThreadUsage::default(),
        })
    }

    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("terminal child never resumes")
    }
}

#[async_trait::async_trait]
impl RawTool for ObservedRawTool {
    fn id(&self) -> &str {
        "external_tool"
    }

    fn concurrency(&self, _arguments: &serde_json::Value) -> ToolConcurrency {
        self.concurrency.clone()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.probe.observe().await;
        Ok(ToolOutput::ok(call.call_id, "external tool done"))
    }
}

impl MockRunDelegationService {
    fn new(run_step: Step, resume_step: Step) -> Self {
        Self {
            run_step,
            resume_step,
            started: Mutex::new(Vec::new()),
            resumed_with: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl RunDelegationService for MockRunDelegationService {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }
    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "delegate".into(),
        ))
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
            .push((request.continuation, request.result));
        step_to_result(&self.resume_step)
    }

    async fn cancel(
        &self,
        cancellation: ChildRunCancellation,
    ) -> Result<(), DelegationExecutionError> {
        self.cancelled.lock().unwrap().push(cancellation);
        Ok(())
    }
}

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
            max_steps: 16,
            delegation_limits: Default::default(),
            model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
            ),
            // Native/SDK delegation is authorized by the exact frozen tool
            // descriptor as well as the installed service. Managed snapshots
            // deliberately omit this descriptor, so an empty fixture would be
            // the negative Managed case rather than a valid `agent_run` run.
            tool_descriptors: vec![
                ToolDescriptor::pinned(
                    "test:delegation",
                    DELEGATE_TOOL,
                    "Delegate a Run to another Agent",
                    serde_json::json!({"type":"object"}),
                )
                .with_kind(ToolKind::AgentDelegation)
                .with_recovery(ToolRecoveryPolicy::durable_request()),
            ],
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn configured_runtime(service: Arc<dyn RunDelegationService>) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(DelegateThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_run_delegation(service);
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
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

fn activation_with_regular_tool() -> RunActivation {
    let mut activation = activation();
    activation
        .snapshot
        .resolved_spec
        .tool_descriptors
        .push(ToolDescriptor::pinned(
            "test:external",
            "external_tool",
            "An external-style regular tool",
            serde_json::json!({"type": "object"}),
        ));
    activation
}

/// A resume command correlated to the `Delegation` ticket (its call id is the
/// correlation id), carrying the given user input.
fn resume_command(input: &str) -> ResumeCommand {
    ResumeCommand {
        operation_id: None,
        correlation_id: CALL_ID.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result: ResumeResult::Input(input.to_string()),
        context_messages: Vec::new(),
        now_ms: 0,
    }
}

fn committed_delegation_reference(
    commit: &MemoryCommitCoordinator,
    call_id: &str,
) -> Option<serde_json::Value> {
    let store = Store::rebuild(&commit.committed().state);
    RunDelegations::load(&store)
        .expect("valid delegation registry")
        .and_then(|registry| {
            registry
                .get(
                    &awaken_agent_contract::agent::delegation::DelegationId::for_parent_call(
                        &RunId("run-1".into()),
                        call_id,
                    ),
                )
                .and_then(|relationship| relationship.cancellation_reference.clone())
        })
}

// --- CE-4: dispatch of a delegation tool call ---

#[tokio::test]
async fn delegation_done_folds_delegate_usage_and_feeds_the_reply_back() {
    // Cause/effect graph: C1=the native snapshot publishes the canonical
    // AgentDelegation descriptor, C2=the installed service handles that id, and
    // C3=the child ends with text plus usage. Effects: E1=one child is entered,
    // E2=its reply is the parent ToolResult, E3=its usage is folded once, and
    // E4=the parent reaches NaturalEnd. Decision rule N1: C1+C2+C3 -> E1-E4.
    // Constraints/invariants: the canonical delegation descriptor/service is the
    // sole child path and usage/reply are each folded exactly once.
    let executor = Arc::new(MockRunDelegationService::new(
        Step::Done("delegate replied".to_string(), 5),
        Step::Fail("unused".to_string()),
    ));
    let runtime = configured_runtime(executor.clone());
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
async fn delegation_start_requires_live_parent_attempt_authority() {
    // Cause/effect graph: C1=the provider has produced an authorized delegation
    // call; C2=parent attempt authority is current/lost/down immediately before
    // child start. Effects: E1=start exactly one child and continue the Run;
    // E2=start zero children and return an attempt error. Authority absence is
    // the direct compatibility rule covered by `delegation_done...`.
    //
    // | Rule | Authority sequence        | Effect |
    // | O1   | current,current,current   | E1     |
    // | O2   | current,lost/down         | E2     |
    // Constraints/invariants: the parent claim is checked at child start;
    // lost/unavailable ownership cannot produce any child side effect.
    let current_service = Arc::new(MockRunDelegationService::new(
        Step::Done("child done".into(), 0),
        Step::Fail("unused".into()),
    ));
    let current_runtime = configured_runtime(current_service.clone());
    let current_commit = Arc::new(MemoryCommitCoordinator::new());
    let current_context = RuntimeRunContext::new()
        .with_commit(current_commit)
        .with_ownership(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
        ])));
    assert_eq!(
        current_runtime
            .execute(activation(), current_context)
            .await
            .expect("O1 current authority permits delegation"),
        RunState::Ended(EndCause::NaturalEnd),
        "O1/E1"
    );
    assert_eq!(current_service.started.lock().unwrap().len(), 1, "O1/E1");

    for (label, stale) in [
        ("lost", OwnershipDecision::Lost),
        ("down", OwnershipDecision::Unavailable),
    ] {
        let service = Arc::new(MockRunDelegationService::new(
            Step::Done("must not start".into(), 0),
            Step::Fail("unused".into()),
        ));
        let runtime = configured_runtime(service.clone());
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(MemoryCommitCoordinator::new()))
            .with_ownership(Arc::new(ScriptedOwnership::new([
                OwnershipDecision::Current,
                stale,
            ])));
        runtime
            .execute(activation(), context)
            .await
            .expect_err("O2 stale authority fences delegation start");
        assert!(service.started.lock().unwrap().is_empty(), "O2/E2 {label}");
    }
}

#[tokio::test]
async fn multiple_terminal_child_agents_execute_concurrently_with_distinct_run_identity() {
    let executor = Arc::new(ConcurrentRunDelegationService::new(2));
    let runtime =
        configured_runtime(executor.clone()).with_llm(Arc::new(ParallelDelegatesThenText {
            calls: AtomicUsize::new(0),
        }));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone()),
        ),
    )
    .await
    .expect("two child futures reach the barrier concurrently")
    .expect("parallel delegation completes");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(executor.maximum_active.load(Ordering::SeqCst), 2);

    let store = Store::rebuild(&commit.committed().state);
    let registry = RunDelegations::load(&store)
        .expect("valid relationships")
        .expect("parallel relationship registry");
    let children: Vec<_> = registry.delegations().collect();
    assert_eq!(children.len(), 2);
    assert!(
        children
            .iter()
            .all(|child| child.status == DelegationStatus::Completed)
    );
    assert_ne!(children[0].child_run_id, children[1].child_run_id);
}

/// Decision table:
///
/// | C1: multiple delegation calls | C2: executor opts into terminal parallelism | Effect |
/// |--------------------------------|------------------------------------------------|--------|
/// | true                           | false                                          | calls remain serial |
///
/// The runtime must never infer concurrency from `ToolKind::AgentDelegation`.
#[tokio::test]
async fn terminal_child_agents_without_parallel_capability_execute_serially() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let executor = Arc::new(ObservationRunDelegationService {
        probe: probe.clone(),
        supports_parallel: false,
    });
    let runtime =
        configured_runtime(executor.clone()).with_llm(Arc::new(ParallelDelegatesThenText {
            calls: AtomicUsize::new(0),
        }));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit),
        )
        .await
        .expect("serial delegation batch completes");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(probe.maximum_active.load(Ordering::SeqCst), 1);
}

/// A delegation capability cannot override an unrelated regular tool's explicit
/// serial restriction, so this mixed batch remains serial.
#[tokio::test]
async fn mixed_delegation_and_regular_tool_batch_executes_serially() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let executor = Arc::new(ObservationRunDelegationService {
        probe: probe.clone(),
        supports_parallel: true,
    });
    let runtime = configured_runtime(executor)
        .with_llm(Arc::new(MixedCallsThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(ObservedRawTool {
            probe: probe.clone(),
            concurrency: ToolConcurrency::Serial,
        }));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(
            activation_with_regular_tool(),
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit),
        )
        .await
        .expect("mixed tool batch completes");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        probe.maximum_active.load(Ordering::SeqCst),
        1,
        "delegation parallel capability must not leak to a regular tool"
    );
}

/// Composition rule: execution families do not create separate schedulers.
/// A terminal-only delegation and an ordinary external tool may overlap when
/// delegation supplies terminal-completion support and the ordinary tool allows
/// parallel execution through the shared scheduler.
#[tokio::test]
async fn mixed_delegation_and_parallel_regular_tool_execute_concurrently() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let executor = Arc::new(ObservationRunDelegationService {
        probe: probe.clone(),
        supports_parallel: true,
    });
    let runtime = configured_runtime(executor)
        .with_llm(Arc::new(MixedCallsThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(ObservedRawTool {
            probe: probe.clone(),
            concurrency: ToolConcurrency::Parallel,
        }));
    let outcome = runtime
        .execute(activation_with_regular_tool(), RuntimeRunContext::new())
        .await
        .expect("mixed compatible batch completes");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(probe.maximum_active.load(Ordering::SeqCst), 2);
}

/// Cause/effect rule: if any call requires a durable permission wait, no future
/// from an otherwise parallel-eligible batch may enter the delegation executor.
#[tokio::test]
async fn permission_wait_prevents_parallel_delegation_start() {
    let executor = Arc::new(ConcurrentRunDelegationService::new(2));
    let runtime = configured_runtime(executor.clone())
        .with_llm(Arc::new(ParallelDelegatesThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_gate(Arc::new(SuspendGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new().with_commit(commit))
        .await
        .expect("permission wait is durable");

    assert_eq!(outcome, RunState::Awaiting);
    assert_eq!(executor.active.load(Ordering::SeqCst), 0);
    assert_eq!(executor.maximum_active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn approved_agent_run_reenters_the_delegation_router() {
    let executor = Arc::new(MockRunDelegationService::new(
        Step::Done("approved delegate replied".to_string(), 0),
        Step::Fail("unused".to_string()),
    ));
    let runtime = configured_runtime(executor.clone()).with_gate(Arc::new(SuspendGate));
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
    assert_eq!(ticket.reason(), AwaitReason::ToolPermission);
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
    let executor = Arc::new(MockRunDelegationService::new(
        Step::Done("grandchild replied".to_string(), 0),
        Step::Fail("unused".to_string()),
    ));
    let runtime = configured_runtime(executor.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let parent_origin = DelegationOrigin::root(RunId("root-run".into()), "root-call");
    let context = RuntimeRunContext::new().with_commit(commit);
    let mut activation = activation();
    activation.delegation_origin = Some(parent_origin);

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
    // T3: a delegate that needs more input awaits the parent while the relationship
    // owns the single durable execution reference.
    let handle = serde_json::json!({ "task": "remote-42" });
    let executor = Arc::new(MockRunDelegationService::new(
        Step::Awaiting(handle.clone()),
        Step::Fail("unused".to_string()),
    ));
    let runtime = configured_runtime(executor);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Awaiting);

    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a delegation ticket is committed");
    assert_eq!(ticket.reason(), AwaitReason::Delegation);
    assert_eq!(ticket.call_id(), Some(CALL_ID));
    assert_eq!(
        committed_delegation_reference(commit.as_ref(), CALL_ID),
        Some(handle),
        "the relationship carries the delegate's opaque execution reference"
    );
}

#[tokio::test]
async fn cancelling_an_awaiting_parent_atomically_persists_child_cancel_intent() {
    // Test design — Causes: C1 the parent is durably Awaiting on one child with
    // an opaque cancellation reference; C2 cancellation targets that parent.
    // Effects: the parent commits Cancelled, the relationship commits
    // CancelRequested with the same reference, and the executor receives one
    // exact child cancellation. Constraints/invariants: parent terminal state
    // and child intent share the commit boundary; no process-local waiter owns
    // recovery. Decision rule C1+C2=>all three facts, each exactly once.
    let executor = Arc::new(MockRunDelegationService::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Fail("unused".to_string()),
    ));
    let runtime = configured_runtime(executor.clone());
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
    let relationship = registry
        .delegations()
        .next()
        .expect("one child relationship");
    assert_eq!(
        relationship.cancellation_reference,
        Some(serde_json::json!({ "task": "remote-42" })),
        "the remote cancellation address is durable beside the relationship"
    );
    assert_eq!(
        executor.cancelled.lock().unwrap().as_slice(),
        &[ChildRunCancellation {
            delegation_id: relationship.id.clone(),
            child_run_id: relationship.child_run_id.clone(),
            target_agent_id: relationship.target_agent_id.clone(),
            execution_reference: relationship.cancellation_reference.clone(),
        }],
        "delivery happens after the parent terminal commit"
    );
    let batch = ActiveToolBatch::load(&store)
        .expect("valid committed tool-batch state")
        .expect("the cancelled batch remains durable");
    assert_eq!(batch.phase(), ToolBatchPhase::Finalized);
    assert!(batch.calls().iter().all(|call| matches!(
        call.phase,
        ToolCallPhase::Completed(_) | ToolCallPhase::Indeterminate { .. }
    )));
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none(),
        "the same terminal commit clears the parent's resume ticket"
    );

    assert_eq!(
        runtime
            .reconcile_delegation_cancellations(&ThreadId("thread-1".into()), commit.as_ref())
            .await
            .expect("same process reconciliation is idempotent"),
        0
    );
    assert_eq!(executor.cancelled.lock().unwrap().len(), 1);

    executor.cancelled.lock().unwrap().clear();
    let replacement = configured_runtime(executor.clone());
    assert_eq!(
        replacement
            .reconcile_delegation_cancellations(&ThreadId("thread-1".into()), commit.as_ref())
            .await
            .expect("a replacement process redelivers durable cancellation"),
        1
    );
    assert_eq!(executor.cancelled.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_result_arriving_after_parent_end_is_rejected() {
    // Cause/effect graph: C1=a descriptor-authorized native child is executing,
    // C2=the parent commits Cancelled before that child returns, and C3=the late
    // result is released afterward. Effects: E1=the running attempt is rejected,
    // E2=no result enters the durable inbox, and E3=the relationship remains
    // CancelRequested. Decision rule L1: C1+C2+C3 -> E1+E2+E3. The bounded wait
    // makes loss of C1 fail diagnostically instead of hanging the whole suite.
    // Constraints/invariants: a terminal parent commit fence is absorbing and a
    // late child result cannot enter the durable inbox or revive the relation.
    let executor = Arc::new(LateResultRunDelegationService {
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let runtime = Arc::new(configured_runtime(executor.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    let running = {
        let runtime = runtime.clone();
        let context = context.clone();
        tokio::spawn(async move { runtime.execute(activation(), context).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.reached.notified(),
    )
    .await
    .expect("descriptor-authorized child reaches the executor");
    assert_eq!(
        runtime
            .cancel_run(RunId("run-1".into()), ThreadId("thread-1".into()), context,)
            .await
            .expect("parent ends while child is still running"),
        RunState::Ended(EndCause::Cancelled)
    );
    executor.release.notify_one();
    assert!(
        running.await.expect("child task joins").is_err(),
        "the terminal commit fence rejects a late child-result commit"
    );

    let store = Store::rebuild(&commit.committed().state);
    assert!(
        PendingChildRunResults::load(&store)
            .expect("valid child-result inbox")
            .is_empty()
    );
    assert_eq!(
        RunDelegations::load(&store)
            .expect("valid relationships")
            .expect("relationship registry")
            .delegations()
            .next()
            .expect("one child")
            .status,
        DelegationStatus::CancelRequested
    );
}

#[tokio::test]
async fn retryable_child_crash_recovers_under_the_same_child_run_identity() {
    let executor = Arc::new(CrashOnceRunDelegationService {
        starts: AtomicUsize::new(0),
    });
    let runtime = configured_runtime(executor.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());

    assert!(
        runtime
            .execute(activation(), context.clone())
            .await
            .is_err(),
        "the first child owner interruption leaves the parent recoverable"
    );
    let store = Store::rebuild(&commit.committed().state);
    assert!(matches!(
        ActiveToolBatch::load(&store)
            .expect("valid batch")
            .expect("durable batch")
            .calls()[0]
            .phase,
        ToolCallPhase::Executing { .. }
    ));
    let first_child_id = RunDelegations::load(&store)
        .expect("valid relationships")
        .expect("open relationship")
        .delegations()
        .next()
        .expect("child relationship")
        .child_run_id
        .clone();

    assert_eq!(
        runtime
            .execute(activation(), context)
            .await
            .expect("replacement child owner reconnects"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    let recovered = Store::rebuild(&commit.committed().state);
    let relationship = RunDelegations::load(&recovered)
        .expect("valid recovered relationships")
        .expect("recovered relationship")
        .delegations()
        .next()
        .expect("same child relationship")
        .clone();
    assert_eq!(relationship.child_run_id, first_child_id);
    assert_eq!(relationship.status, DelegationStatus::Completed);
}

#[tokio::test]
async fn delegation_error_feeds_a_model_visible_error_result() {
    // T4: a delegate that cannot run yields a model-visible error result; the run
    // continues rather than aborting.
    let executor = Arc::new(MockRunDelegationService::new(
        Step::Fail("sub-agent unavailable".to_string()),
        Step::Fail("unused".to_string()),
    ));
    let runtime = configured_runtime(executor);
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
    executor: Arc<MockRunDelegationService>,
) -> (Runtime, Arc<MemoryCommitCoordinator>) {
    let runtime = configured_runtime(executor);
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
    // Test design — Causes: C1 a parent is durably Awaiting with one child
    // handle; C2 resume supplies user input; C3 the child returns Done text.
    // Effects: the executor receives the exact handle/input once, the text folds
    // as the delegate Tool result, the ticket clears, and the parent reaches
    // NaturalEnd. Constraints/invariants: the committed ticket/relationship is
    // the sole continuation authority; reply folding cannot leave a stale await.
    // Decision rule RD2=C1+C2+C3=>the complete resume/commit terminal sequence.
    let executor = Arc::new(MockRunDelegationService::new(
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
    assert_eq!(resumed[0].1, ResumeResult::Input("more please".into()));
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
async fn delegation_resume_requires_live_parent_attempt_authority() {
    // Cause/effect graph: C1=an awaiting child has one durable continuation;
    // C2=the replacement parent authority is absent/current/lost/down before
    // resume. Effects: E1=resume the child exactly once; E2=zero child resume
    // calls and an attempt error. The absence compatibility rule is covered by
    // `resuming_a_delegation_done...`.
    //
    // | Rule | Authority | Effect |
    // | O1   | current   | E1     |
    // | O2   | lost/down | E2     |
    // Constraints/invariants: a replacement parent verifies its live claim
    // immediately before the sole child-resume call; stale authority has no effect.
    let current_service = Arc::new(MockRunDelegationService::new(
        Step::Awaiting(serde_json::json!({ "task": "current" })),
        Step::Done("finished".into(), 0),
    ));
    let (current_runtime, current_commit) = await_on_delegation(current_service.clone()).await;
    let current_context = RuntimeRunContext::new()
        .with_commit(current_commit.clone())
        .with_ownership(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Current,
        ])));
    assert_eq!(
        current_runtime
            .resume(
                resume_command("continue"),
                current_commit.as_ref(),
                current_context,
            )
            .await
            .expect("O1 current authority resumes child"),
        RunState::Ended(EndCause::NaturalEnd),
        "O1/E1"
    );
    assert_eq!(
        current_service.resumed_with.lock().unwrap().len(),
        1,
        "O1/E1"
    );

    for (label, stale) in [
        ("lost", OwnershipDecision::Lost),
        ("down", OwnershipDecision::Unavailable),
    ] {
        let service = Arc::new(MockRunDelegationService::new(
            Step::Awaiting(serde_json::json!({ "task": label })),
            Step::Done("must not resume".into(), 0),
        ));
        let (runtime, commit) = await_on_delegation(service.clone()).await;
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_ownership(Arc::new(ScriptedOwnership::new([stale])));
        runtime
            .resume(resume_command("continue"), commit.as_ref(), context)
            .await
            .expect_err("O2 stale authority fences delegation resume");
        assert!(
            service.resumed_with.lock().unwrap().is_empty(),
            "O2/E2 {label}"
        );
    }
}

#[tokio::test]
async fn resuming_a_delegation_that_awaits_again_uses_the_new_handle() {
    // RD3: a resumed delegate that still needs input replaces the relationship's
    // execution reference at the new durable wait boundary.
    let new_handle = serde_json::json!({ "task": "remote-99" });
    let executor = Arc::new(MockRunDelegationService::new(
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
    assert_eq!(ticket.reason(), AwaitReason::Delegation);
    assert_eq!(
        committed_delegation_reference(commit.as_ref(), CALL_ID),
        Some(new_handle),
        "the re-await replaces the relationship execution reference"
    );
}

#[tokio::test]
async fn resuming_a_delegation_error_feeds_an_error_result_and_completes() {
    // RD4: a resumed delegate that fails yields a model-visible error result; the
    // parent drives on rather than aborting.
    let executor = Arc::new(MockRunDelegationService::new(
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
