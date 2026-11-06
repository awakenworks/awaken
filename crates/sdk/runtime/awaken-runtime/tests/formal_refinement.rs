// @formal evidence delegation.cancel_intent
// @formal evidence delegation.relationship_identity
// @formal evidence delegation.result_consume_once
// @formal evidence delegation.stable_child_crash_recovery
// @formal evidence tool.executor_entry_order
// @formal evidence tool.publication_barrier
// @formal evidence tool.recovery_policy
//! Executable Rust -> TLA+ refinement traces.
//!
//! These tests drive the real async Runtime and capture the exact ThreadCommit
//! sequence. `scripts/ci/check_formal.sh` asks the tests to serialize the durable
//! projection; TLC then evaluates every adjacent pair with
//! `ThreadCommitProjection!NextState`. Assertions at executor entry additionally prove
//! that an `Executing` ToolBatch (and, for delegation, the relationship) was
//! committed before an external effect is entered.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::{
    DelegationId, DelegationOrigin, DelegationRegistry, DelegationStatus, RequestDelegation,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::model_request::TokenUsage;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::usage::ThreadUsage;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::{
    ChildRunResult, DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    PendingChildRunResults, RunDelegationService, RunDelegations,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor, ToolKind,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_tool_contract::tool::ToolCall;
use awaken_tool_contract::tool::{
    RawTool, ToolError, ToolOutput, ToolOutputSpiller, ToolRecoveryCapability, ToolRecoveryMode,
    ToolRecoveryPolicy, current_tool_operation_context,
};
use awaken_tool_contract::tool_batch::{
    ActiveToolBatch, ResourceOriginRef, ToolBatch, ToolBatchPhase, ToolCallPhase,
    ToolExecutionDomain,
};
use serde::Serialize;

const RUN_ID: &str = "formal-run";
const THREAD_ID: &str = "formal-thread";
const SNAPSHOT_ID: &str = "formal-snapshot";
const FINGERPRINT: &str = "formal-catalog";

#[derive(Clone)]
struct TracingCoordinator {
    inner: MemoryCommitCoordinator,
    commits: Arc<Mutex<Vec<ThreadCommit>>>,
}

impl Default for TracingCoordinator {
    fn default() -> Self {
        Self {
            inner: MemoryCommitCoordinator::new(awaken_time::system_clock()),
            commits: Arc::default(),
        }
    }
}

impl TracingCoordinator {
    fn commits(&self) -> Vec<ThreadCommit> {
        self.commits.lock().expect("trace lock").clone()
    }

    fn latest_batch(&self) -> Option<ToolBatch> {
        let mut store = Store::new();
        for commit in self.commits() {
            for command in commit.state {
                store.apply(&command);
            }
        }
        ActiveToolBatch::load(&store).expect("valid ToolBatch trace state")
    }

    fn latest_delegation_status(&self, call_id: &str) -> Option<DelegationStatus> {
        let mut store = Store::new();
        for commit in self.commits() {
            for command in commit.state {
                store.apply(&command);
            }
        }
        RunDelegations::load(&store)
            .expect("valid delegation trace state")
            .and_then(|registry| {
                registry
                    .delegations()
                    .find(|entry| entry.parent_call_id == call_id)
                    .map(|entry| entry.status)
            })
    }
}

#[async_trait::async_trait]
impl CommitCoordinator for TracingCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        let observed = commit.clone();
        let record = self.inner.commit(commit).await?;
        self.commits.lock().expect("trace lock").push(observed);
        Ok(record)
    }
}

/// Accept the child-result delivery commit, then simulate an owner crash before
/// the parent can atomically consume it into ToolBatch.
#[derive(Clone)]
struct CrashAfterChildResult {
    trace: TracingCoordinator,
    armed: Arc<std::sync::atomic::AtomicBool>,
    after_child: Option<DelegationId>,
}

#[async_trait::async_trait]
impl CommitCoordinator for CrashAfterChildResult {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            return Err(CommitError::Rejected(
                "simulated crash after child result delivery".to_string(),
            ));
        }
        let arms_crash = match &self.after_child {
            None => commit
                .state
                .iter()
                .any(|command| command.key.0 == PendingChildRunResults::KEY),
            Some(id) => PendingChildRunResults::load(&Store::rebuild(&commit.state))
                .expect("the original producer's delivery is typed")
                .get(id)
                .is_some(),
        };
        let record = self.trace.commit(commit).await?;
        if arms_crash {
            self.armed.store(true, Ordering::SeqCst);
        }
        Ok(record)
    }
}

fn assert_execution_was_committed(trace: &TracingCoordinator, call_id: &str) {
    let batch = trace.latest_batch().expect("a durable batch before invoke");
    let call = batch
        .calls()
        .iter()
        .find(|entry| entry.call.call_id == call_id)
        .expect("the invoked call is durable");
    assert!(
        matches!(call.phase, ToolCallPhase::Executing { .. }),
        "executor entry must follow the Executing commit"
    );
}

fn assert_tool_executor_admission(trace: &TracingCoordinator, call_id: &str) {
    assert_execution_was_committed(trace, call_id);
    let batch = trace.latest_batch().expect("a durable batch before invoke");
    let context = current_tool_operation_context().expect("Runtime scopes executor identity");
    assert_eq!(
        context.operation_id,
        batch.operation_id(call_id),
        "executor entry consumes the committed batch's sole operation identity"
    );
}

struct TraceTool {
    id: String,
    trace: TracingCoordinator,
    capability: ToolRecoveryCapability,
    invocations: Arc<AtomicUsize>,
    preparations: AtomicUsize,
    execution_domain_override: Option<ToolExecutionDomain>,
}

#[async_trait::async_trait]
impl RawTool for TraceTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.capability
    }

    fn execution_domain(&self) -> awaken_tool_contract::tool_batch::ToolExecutionDomain {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        self.execution_domain_override.clone().unwrap_or_default()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        assert_tool_executor_admission(&self.trace, &call.call_id);
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, "ok"))
    }
}

fn trace_tool(id: &str, trace: &TracingCoordinator) -> (Arc<TraceTool>, Arc<AtomicUsize>) {
    trace_tool_with_capability(id, trace, ToolRecoveryCapability::NonRecoverable)
}

fn trace_tool_with_capability(
    id: &str,
    trace: &TracingCoordinator,
    capability: ToolRecoveryCapability,
) -> (Arc<TraceTool>, Arc<AtomicUsize>) {
    let invocations = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(TraceTool {
            id: id.to_string(),
            trace: trace.clone(),
            capability,
            invocations: invocations.clone(),
            preparations: AtomicUsize::new(0),
            execution_domain_override: None,
        }),
        invocations,
    )
}

struct CallsThenText {
    calls: Vec<ToolCall>,
    invocations: AtomicUsize,
}

impl CallsThenText {
    fn new(calls: Vec<ToolCall>) -> Self {
        Self {
            calls,
            invocations: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for CallsThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.invocations.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(self.calls.clone())
        } else {
            AssistantOutput::text("done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct TextOnly;

struct RecoverySpiller;

#[async_trait::async_trait]
impl ToolOutputSpiller for RecoverySpiller {
    async fn spill(
        &self,
        _run_id: &RunId,
        _call_id: &str,
        content: String,
    ) -> Result<String, ToolError> {
        Ok(format!("recovered-preview: {content}"))
    }
}

#[async_trait::async_trait]
impl LlmExecutor for TextOnly {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

struct SuspendGate;

#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(&self, _: &ToolCall, _: &Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "formal-approval".to_string(),
        })
    }
}

struct BlockGate;

#[async_trait::async_trait]
impl ToolGateHook for BlockGate {
    async fn gate(&self, _: &ToolCall, _: &Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::Block {
            reason: "formal policy denial".to_string(),
        })
    }
}

struct AwaitingDelegation {
    trace: TracingCoordinator,
    ended_child: Option<ChildRunResult>,
    resumed_child: Option<ChildRunResult>,
    starts: Arc<Mutex<Vec<RunId>>>,
    resumes: Arc<Mutex<Vec<RunId>>>,
}

impl AwaitingDelegation {
    fn new(trace: TracingCoordinator) -> Self {
        Self {
            trace,
            ended_child: None,
            resumed_child: None,
            starts: Arc::default(),
            resumes: Arc::default(),
        }
    }
}

struct CompletingDelegation {
    trace: TracingCoordinator,
    invocations: Arc<AtomicUsize>,
    usage: ThreadUsage,
}

struct CrashOnceDelegation {
    trace: TracingCoordinator,
    starts: AtomicUsize,
}

#[async_trait::async_trait]
impl RunDelegationService for CrashOnceDelegation {
    fn tool_id(&self) -> &str {
        "agent_run"
    }

    fn target_agent_id(
        &self,
        _: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "researcher".to_string(),
        ))
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        assert_execution_was_committed(&self.trace, &request.origin.parent_call_id);
        if self.starts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(DelegationExecutionError::retryable(
                "formal child owner crash",
            ))
        } else {
            Ok(DelegationStep::Ended {
                text: "child recovered".into(),
                usage: ThreadUsage::default(),
            })
        }
    }

    async fn resume(
        &self,
        _: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("the child never awaits")
    }
}

#[async_trait::async_trait]
impl RunDelegationService for CompletingDelegation {
    fn tool_id(&self) -> &str {
        "agent_run"
    }

    fn target_agent_id(
        &self,
        _: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "researcher".to_string(),
        ))
    }

    fn supports_parallel_completion(&self, _: &serde_json::Value) -> bool {
        true
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        assert_execution_was_committed(&self.trace, &request.origin.parent_call_id);
        assert_eq!(
            self.trace
                .latest_delegation_status(&request.origin.parent_call_id),
            Some(DelegationStatus::Open),
            "recovered child dispatch must reuse the committed relationship"
        );
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(DelegationStep::Ended {
            text: "recovered".to_string(),
            usage: self.usage.clone(),
        })
    }

    async fn resume(
        &self,
        _: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("the recovery scenario completes synchronously")
    }
}

#[async_trait::async_trait]
impl RunDelegationService for AwaitingDelegation {
    fn tool_id(&self) -> &str {
        "agent_run"
    }

    fn target_agent_id(
        &self,
        _: &serde_json::Value,
    ) -> Result<awaken_runtime_contract::snapshot::AgentId, DelegationExecutionError> {
        Ok(awaken_runtime_contract::snapshot::AgentId(
            "researcher".to_string(),
        ))
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        assert_execution_was_committed(&self.trace, &request.origin.parent_call_id);
        assert_eq!(
            self.trace
                .latest_delegation_status(&request.origin.parent_call_id),
            Some(DelegationStatus::Open),
            "child dispatch must follow the relationship commit"
        );
        self.starts
            .lock()
            .expect("child starts")
            .push(request.child_run_id.clone());
        if let Some(result) = &self.ended_child
            && result.child_run_id == request.child_run_id
        {
            return Ok(DelegationStep::Ended {
                text: result.text.clone(),
                usage: result.usage.clone(),
            });
        }
        Ok(DelegationStep::Awaiting {
            continuation: serde_json::json!({"task": request.child_run_id.0}),
        })
    }

    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.resumes
            .lock()
            .expect("child resumes")
            .push(request.child_run_id.clone());
        if let Some(result) = &self.resumed_child
            && result.child_run_id == request.child_run_id
        {
            return Ok(DelegationStep::Ended {
                text: result.text.clone(),
                usage: result.usage.clone(),
            });
        }
        Ok(DelegationStep::Ended {
            text: "done".to_string(),
            usage: ThreadUsage::default(),
        })
    }
}

fn tool_call(call_id: &str, tool_id: &str) -> ToolCall {
    ToolCall {
        call_id: call_id.to_string(),
        tool_id: tool_id.to_string(),
        arguments: serde_json::json!({}),
    }
}

fn snapshot(tool_ids: &[&str]) -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        metadata: Default::default(),
        root_agent_id: AgentId("formal-agent".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fingerprint.clone(),
            instructions: String::new(),
            max_steps: Some(8),
            delegation_limits: Default::default(),
            model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                ModelBinding::new(
                    "provider".to_string(),
                    "model".to_string(),
                    "backend".to_string(),
                ),
            ),
            tool_descriptors: tool_ids
                .iter()
                .map(|id| {
                    let descriptor =
                        ToolDescriptor::pinned("formal", *id, *id, serde_json::json!({}));
                    if *id == "agent_run" {
                        // Cause/effect fixture rule F1: a formal trace asking the
                        // installed delegation service to execute `agent_run`
                        // must also freeze AgentDelegation authority into the
                        // snapshot; ordinary trace tools remain Regular. This
                        // models native/SDK publication while preserving the
                        // Managed no-descriptor negative case elsewhere.
                        descriptor
                            .with_kind(ToolKind::AgentDelegation)
                            .with_recovery(ToolRecoveryPolicy::durable_request())
                    } else {
                        descriptor
                    }
                })
                .collect(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn activation(snapshot: ExecutableAgentSnapshot) -> RunActivation {
    RunActivation::new(
        RunId(RUN_ID.to_string()),
        ThreadId(THREAD_ID.to_string()),
        snapshot,
        vec![Message {
            id: MessageId("formal-input".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("run")],
        }],
    )
}

async fn seed_executing_batch(
    trace: &TracingCoordinator,
    call: ToolCall,
    policy: ToolRecoveryPolicy,
    mut executing_state: Vec<awaken_agent_contract::agent::state::Command>,
) {
    let run_id = RunId(RUN_ID.to_string());
    let thread_id = ThreadId(THREAD_ID.to_string());
    let mut batch = ToolBatch::for_step(run_id.clone(), 0, [(call.clone(), policy)])
        .expect("recovery seed batch");
    trace
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            RunDisposition::running(run_id.clone()),
            true,
            Vec::new(),
            // Current Configured producer input precedes Requested/Executing.
            // This fixture is a positive recovery control, not legacy backfill.
            vec![
                awaken_runtime_contract::permission::RunToolCapability::write(&Some(
                    awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured,
                )),
                ActiveToolBatch::write(&Some(batch.clone())),
            ],
            Vec::new(),
        ))
        .await
        .expect("commit Requested recovery seed");

    assert_eq!(batch.mark_executing(&call.call_id), Ok(1));
    executing_state.insert(0, ActiveToolBatch::write(&Some(batch)));
    trace
        .commit(ThreadCommit::assemble(
            thread_id,
            RunDisposition::running(run_id),
            false,
            Vec::new(),
            executing_state,
            Vec::new(),
        ))
        .await
        .expect("commit Executing recovery seed");
}

fn register_snapshot(runtime: &Runtime, snapshot: &ExecutableAgentSnapshot) {
    runtime.register_snapshot(snapshot.clone());
}

const TRACE_SCHEMA_VERSION: u16 = 2;
const PROJECTION_ID: &str = "thread-commit-durable-v1";
const PROJECTED_FIELDS: [&str; 8] = [
    "run_state",
    "ticket_kind",
    "ticket_call",
    "call_state",
    "attempts",
    "batch_state",
    "link_state",
    "version",
];

#[derive(Serialize)]
struct TraceDocument {
    schema_version: u16,
    projection_id: &'static str,
    projected_fields: [&'static str; 8],
    name: String,
    calls: Vec<String>,
    agent_calls: Vec<String>,
    max_attempts: u16,
    states: Vec<TraceState>,
    transitions: Vec<TraceTransition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TraceState {
    run_state: String,
    ticket_kind: String,
    ticket_call: String,
    call_state: BTreeMap<String, String>,
    attempts: BTreeMap<String, u16>,
    batch_state: String,
    link_state: BTreeMap<String, String>,
    version: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum TransitionId {
    PersistBatch,
    CommitNoop,
    StartOrRetry,
    StartParallelCalls,
    AwaitCall,
    ResumeExecuting,
    RecordReturns,
    CompleteCall,
    CompleteAndFinalize,
    CompleteImmediate,
    CompleteImmediateAndFinalize,
    MarkIndeterminate,
    FinalizeBatch,
    EndRun,
}

#[derive(Debug, Serialize)]
struct TraceTransition {
    id: TransitionId,
    from_version: usize,
    to_version: usize,
}

fn same_except_version(before: &TraceState, after: &TraceState) -> bool {
    let mut normalized = after.clone();
    normalized.version = before.version;
    &normalized == before
}

fn changed_calls<'a>(
    calls: &'a [String],
    before: &TraceState,
    after: &TraceState,
) -> Vec<&'a String> {
    calls
        .iter()
        .filter(|call| {
            before.call_state[*call] != after.call_state[*call]
                || before.attempts[*call] != after.attempts[*call]
                || before.link_state[*call] != after.link_state[*call]
        })
        .collect()
}

/// Classify one real durable commit by its projected before/after shape. TLC
/// independently checks that this ID names an operator which accepts the same
/// state pair; this classifier cannot make an invalid commit refine the model.
fn classify_transition(
    calls: &[String],
    agent_calls: &BTreeSet<String>,
    before: &TraceState,
    after: &TraceState,
) -> TransitionId {
    assert_eq!(
        after.version,
        before.version + 1,
        "each observed ThreadCommit advances the projection version once"
    );

    if before.run_state != "Ended" && after.run_state == "Ended" {
        return TransitionId::EndRun;
    }
    if before.batch_state == "Absent"
        && after.batch_state == "Open"
        && before.run_state == after.run_state
        && before.ticket_kind == after.ticket_kind
        && before.ticket_call == after.ticket_call
        && before.call_state == after.call_state
        && before.attempts == after.attempts
        && before.link_state == after.link_state
    {
        return TransitionId::PersistBatch;
    }
    if before.batch_state == "Open"
        && after.batch_state == "Finalized"
        && before.run_state == after.run_state
        && before.ticket_kind == after.ticket_kind
        && before.ticket_call == after.ticket_call
        && before.call_state == after.call_state
        && before.attempts == after.attempts
        && before.link_state == after.link_state
    {
        return TransitionId::FinalizeBatch;
    }
    if before.run_state == "Running"
        && after.run_state == "Awaiting"
        && before.ticket_kind == "None"
        && after.ticket_kind != "None"
        && after.ticket_call != "no_call"
    {
        return TransitionId::AwaitCall;
    }
    if before.run_state == "Awaiting"
        && after.run_state == "Running"
        && before.ticket_call != "no_call"
        && after.ticket_kind == "None"
        && after.ticket_call == "no_call"
        && before.call_state[&before.ticket_call] == "Awaiting"
        && after.call_state[&before.ticket_call] == "Executing"
    {
        return TransitionId::ResumeExecuting;
    }

    let changed = changed_calls(calls, before, after);
    if !changed.is_empty()
        && changed.iter().all(|call| {
            after.call_state[*call] == "Returned"
                && matches!(
                    before.call_state[*call].as_str(),
                    "Requested" | "Executing" | "Awaiting"
                )
        })
    {
        return TransitionId::RecordReturns;
    }
    if changed.len() > 1
        && changed.iter().all(|call| {
            before.call_state[*call] == "Requested"
                && after.call_state[*call] == "Executing"
                && after.attempts[*call] == before.attempts[*call] + 1
                && after.link_state[*call]
                    == if agent_calls.contains(*call) {
                        "Open"
                    } else {
                        before.link_state[*call].as_str()
                    }
        })
    {
        return TransitionId::StartParallelCalls;
    }
    if changed.len() == 1 {
        let call = changed[0];
        let source = before.call_state[call].as_str();
        let target = after.call_state[call].as_str();
        if target == "Completed" {
            let immediate =
                source == "Requested" || (source == "Returned" && before.attempts[call] == 0);
            return match (immediate, after.batch_state == "Finalized") {
                (true, true) => TransitionId::CompleteImmediateAndFinalize,
                (true, false) => TransitionId::CompleteImmediate,
                (false, true) => TransitionId::CompleteAndFinalize,
                (false, false) => TransitionId::CompleteCall,
            };
        }
        if target == "Indeterminate" {
            return TransitionId::MarkIndeterminate;
        }
        if target == "Executing" && after.attempts[call] == before.attempts[call] + 1 {
            return TransitionId::StartOrRetry;
        }
    }
    if same_except_version(before, after) {
        return TransitionId::CommitNoop;
    }

    panic!(
        "unclassified production transition v{} -> v{}: before={before:?}, after={after:?}",
        before.version, after.version
    );
}

fn ticket_kind(commit: &ThreadCommit) -> String {
    match commit.resume_ticket().map(|ticket| ticket.reason()) {
        None => "None",
        Some(AwaitReason::ToolPermission) => "ToolPermission",
        Some(AwaitReason::Delegation) => "Delegation",
        Some(AwaitReason::ScheduledAction) => "Scheduled",
        Some(_) => "External",
    }
    .to_string()
}

fn phase_name(phase: &ToolCallPhase) -> &'static str {
    match phase {
        ToolCallPhase::Requested => "Requested",
        ToolCallPhase::Executing { .. } => "Executing",
        ToolCallPhase::Awaiting { .. } => "Awaiting",
        ToolCallPhase::Returned(_) => "Returned",
        ToolCallPhase::KnownAborted { .. } => "KnownAborted",
        ToolCallPhase::Completed(_) => "Completed",
        ToolCallPhase::Indeterminate { .. } => "Indeterminate",
    }
}

fn link_name(status: DelegationStatus) -> &'static str {
    match status {
        DelegationStatus::Open => "Open",
        DelegationStatus::Completed => "Completed",
        DelegationStatus::CancelRequested => "CancelRequested",
    }
}

fn project_trace(name: &str, commits: Vec<ThreadCommit>) -> TraceDocument {
    let mut discovery = Store::new();
    let mut calls = Vec::new();
    let mut agent_calls = BTreeSet::new();
    let mut max_attempts = 1;
    for commit in &commits {
        for command in &commit.state {
            discovery.apply(command);
        }
        if calls.is_empty()
            && let Some(batch) =
                ActiveToolBatch::load(&discovery).expect("valid discovered ToolBatch")
        {
            calls = batch
                .calls()
                .iter()
                .map(|entry| entry.call.call_id.clone())
                .collect();
            max_attempts = batch
                .calls()
                .iter()
                .map(|entry| entry.recovery_policy.max_attempts().get())
                .max()
                .unwrap_or(1);
        }
        if let Some(registry) =
            RunDelegations::load(&discovery).expect("valid discovered delegations")
        {
            agent_calls.extend(
                registry
                    .delegations()
                    .map(|entry| entry.parent_call_id.clone()),
            );
        }
    }
    assert!(
        !calls.is_empty(),
        "a formal trace must contain one tool batch"
    );

    let requested = calls
        .iter()
        .map(|call| (call.clone(), "Requested".to_string()))
        .collect();
    let zero_attempts = calls.iter().map(|call| (call.clone(), 0)).collect();
    let absent_links = calls
        .iter()
        .map(|call| (call.clone(), "Absent".to_string()))
        .collect();
    let mut states = vec![TraceState {
        run_state: "Running".to_string(),
        ticket_kind: "None".to_string(),
        ticket_call: "no_call".to_string(),
        call_state: requested,
        attempts: zero_attempts,
        batch_state: "Absent".to_string(),
        link_state: absent_links,
        version: 0,
    }];

    let mut store = Store::new();
    let mut attempts = states[0].attempts.clone();
    for (index, commit) in commits.iter().enumerate() {
        for command in &commit.state {
            store.apply(command);
        }
        let batch = ActiveToolBatch::load(&store).expect("valid projected ToolBatch");
        let mut call_state = BTreeMap::new();
        if let Some(batch) = &batch {
            for call_id in &calls {
                let entry = batch
                    .calls()
                    .iter()
                    .find(|entry| &entry.call.call_id == call_id)
                    .expect("one stable batch in a refinement scenario");
                call_state.insert(call_id.clone(), phase_name(&entry.phase).to_string());
                if let ToolCallPhase::Executing { attempt } = entry.phase {
                    attempts.insert(call_id.clone(), attempt);
                }
            }
        } else {
            for call_id in &calls {
                call_state.insert(call_id.clone(), "Requested".to_string());
            }
        }

        let mut links = calls
            .iter()
            .map(|call| (call.clone(), "Absent".to_string()))
            .collect::<BTreeMap<_, _>>();
        if let Some(registry) = RunDelegations::load(&store).expect("valid projected delegations") {
            for entry in registry.delegations() {
                links.insert(
                    entry.parent_call_id.clone(),
                    link_name(entry.status).to_string(),
                );
            }
        }

        states.push(TraceState {
            run_state: match commit.run_state() {
                RunState::Running => "Running",
                RunState::Awaiting => "Awaiting",
                RunState::Ended(_) => "Ended",
            }
            .to_string(),
            ticket_kind: ticket_kind(commit),
            ticket_call: commit
                .resume_ticket()
                .and_then(|ticket| ticket.call_id().map(str::to_owned))
                .unwrap_or_else(|| "no_call".to_string()),
            call_state,
            attempts: attempts.clone(),
            batch_state: match batch.as_ref().map(ToolBatch::phase) {
                None => "Absent",
                Some(ToolBatchPhase::Open) => "Open",
                Some(ToolBatchPhase::Finalized) => "Finalized",
            }
            .to_string(),
            link_state: links,
            version: index + 1,
        });
    }

    let transitions = states
        .windows(2)
        .map(|pair| TraceTransition {
            id: classify_transition(&calls, &agent_calls, &pair[0], &pair[1]),
            from_version: pair[0].version,
            to_version: pair[1].version,
        })
        .collect();

    TraceDocument {
        schema_version: TRACE_SCHEMA_VERSION,
        projection_id: PROJECTION_ID,
        projected_fields: PROJECTED_FIELDS,
        name: name.to_string(),
        calls,
        agent_calls: agent_calls.into_iter().collect(),
        max_attempts,
        states,
        transitions,
    }
}

fn emit_trace(name: &str, trace: &TracingCoordinator) {
    let document = project_trace(name, trace.commits());
    let final_state = document.states.last().expect("non-empty trace");
    assert_eq!(final_state.run_state, "Ended");
    assert!(
        final_state.batch_state == "Absent" || final_state.batch_state == "Finalized",
        "an ended Run cannot retain an open batch"
    );

    let Some(directory) = std::env::var_os("AWAKEN_FORMAL_TRACE_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    std::fs::create_dir_all(directory).expect("create formal trace directory");
    let path = directory.join(format!("{name}.json"));
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&document).expect("serialize formal trace"),
    )
    .expect("write formal trace");
}

#[tokio::test]
async fn ordinary_parallel_batch_produces_a_refinement_trace() {
    let trace = TracingCoordinator::default();
    let calls = vec![
        tool_call("ordinary_a", "tool_a"),
        tool_call("ordinary_b", "tool_b"),
    ];
    let snapshot = snapshot(&["tool_a", "tool_b"]);
    let (tool_a, _) = trace_tool("tool_a", &trace);
    let (tool_b, _) = trace_tool("tool_b", &trace);
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenText::new(calls)))
        .with_tool(tool_a)
        .with_tool(tool_b);
    register_snapshot(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new().with_commit(Arc::new(trace.clone())),
        )
        .await
        .expect("ordinary trace runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    emit_trace("ordinary_parallel", &trace);
}

#[tokio::test]
async fn policy_blocked_batch_produces_immediate_completion_refinement_transitions() {
    // Gate denials are answered by the Runtime without entering an external
    // executor. Two calls exercise both the intermediate immediate completion
    // and the final immediate completion/publication barrier.
    let trace = TracingCoordinator::default();
    let calls = vec![
        tool_call("blocked_a", "tool_a"),
        tool_call("blocked_b", "tool_b"),
    ];
    let snapshot = snapshot(&["tool_a", "tool_b"]);
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenText::new(calls)))
        .with_gate(Arc::new(BlockGate));
    register_snapshot(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new().with_commit(Arc::new(trace.clone())),
        )
        .await
        .expect("policy-blocked trace runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    emit_trace("policy_blocked_batch", &trace);
}

#[tokio::test]
async fn delegated_preparation_refusal_closes_zero_attempt_returned_relationships() {
    // Cause/effect graph: C1 two published AgentDelegation calls and the exact
    // installed child service; C2 metadata carries origin A while the request
    // borrows origin B; C3 serial / compatible parallel dispatch. C1 stages each
    // relationship before C2 refuses preparation, without admitting an executor.
    // E1 the real commit projects Returned/Open/0; E2 ordered folding closes
    // each relationship without a child-result envelope or invocation; E3 the
    // same two error results publish once before NaturalEnd/Finalized.
    // Decision table: R1 C1+C2+serial => each raw return is checkpointed; the
    // first CompleteImmediate and final CompleteImmediateAndFinalize close Open.
    // R2 C1+C2+parallel => one RecordReturns opens both calls atomically; the
    // same two ordered folds close them. Existing parallel_child_runs owns the
    // successful preparation control; its fixture/default domain is unchanged.
    // The immutable activation, SDK surface, schema and transition IDs remain
    // original. TracingCoordinator/project_trace observe production commits;
    // no constructed trace grants missing relationships or execution attempts.
    for parallel in [false, true] {
        let trace = TracingCoordinator::default();
        let calls = vec![
            tool_call("refused_a", "agent_run"),
            tool_call("refused_b", "agent_run"),
        ];
        let snapshot = snapshot(&["agent_run"]);
        let (mut declaration, tool_invocations) =
            trace_tool_with_capability("agent_run", &trace, ToolRecoveryCapability::DurableRequest);
        Arc::get_mut(&mut declaration)
            .expect("the declaration has not been shared")
            .execution_domain_override = Some(ToolExecutionDomain::Worker {
            resource_origin: Some(ResourceOriginRef::new("executor-origin").unwrap()),
        });
        let serial = Arc::new(AwaitingDelegation::new(trace.clone()));
        let child_invocations = Arc::new(AtomicUsize::new(0));
        let service: Arc<dyn RunDelegationService> = if parallel {
            Arc::new(CompletingDelegation {
                trace: trace.clone(),
                invocations: child_invocations.clone(),
                usage: ThreadUsage::default(),
            })
        } else {
            serial.clone()
        };
        let runtime = Runtime::new()
            .with_llm(Arc::new(CallsThenText::new(calls)))
            .with_tool(declaration.clone())
            .with_run_delegation(service);
        register_snapshot(&runtime, &snapshot);
        let mut context = RuntimeRunContext::new()
            .with_commit(Arc::new(trace.clone()))
            .with_reader(Arc::new(trace.inner.clone()));
        context.resource_origin = Some(ResourceOriginRef::new("borrowed-origin").unwrap());
        assert_eq!(
            runtime
                .execute(activation(snapshot), context)
                .await
                .unwrap(),
            RunState::Ended(EndCause::NaturalEnd),
            "R1/R2 ordinary preparation refusal"
        );
        assert_eq!(declaration.preparations.load(Ordering::SeqCst), 2);
        assert_eq!(tool_invocations.load(Ordering::SeqCst), 0);
        assert_eq!(child_invocations.load(Ordering::SeqCst), 0);
        assert!(serial.starts.lock().unwrap().is_empty());
        assert!(serial.resumes.lock().unwrap().is_empty());

        let name = if parallel {
            "delegated_preparation_refusal_parallel"
        } else {
            "delegated_preparation_refusal_serial"
        };
        let document = project_trace(name, trace.commits());
        assert_eq!(
            document.agent_calls,
            vec![String::from("refused_a"), String::from("refused_b")]
        );
        assert!(
            document.states.iter().all(|state| {
                state.attempts["refused_a"] == 0 && state.attempts["refused_b"] == 0
            })
        );
        let raw = document
            .states
            .iter()
            .find(|state| state.call_state["refused_a"] == "Returned")
            .expect("E1 a real raw-return checkpoint precedes folding");
        assert_eq!(raw.run_state, "Running");
        assert_eq!(raw.batch_state, "Open");
        assert_eq!(raw.ticket_kind, "None");
        assert_eq!(raw.link_state["refused_a"], "Open");
        assert_eq!(
            raw.call_state["refused_b"],
            if parallel { "Returned" } else { "Requested" }
        );
        assert_eq!(
            raw.link_state["refused_b"],
            if parallel { "Open" } else { "Absent" }
        );
        assert_eq!(
            document
                .transitions
                .iter()
                .filter(|transition| transition.id == TransitionId::RecordReturns)
                .count(),
            if parallel { 1 } else { 2 },
            "R1/R2 actual raw-return commit grouping"
        );
        let folds = document
            .transitions
            .iter()
            .filter(|transition| {
                matches!(
                    transition.id,
                    TransitionId::CompleteImmediate | TransitionId::CompleteImmediateAndFinalize
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            folds
                .iter()
                .map(|transition| transition.id)
                .collect::<Vec<_>>(),
            [
                TransitionId::CompleteImmediate,
                TransitionId::CompleteImmediateAndFinalize
            ]
        );
        for (call, fold) in ["refused_a", "refused_b"].into_iter().zip(folds) {
            let before = &document.states[fold.from_version];
            let after = &document.states[fold.to_version];
            assert_eq!(before.call_state[call], "Returned", "E1 {call}");
            assert_eq!(before.link_state[call], "Open", "E1 {call}");
            assert_eq!(after.call_state[call], "Completed", "E2 {call}");
            assert_eq!(after.link_state[call], "Completed", "E2 {call}");
            assert_eq!(after.attempts[call], 0, "E2 no entry {call}");
        }
        let mut observed = Store::new();
        for commit in trace.commits() {
            for command in &commit.state {
                observed.apply(command);
            }
            assert!(
                PendingChildRunResults::load(&observed).unwrap().is_empty(),
                "E2 no durable child envelope at any observed boundary"
            );
        }
        let committed = trace.inner.committed();
        let store = Store::rebuild(&committed.state);
        assert!(PendingChildRunResults::load(&store).unwrap().is_empty());
        let error =
            "persisted tool batch is invalid: executor and borrowed resource origins differ";
        let batch = ActiveToolBatch::load(&store).unwrap().unwrap();
        assert_eq!(batch.phase(), ToolBatchPhase::Finalized);
        for entry in batch.calls() {
            assert_eq!(
                entry.recovery_policy.mode(),
                ToolRecoveryMode::DurableRequest
            );
            assert!(entry.execution_domain.is_none(), "E1 no executor entry");
            let ToolCallPhase::Completed(output) = &entry.phase else {
                panic!("E3 every refusal is completed");
            };
            assert!(output.is_error);
            assert_eq!(output.text(), error);
        }
        let results = committed
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), *is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results,
            [("refused_a", true), ("refused_b", true)],
            "E3 order and cardinality"
        );
        assert!(document.states.last().is_some_and(|state| {
            state.run_state == "Ended"
                && state.batch_state == "Finalized"
                && state.link_state["refused_a"] == "Completed"
                && state.link_state["refused_b"] == "Completed"
        }));
        emit_trace(name, &trace);
    }
}

#[tokio::test]
async fn parallel_child_runs_produce_a_refinement_trace() {
    let trace = TracingCoordinator::default();
    let calls = vec![
        tool_call("agent_parallel_a", "agent_run"),
        tool_call("agent_parallel_b", "agent_run"),
    ];
    let snapshot = snapshot(&["agent_run"]);
    let invocations = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenText::new(calls)))
        .with_run_delegation(Arc::new(CompletingDelegation {
            trace: trace.clone(),
            invocations: invocations.clone(),
            usage: ThreadUsage::default(),
        }));
    register_snapshot(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new()
                .with_commit(Arc::new(trace.clone()))
                .with_reader(Arc::new(trace.inner.clone())),
        )
        .await
        .expect("parallel child trace runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(invocations.load(Ordering::SeqCst), 2);
    emit_trace("parallel_child_runs", &trace);
}

#[tokio::test]
async fn approval_resume_produces_a_refinement_trace() {
    // Causes: C1 the tool is fresh or resumes an exact approval ticket; C2 the
    // Awaiting -> Executing commit succeeds before executor entry. Effects: E1
    // both entries use the same batch/run/step/call operation identity; E2 no
    // legacy resume-only identity survives. Decision rules: fresh+C1 -> E1;
    // approved+C1+C2 -> E1+E2. `TraceTool` checks the identity against the
    // latest committed ActiveToolBatch at the actual executor boundary.
    let trace = TracingCoordinator::default();
    let snapshot = snapshot(&["approved_tool"]);
    let (tool, _) = trace_tool("approved_tool", &trace);
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenText::new(vec![tool_call(
            "approved_call",
            "approved_tool",
        )])))
        .with_tool(tool)
        .with_gate(Arc::new(SuspendGate));
    register_snapshot(&runtime, &snapshot);
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(trace.clone()))
        .with_reader(Arc::new(trace.inner.clone()));

    assert_eq!(
        runtime
            .execute(activation(snapshot), context.clone())
            .await
            .expect("approval trace awaits"),
        RunState::Awaiting
    );
    let ticket = trace
        .inner
        .resume_ticket_for(&RunId(RUN_ID.to_string()))
        .expect("approval ticket");
    let command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0);
    assert_eq!(
        runtime
            .resume(command, &trace.inner, context)
            .await
            .expect("approval trace resumes"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    emit_trace("approval_resume", &trace);
}

#[tokio::test]
async fn delegated_child_cancel_produces_a_refinement_trace() {
    let trace = TracingCoordinator::default();
    let snapshot = snapshot(&["agent_run"]);
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenText::new(vec![tool_call(
            "agent_call",
            "agent_run",
        )])))
        .with_run_delegation(Arc::new(AwaitingDelegation::new(trace.clone())));
    register_snapshot(&runtime, &snapshot);
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(trace.clone()))
        .with_reader(Arc::new(trace.inner.clone()));

    assert_eq!(
        runtime
            .execute(activation(snapshot), context.clone())
            .await
            .expect("delegation trace awaits"),
        RunState::Awaiting
    );
    assert_eq!(
        runtime
            .cancel_run(
                RunId(RUN_ID.to_string()),
                ThreadId(THREAD_ID.to_string()),
                context,
            )
            .await
            .expect("delegation trace cancels"),
        RunState::Ended(EndCause::Cancelled)
    );
    assert_eq!(
        trace.latest_batch().expect("delegation batch").calls()[0]
            .recovery_policy
            .mode(),
        ToolRecoveryMode::DurableRequest,
        "agent_run pins durable-request recovery automatically"
    );
    emit_trace("delegated_child_cancel", &trace);
}

#[tokio::test]
async fn replay_safe_crash_recovery_produces_a_refinement_trace() {
    // Recovery cause/effect rule R-REC: an Executing replay-safe call is invoked
    // again, but its new result must pass the same spiller before Completed state
    // and transcript publication; crash recovery is not an oversized-output bypass.
    let trace = TracingCoordinator::default();
    let policy = ToolRecoveryPolicy::try_new(ToolRecoveryMode::ReplaySafe, 3)
        .expect("non-zero recovery attempt budget");
    let call = tool_call("replay_call", "replay_tool");
    seed_executing_batch(&trace, call, policy.clone(), Vec::new()).await;

    let mut snapshot = snapshot(&["replay_tool"]);
    snapshot.resolved_spec.tool_descriptors[0] = snapshot.resolved_spec.tool_descriptors[0]
        .clone()
        .with_recovery(policy);
    let (tool, invocations) =
        trace_tool_with_capability("replay_tool", &trace, ToolRecoveryCapability::ReplaySafe);
    let runtime = Runtime::new().with_llm(Arc::new(TextOnly)).with_tool(tool);
    register_snapshot(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new()
                .with_commit(Arc::new(trace.clone()))
                .with_reader(Arc::new(trace.inner.clone()))
                .with_tool_output_spiller(Arc::new(RecoverySpiller)),
        )
        .await
        .expect("replay-safe recovery runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let batch = trace
        .latest_batch()
        .expect("recovered batch remains durable");
    assert!(matches!(
        &batch.calls()[0].phase,
        ToolCallPhase::Completed(output) if output.text() == "recovered-preview: ok"
    ));
    emit_trace("replay_safe_recovery", &trace);
}

#[tokio::test]
async fn never_replay_crash_recovery_is_fail_closed() {
    // REC0: the original policy rejects reentry before domain preparation,
    // which can perform lazy Sandbox I/O. Zero invoke alone cannot catch that
    // regression. The same TraceTool records both boundaries, without a new
    // recovery executor or policy preview.
    let trace = TracingCoordinator::default();
    let policy = ToolRecoveryPolicy::default();
    let call = tool_call("never_call", "never_tool");
    seed_executing_batch(&trace, call, policy, Vec::new()).await;

    let snapshot = snapshot(&["never_tool"]);
    let (tool, invocations) = trace_tool("never_tool", &trace);
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextOnly))
        .with_tool(tool.clone());
    register_snapshot(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new()
                .with_commit(Arc::new(trace.clone()))
                .with_reader(Arc::new(trace.inner.clone())),
        )
        .await
        .expect("never-replay recovery fails closed without invoking");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(invocations.load(Ordering::SeqCst), 0);
    assert_eq!(
        tool.preparations.load(Ordering::SeqCst),
        0,
        "REC0 zero metadata I/O"
    );
    let batch = trace
        .latest_batch()
        .expect("recovered batch remains durable");
    assert!(matches!(
        batch.calls()[0].phase,
        ToolCallPhase::Indeterminate { .. }
    ));
    emit_trace("never_replay_recovery", &trace);
}

/// An external service retains its idempotency receipt independently of the
/// Runtime owner. The first invocation applies the effect but loses its reply.
struct ResponseLossTool {
    trace: TracingCoordinator,
    entered: tokio::sync::Notify,
    calls: Mutex<Vec<String>>,
    receipts: Mutex<BTreeSet<String>>,
}

#[async_trait::async_trait]
impl RawTool for ResponseLossTool {
    fn id(&self) -> &str {
        "receipt_tool"
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::Idempotent
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        assert_tool_executor_admission(&self.trace, &call.call_id);
        let first = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(call.call_id.clone());
            assert_eq!(
                self.trace.latest_batch().unwrap().calls()[0].phase,
                ToolCallPhase::Executing {
                    attempt: calls.len() as u16
                },
                "every executor entry follows its own durable attempt"
            );
            calls.len() == 1
        };
        self.receipts.lock().unwrap().insert(call.call_id.clone());
        if first {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::ok(call.call_id, "one external effect"))
    }
}

#[tokio::test]
async fn response_loss_recovery_preserves_effect_identity_and_never_replay() {
    // Causes: C1 Executing is durable; C2 the external service applies its
    // effect; C3 the owner dies before receiving/staging the result; C4 recovery
    // policy is Idempotent or NeverReplay. Effects: E1 C1 precedes every entry;
    // E2 one external receipt survives owner replacement; E3 exact stable-id
    // replay completes with attempt 2; E4 NeverReplay makes no second call and
    // records Indeterminate. Decision rules: RL1=C1+C2+C3+Idempotent -> E1/E2/E3;
    // RL2=C1+C2+C3+NeverReplay -> E1/E2/E4. Constraint: a Notify barrier fixes
    // the failure window, abort/join drops the real async owner, and a new Runtime
    // reconstructs from committed truth. External receipt persistence is a
    // fixture contract, not a proof of arbitrary provider exactly-once behavior.
    for (mode, entries, trace_name) in [
        (ToolRecoveryMode::Idempotent, 2, "idempotent_response_loss"),
        (
            ToolRecoveryMode::NeverReplay,
            1,
            "never_replay_response_loss",
        ),
    ] {
        let trace = TracingCoordinator::default();
        let policy = ToolRecoveryPolicy::try_new(mode, 3).unwrap();
        let mut snapshot = snapshot(&["receipt_tool"]);
        snapshot.resolved_spec.tool_descriptors[0] = snapshot.resolved_spec.tool_descriptors[0]
            .clone()
            .with_recovery(policy);
        let tool = Arc::new(ResponseLossTool {
            trace: trace.clone(),
            entered: tokio::sync::Notify::new(),
            calls: Mutex::new(Vec::new()),
            receipts: Mutex::new(BTreeSet::new()),
        });
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(trace.clone()))
            .with_reader(Arc::new(trace.inner.clone()));
        let owner = Runtime::new()
            .with_llm(Arc::new(CallsThenText::new(vec![tool_call(
                "receipt_call",
                "receipt_tool",
            )])))
            .with_tool(tool.clone());
        register_snapshot(&owner, &snapshot);
        let first_activation = activation(snapshot.clone());
        let first_context = context.clone();
        let task =
            tokio::spawn(async move { owner.execute(first_activation, first_context).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), tool.entered.notified())
            .await
            .expect("external effect reached before the owner is interrupted");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            !context.physical_quiescence.can_acknowledge_return(),
            "aborted owner is not physical termination evidence"
        );
        assert!(matches!(
            trace.latest_batch().unwrap().calls()[0].phase,
            ToolCallPhase::Executing { attempt: 1 }
        ));
        assert_eq!(
            tool.receipts.lock().unwrap().len(),
            1,
            "RL1/RL2 effect survived"
        );

        let replacement = Runtime::new()
            .with_llm(Arc::new(TextOnly))
            .with_tool(tool.clone());
        register_snapshot(&replacement, &snapshot);
        // This trace isolates operation replay AFTER independent physical
        // admission, not a Dispatch crash-recovery authorization. Reusing A's
        // uncertainty latch as B's evidence would model the wrong ownership.
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(trace.clone()))
            .with_reader(Arc::new(trace.inner.clone()));
        assert_eq!(
            replacement
                .execute(activation(snapshot), context)
                .await
                .unwrap(),
            RunState::Ended(EndCause::NaturalEnd)
        );
        assert_eq!(
            *tool.calls.lock().unwrap(),
            vec!["receipt_call"; entries],
            "stable identity"
        );
        assert_eq!(
            tool.receipts.lock().unwrap().len(),
            1,
            "one external effect"
        );
        let batch = trace.latest_batch().unwrap();
        match mode {
            ToolRecoveryMode::Idempotent => assert!(matches!(
                batch.calls()[0].phase,
                ToolCallPhase::Completed(_)
            )),
            ToolRecoveryMode::NeverReplay => assert!(matches!(
                batch.calls()[0].phase,
                ToolCallPhase::Indeterminate { .. }
            )),
            _ => unreachable!(),
        }
        emit_trace(trace_name, &trace);
    }
}

#[tokio::test]
async fn durable_delegation_crash_recovery_reuses_the_child_relationship() {
    let trace = TracingCoordinator::default();
    let call = tool_call("agent_recovery_call", "agent_run");
    let run_id = RunId(RUN_ID.to_string());
    let origin =
        DelegationOrigin::root_for_agent(run_id.clone(), call.call_id.clone(), "formal-agent");
    let mut registry =
        DelegationRegistry::new(run_id, "formal-agent", Vec::new(), 0, Default::default());
    registry
        .request(RequestDelegation {
            id: origin.delegation_id.clone(),
            parent_call_id: call.call_id.clone(),
            target_agent_id: "researcher".to_string(),
            recursive_self: false,
            child_run_id: origin.child_run_id(),
        })
        .expect("seed durable delegation relationship");
    seed_executing_batch(
        &trace,
        call,
        ToolRecoveryPolicy::durable_request(),
        vec![RunDelegations::write(&Some(registry))],
    )
    .await;

    let snapshot = snapshot(&["agent_run"]);
    let invocations = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextOnly))
        .with_run_delegation(Arc::new(CompletingDelegation {
            trace: trace.clone(),
            invocations: invocations.clone(),
            usage: ThreadUsage::default(),
        }));
    register_snapshot(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new()
                .with_commit(Arc::new(trace.clone()))
                .with_reader(Arc::new(trace.inner.clone())),
        )
        .await
        .expect("durable child recovery runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(
        trace.latest_delegation_status("agent_recovery_call"),
        Some(DelegationStatus::Completed)
    );
    emit_trace("durable_delegation_recovery", &trace);
}

#[tokio::test]
async fn closed_delegation_relationship_never_reenters_the_child_executor() {
    // Cause/effect decision table: R1 an Executing durable-request ToolBatch
    // plus its exact Open relationship may recover by entering the child once
    // (covered by durable_delegation_crash_recovery_reuses_the_child_relationship);
    // R2 the same batch plus an exact Completed relationship must produce a
    // fail-closed tool result without entering the child. The production gate
    // reads committed RunDelegations immediately before the external call, so
    // a closed relationship cannot be reopened by recovery control flow.
    let trace = TracingCoordinator::default();
    let call = tool_call("closed_agent_recovery_call", "agent_run");
    let run_id = RunId(RUN_ID.to_string());
    let origin =
        DelegationOrigin::root_for_agent(run_id.clone(), call.call_id.clone(), "formal-agent");
    let mut registry =
        DelegationRegistry::new(run_id, "formal-agent", Vec::new(), 0, Default::default());
    registry
        .request(RequestDelegation {
            id: origin.delegation_id.clone(),
            parent_call_id: call.call_id.clone(),
            target_agent_id: "researcher".to_string(),
            recursive_self: false,
            child_run_id: origin.child_run_id(),
        })
        .expect("seed exact delegation relationship");
    registry
        .complete(&origin.delegation_id)
        .expect("seed closed relationship");
    seed_executing_batch(
        &trace,
        call,
        ToolRecoveryPolicy::durable_request(),
        vec![RunDelegations::write(&Some(registry))],
    )
    .await;

    let snapshot = snapshot(&["agent_run"]);
    let invocations = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextOnly))
        .with_run_delegation(Arc::new(CompletingDelegation {
            trace: trace.clone(),
            invocations: invocations.clone(),
            usage: ThreadUsage::default(),
        }));
    register_snapshot(&runtime, &snapshot);

    assert_eq!(
        runtime
            .execute(
                activation(snapshot),
                RuntimeRunContext::new()
                    .with_commit(Arc::new(trace.clone()))
                    .with_reader(Arc::new(trace.inner.clone())),
            )
            .await
            .expect("closed child recovery fails closed as a tool result"),
        RunState::Ended(EndCause::NaturalEnd),
        "R2"
    );
    assert_eq!(invocations.load(Ordering::SeqCst), 0, "R2");
}

#[tokio::test]
async fn retryable_child_owner_crash_produces_a_refinement_trace() {
    let trace = TracingCoordinator::default();
    let snapshot = snapshot(&["agent_run"]);
    let executor = Arc::new(CrashOnceDelegation {
        trace: trace.clone(),
        starts: AtomicUsize::new(0),
    });
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenText::new(vec![tool_call(
            "child_crash_call",
            "agent_run",
        )])))
        .with_run_delegation(executor.clone());
    register_snapshot(&runtime, &snapshot);
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(trace.clone()))
        .with_reader(Arc::new(trace.inner.clone()));

    assert!(
        runtime
            .execute(activation(snapshot.clone()), context.clone())
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .execute(activation(snapshot), context)
            .await
            .expect("replacement child owner completes"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    emit_trace("child_crash_recovery", &trace);
}

#[tokio::test]
async fn child_result_survives_crash_and_is_consumed_without_reinvocation() {
    // CL9 child ended delivery committed, parent crashed before consume =>
    // recovery uses the complete cached value with zero second child invocation.
    // Every per-model token field must survive; the envelope is consumed once
    // by the original ToolBatch/relationship commit, never republished by lookup.
    let mut usage = ThreadUsage::default();
    usage.record(
        "cached-a",
        TokenUsage {
            prompt_tokens: 2,
            completion_tokens: 3,
            cache_read_tokens: 5,
            cache_creation_tokens: 7,
        },
    );
    usage.record(
        "cached-b",
        TokenUsage {
            prompt_tokens: 11,
            completion_tokens: 13,
            cache_read_tokens: 17,
            cache_creation_tokens: 19,
        },
    );
    let trace = TracingCoordinator::default();
    let call = tool_call("delivered_child_call", "agent_run");
    let run_id = RunId(RUN_ID.to_string());
    let origin =
        DelegationOrigin::root_for_agent(run_id.clone(), call.call_id.clone(), "formal-agent");
    let mut registry =
        DelegationRegistry::new(run_id, "formal-agent", Vec::new(), 0, Default::default());
    registry
        .request(RequestDelegation {
            id: origin.delegation_id.clone(),
            parent_call_id: call.call_id.clone(),
            target_agent_id: "researcher".to_string(),
            recursive_self: false,
            child_run_id: origin.child_run_id(),
        })
        .expect("seed durable delegation relationship");
    seed_executing_batch(
        &trace,
        call,
        ToolRecoveryPolicy::durable_request(),
        vec![RunDelegations::write(&Some(registry))],
    )
    .await;

    let snapshot = snapshot(&["agent_run"]);
    let invocations = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextOnly))
        .with_run_delegation(Arc::new(CompletingDelegation {
            trace: trace.clone(),
            invocations: invocations.clone(),
            usage: usage.clone(),
        }));
    register_snapshot(&runtime, &snapshot);
    let crashing = CrashAfterChildResult {
        trace: trace.clone(),
        armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        after_child: None,
    };

    let first = runtime
        .execute(
            activation(snapshot.clone()),
            RuntimeRunContext::new()
                .with_commit(Arc::new(crashing))
                .with_reader(Arc::new(trace.inner.clone())),
        )
        .await;
    assert!(first.is_err(), "the parent crashes after durable delivery");
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let delivered = trace
        .commits()
        .iter()
        .flat_map(|commit| &commit.state)
        .any(|command| command.key.0 == PendingChildRunResults::KEY);
    assert!(delivered, "the child result reached committed truth");
    let pending = PendingChildRunResults::load(&Store::rebuild(&trace.inner.committed().state))
        .expect("the original producer wrote a valid inbox");
    assert_eq!(
        pending.get(&origin.delegation_id).unwrap().usage,
        usage,
        "CL9 full delivery"
    );
    let delivery_writes = trace
        .commits()
        .iter()
        .flat_map(|commit| &commit.state)
        .filter(|command| command.key.0 == PendingChildRunResults::KEY)
        .count();

    let recovered = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new()
                .with_commit(Arc::new(trace.clone()))
                .with_reader(Arc::new(trace.inner.clone())),
        )
        .await
        .expect("parent consumes the already-delivered child result");
    assert_eq!(recovered, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "recovery must not invoke the child Run a second time"
    );

    let mut store = Store::new();
    for commit in trace.commits() {
        for command in commit.state {
            store.apply(&command);
        }
    }
    assert!(
        PendingChildRunResults::load(&store)
            .expect("valid result inbox")
            .is_empty(),
        "consumption removes the transient delivery envelope"
    );
    assert_eq!(
        trace.latest_delegation_status("delivered_child_call"),
        Some(DelegationStatus::Completed)
    );
    assert_eq!(
        ThreadUsage::from_committed_state(&trace.inner.committed().state),
        usage,
        "CL9 full usage once"
    );
    let writes = trace
        .commits()
        .iter()
        .flat_map(|commit| &commit.state)
        .filter(|command| command.key.0 == PendingChildRunResults::KEY)
        .count();
    assert_eq!(
        writes,
        delivery_writes + 1,
        "CL9: only original consume, no duplicate delivery write"
    );
    emit_trace("durable_child_result_recovery", &trace);
}

#[test]
fn nested_origin_identity_used_by_the_trace_is_stable() {
    let root = DelegationOrigin::root(RunId("root".to_string()), "root_call");
    let nested = DelegationOrigin::nested_for_agent(
        RunId(RUN_ID.to_string()),
        "agent_call",
        root.depth,
        &root.agent_lineage,
        "formal-agent",
    )
    .expect("nested origin");
    assert_eq!(nested.depth, root.depth + 1);
    assert_eq!(nested.child_run_id(), nested.child_run_id());
}

#[tokio::test]
async fn resumed_terminal_state_never_erases_a_later_child_delivery() {
    // Cause/effect graph: C1 one real model batch contains A then B; the
    // existing service awaits A and ends B. C2 A resumes through the real
    // producer and Completed.result_state keeps its original inbox consumption.
    // C3 B's full result is delivered by the real producer in a later commit.
    // C4 the original crash coordinator rejects the next parent-completion
    // commit, then a rebuilt Runtime reads the actual committed prefix.
    // Effects: E1 retained commits establish A-completed before B-delivered;
    // E2 B's entire result survives, with no second child start/resume; E3 its
    // full per-model usage folds once and the envelope/relationship completes;
    // E4 a replay of the consumed A command stutters with NotWaiting.
    // Decision rules CH1/CH2=C1+C2+C3+C4=>E1-E4, with empty/full A usage.
    // CH2 overlaps one model and adds an A-only model: later B rollup must
    // retain every token field once, and final publication cannot rewind it.
    // Constraints: one Run/ToolBatch,
    // stable child identities, real ThreadCommit producer/consumer only, no
    // manually inserted Store/inbox/history or predeclared eligibility. The
    // crash observes typed delivery and writes nothing itself. Existing CL9
    // supplies the simpler delivery-before-consume recovery control.
    for with_a_usage in [false, true] {
        let trace = TracingCoordinator::default();
        let run_id = RunId(RUN_ID.into());
        let call_a = tool_call("chronology-a", "agent_run");
        let call_b = tool_call("chronology-b", "agent_run");
        let id_a = DelegationId::for_parent_call(&run_id, &call_a.call_id);
        let id_b = DelegationId::for_parent_call(&run_id, &call_b.call_id);
        let mut usage = ThreadUsage::default();
        for (model, tokens) in [
            ("chronology-first", [2, 3, 5, 7]),
            ("chronology-second", [11, 13, 17, 19]),
        ] {
            usage.record(
                model,
                TokenUsage {
                    prompt_tokens: tokens[0],
                    completion_tokens: tokens[1],
                    cache_read_tokens: tokens[2],
                    cache_creation_tokens: tokens[3],
                },
            );
        }
        let child_b = ChildRunResult {
            child_run_id: id_b.child_run_id(),
            text: "later child B: exact delivery".into(),
            usage: usage.clone(),
        };
        let mut a_usage = ThreadUsage::default();
        if with_a_usage {
            for (model, tokens) in [
                ("chronology-first", [23, 29, 31, 37]),
                ("chronology-a-only", [41, 43, 47, 53]),
            ] {
                a_usage.record(
                    model,
                    TokenUsage {
                        prompt_tokens: tokens[0],
                        completion_tokens: tokens[1],
                        cache_read_tokens: tokens[2],
                        cache_creation_tokens: tokens[3],
                    },
                );
            }
        }
        let mut expected_usage = usage.clone();
        expected_usage.merge(&a_usage);
        let service = Arc::new(AwaitingDelegation {
            resumed_child: Some(ChildRunResult {
                child_run_id: id_a.child_run_id(),
                text: "done".into(),
                usage: a_usage,
            }),
            ended_child: Some(child_b.clone()),
            ..AwaitingDelegation::new(trace.clone())
        });
        let snapshot = snapshot(&["agent_run"]);
        let runtime = Runtime::new()
            .with_llm(Arc::new(CallsThenText::new(vec![
                call_a.clone(),
                call_b.clone(),
            ])))
            .with_run_delegation(service.clone());
        register_snapshot(&runtime, &snapshot);
        assert_eq!(
            runtime
                .execute(
                    activation(snapshot.clone()),
                    RuntimeRunContext::new()
                        .with_commit(Arc::new(trace.clone()))
                        .with_reader(Arc::new(trace.inner.clone())),
                )
                .await
                .expect("CH1 original model batch awaits A"),
            RunState::Awaiting
        );
        let ticket = trace.inner.resume_ticket_for(&run_id).expect("A ticket");
        assert_eq!(ticket.call_id(), Some(call_a.call_id.as_str()), "CH1");
        let command =
            ResumeCommand::from_ticket(&ticket, ResumeResult::Input("continue A".into()), 0);
        let crashing = CrashAfterChildResult {
            trace: trace.clone(),
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            after_child: Some(id_b.clone()),
        };
        let failed = runtime
            .resume(
                command.clone(),
                &trace.inner,
                RuntimeRunContext::new()
                    .with_commit(Arc::new(crashing))
                    .with_reader(Arc::new(trace.inner.clone())),
            )
            .await
            .expect_err("CH1 crash follows B delivery, before its parent completion");
        assert_eq!(failed.code(), "commit_failed", "CH1 injected boundary");
        assert_eq!(
            *service.starts.lock().unwrap(),
            [id_a.child_run_id(), id_b.child_run_id()],
            "CH1 original starts"
        );
        assert_eq!(
            *service.resumes.lock().unwrap(),
            [id_a.child_run_id()],
            "CH1 only A resumed"
        );
        let before = trace.inner.committed();
        let original_pending = PendingChildRunResults::load(&Store::rebuild(&before.state))
            .expect("CH1 original producer's complete pending value");
        assert_eq!(
            original_pending.get(&id_b),
            Some(&child_b),
            "E1 full delivery"
        );
        let original_registry = RunDelegations::load(&Store::rebuild(&before.state))
            .expect("E1 real registry")
            .expect("E1 registry retained");
        let original_b = original_registry
            .get(&id_b)
            .expect("E1 original B relation");
        assert_eq!(
            original_b.child_run_id, child_b.child_run_id,
            "E1 stable child"
        );
        assert_eq!(
            original_b.status,
            DelegationStatus::Open,
            "E1 unconsumed relation"
        );
        let batch = trace.latest_batch().expect("CH1 original same batch");
        let completed_a = batch
            .calls()
            .iter()
            .find(|entry| entry.call == call_a)
            .unwrap();
        assert!(
            matches!(completed_a.phase, ToolCallPhase::Completed(_)),
            "E1"
        );
        assert!(
            completed_a
                .result_state
                .iter()
                .any(|state| state.key.0 == PendingChildRunResults::KEY),
            "E1 A retains its real earlier consumption"
        );
        // The tool-result journal commits B's raw return with its delivery, so
        // the crash leaves B `Returned`: retained as data, with no state applied,
        // no hook run and nothing published.
        assert!(
            batch.calls().iter().any(|entry| entry.call == call_b
                && matches!(&entry.phase, ToolCallPhase::Returned(output)
                    if output.text() == child_b.text)),
            "E1 B is not consumed"
        );
        let commits = trace.commits();
        let a_completed_at = commits
            .iter()
            .position(|commit| {
                ActiveToolBatch::load(&Store::rebuild(&commit.state))
                    .expect("real batch commit")
                    .is_some_and(|batch| {
                        batch.calls().iter().any(|entry| {
                            entry.call == call_a
                                && matches!(entry.phase, ToolCallPhase::Completed(_))
                        })
                    })
            })
            .expect("E1 retained A completion");
        let b_delivered_at = commits
            .iter()
            .position(|commit| {
                PendingChildRunResults::load(&Store::rebuild(&commit.state))
                    .expect("real delivery commit")
                    .get(&id_b)
                    == Some(&child_b)
            })
            .expect("E1 retained B delivery");
        assert!(a_completed_at < b_delivered_at, "E1 actual commit order");

        // A new Runtime and a fresh committed reader discard all previous local
        // Store state. The observed envelope above is data from the real producer,
        // not a fixture-created permission to invoke or consume it.
        let recovered_runtime = Runtime::new()
            .with_llm(Arc::new(TextOnly))
            .with_run_delegation(service.clone());
        register_snapshot(&recovered_runtime, &snapshot);
        let outcome = recovered_runtime
            .execute(
                activation(snapshot),
                RuntimeRunContext::new()
                    .with_commit(Arc::new(trace.clone()))
                    .with_reader(Arc::new(trace.inner.clone())),
            )
            .await
            .expect("CH1 recovery consumes B's existing delivery");
        assert_eq!(
            *service.starts.lock().unwrap(),
            [id_a.child_run_id(), id_b.child_run_id()],
            "E2 no duplicate child start after A's old result_state"
        );
        assert_eq!(
            *service.resumes.lock().unwrap(),
            [id_a.child_run_id()],
            "E2 no duplicate child resume"
        );
        assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "E3");
        let after = trace.inner.committed();
        assert_eq!(
            ThreadUsage::from_committed_state(&after.state),
            expected_usage,
            "E3 full usage once"
        );
        assert!(
        after
            .messages
            .iter()
            .any(|message| message.role == Role::Tool && message.text_content() == child_b.text),
        "E3 exact B text published"
    );
        assert!(
            PendingChildRunResults::load(&Store::rebuild(&after.state))
                .unwrap()
                .is_empty(),
            "E3 consumed"
        );
        assert_eq!(
            trace.latest_delegation_status(&call_b.call_id),
            Some(DelegationStatus::Completed),
            "E3 same original relationship"
        );
        let commits_after = trace.commits().len();
        let replay = recovered_runtime
            .resume(
                command,
                &trace.inner,
                RuntimeRunContext::new().with_commit(Arc::new(trace.clone())),
            )
            .await
            .expect_err("E4 A ticket was already replaced");
        assert_eq!(replay.code(), "run_not_awaiting", "E4");
        assert_eq!(
            trace.commits().len(),
            commits_after,
            "E4 zero side-effect commit"
        );
        assert_eq!(
            trace.inner.committed().state,
            after.state,
            "E4 no second usage/consume"
        );
        assert_eq!(
            *service.starts.lock().unwrap(),
            [id_a.child_run_id(), id_b.child_run_id()],
            "E4 no duplicate start"
        );
        assert_eq!(
            *service.resumes.lock().unwrap(),
            [id_a.child_run_id()],
            "E4 no duplicate resume"
        );
    }
}

#[tokio::test]
async fn delegation_resume_joins_the_original_ticket_and_batch_before_child_effects() {
    // Causes: C1 the real Runtime/child producer commits a Delegation wait;
    // C2 a retained batch is internally valid but disagrees with that ticket on
    // call id/tool/arguments/wait kind/correlation/phase. R1 C1+!C2 resumes the
    // original child once. R2 C1+C2 rejects before child resume or delivery,
    // leaving the original ticket, Run, messages, state and events unchanged.
    // The permission and tool-only paths' RB2/RB3 cases live in awaiting.rs;
    // this table covers the distinct child-service route, not another validator.
    // Fault injection changes the actual committed projection after a real
    // Awaiting producer; it creates no eligibility flag or replacement reader.
    for fault in [
        "call",
        "tool",
        "arguments",
        "kind",
        "correlation",
        "phase",
        "valid",
    ] {
        let trace = TracingCoordinator::default();
        let service = Arc::new(AwaitingDelegation::new(trace.clone()));
        let call = tool_call("coherent-child", "agent_run");
        let run = RunId(RUN_ID.into());
        let thread = ThreadId(THREAD_ID.into());
        let snapshot = snapshot(&["agent_run"]);
        let runtime = Runtime::new()
            .with_llm(Arc::new(CallsThenText::new(vec![call.clone()])))
            .with_run_delegation(service.clone());
        register_snapshot(&runtime, &snapshot);
        assert_eq!(
            runtime
                .execute(
                    activation(snapshot),
                    RuntimeRunContext::new()
                        .with_commit(Arc::new(trace.clone()))
                        .with_reader(Arc::new(trace.inner.clone())),
                )
                .await
                .unwrap(),
            RunState::Awaiting,
            "C1/{fault}"
        );
        let ticket = trace
            .inner
            .resume_ticket_for(&run)
            .expect("original child wait");
        assert_eq!(ticket.call_id(), Some(call.call_id.as_str()), "C1/{fault}");
        if fault != "valid" {
            let mut value = serde_json::to_value(trace.latest_batch().unwrap()).unwrap();
            let entry = &mut value["calls"][0];
            match fault {
                "call" => entry["call"]["call_id"] = serde_json::json!("other-call"),
                "tool" => entry["call"]["tool_id"] = serde_json::json!("other-tool"),
                "arguments" => entry["call"]["arguments"] = serde_json::json!({"changed":true}),
                "kind" => {
                    entry["phase"]["Awaiting"]["wait"]["kind"] = serde_json::json!("ToolPermission")
                }
                "correlation" => {
                    entry["phase"]["Awaiting"]["wait"]["correlation_id"] =
                        serde_json::json!(format!("{}-changed", ticket.correlation_id))
                }
                "phase" => entry["phase"] = serde_json::json!("Requested"),
                _ => unreachable!(),
            }
            let conflicting: ToolBatch =
                serde_json::from_value(value).expect("C2 remains an internally valid batch");
            trace
                .commit(ThreadCommit::assemble(
                    thread.clone(),
                    RunDisposition::awaiting(ticket.clone()),
                    false,
                    Vec::new(),
                    vec![ActiveToolBatch::write(&Some(conflicting))],
                    Vec::new(),
                ))
                .await
                .unwrap();
        }
        let before = trace.inner.committed();
        let commits_before = trace.commits().len();
        let outcome = runtime
            .resume(
                ResumeCommand::from_ticket(&ticket, ResumeResult::Input("continue".into()), 0),
                &trace.inner,
                RuntimeRunContext::new()
                    .with_commit(Arc::new(trace.clone()))
                    .with_reader(Arc::new(trace.inner.clone())),
            )
            .await;
        if fault == "valid" {
            assert_eq!(
                outcome.unwrap(),
                RunState::Ended(EndCause::NaturalEnd),
                "R1"
            );
            assert_eq!(
                *service.resumes.lock().unwrap(),
                [DelegationId::for_parent_call(&run, &call.call_id).child_run_id()],
                "R1"
            );
        } else {
            assert!(
                service.resumes.lock().unwrap().is_empty(),
                "R2/{fault}: child resume must follow ticket/batch admission"
            );
            assert!(
                outcome
                    .unwrap_err()
                    .to_string()
                    .contains("incoherent tool resume"),
                "R2/{fault}"
            );
            assert_eq!(
                trace.commits().len(),
                commits_before,
                "R2/{fault}: no delivery or resume commit"
            );
            let after = trace.inner.committed();
            assert_eq!(after.state, before.state, "R2/{fault}");
            assert_eq!(after.messages, before.messages, "R2/{fault}");
            assert_eq!(after.events, before.events, "R2/{fault}");
            assert_eq!(
                trace.inner.resume_ticket_for(&run),
                Some(ticket),
                "R2/{fault}"
            );
            assert_eq!(awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView::run_state(&trace.inner, &run), Some(RunState::Awaiting), "R2/{fault}");
        }
        assert_eq!(
            service.starts.lock().unwrap().len(),
            1,
            "one original child/{fault}"
        );
    }
}
