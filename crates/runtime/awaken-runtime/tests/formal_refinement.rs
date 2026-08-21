//! Executable Rust -> TLA+ refinement traces.
//!
//! These tests drive the real async Runtime and capture the exact ThreadCommit
//! sequence. `scripts/ci/check_formal.sh` asks the tests to serialize the durable
//! projection; TLC then evaluates every adjacent pair with
//! `RustCommitSystem!NextState`. Assertions at executor entry additionally prove
//! that an `Executing` ToolBatch (and, for delegation, the relationship) was
//! committed before an external effect is entered.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::{
    DelegationOrigin, DelegationRegistry, DelegationStatus, RequestDelegation,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    PendingChildRunResults, RunDelegationService, RunDelegations,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ThreadUsage, ToolCall,
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
use awaken_runtime_contract::tool::{
    RawTool, ToolError, ToolOutput, ToolOutputSpiller, ToolRecoveryCapability, ToolRecoveryMode,
    ToolRecoveryPolicy,
};
use awaken_runtime_contract::tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchPhase, ToolCallPhase,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use serde::Serialize;

const RUN_ID: &str = "formal-run";
const THREAD_ID: &str = "formal-thread";
const SNAPSHOT_ID: &str = "formal-snapshot";
const FINGERPRINT: &str = "formal-catalog";

#[derive(Clone, Default)]
struct TracingCoordinator {
    inner: MemoryCommitCoordinator,
    commits: Arc<Mutex<Vec<ThreadCommit>>>,
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
}

#[async_trait::async_trait]
impl CommitCoordinator for CrashAfterChildResult {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            return Err(CommitError::Rejected(
                "simulated crash after child result delivery".to_string(),
            ));
        }
        let arms_crash = commit
            .state
            .iter()
            .any(|command| command.key.0 == PendingChildRunResults::KEY);
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

struct TraceTool {
    id: String,
    trace: TracingCoordinator,
    capability: ToolRecoveryCapability,
    invocations: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RawTool for TraceTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.capability
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        assert_execution_was_committed(&self.trace, &call.call_id);
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
    async fn gate(&self, _: &ToolCall, _: &Store) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: "formal-approval".to_string(),
        }
    }
}

struct BlockGate;

#[async_trait::async_trait]
impl ToolGateHook for BlockGate {
    async fn gate(&self, _: &ToolCall, _: &Store) -> GateOutcome {
        GateOutcome::Block {
            reason: "formal policy denial".to_string(),
        }
    }
}

struct AwaitingDelegation {
    trace: TracingCoordinator,
}

struct CompletingDelegation {
    trace: TracingCoordinator,
    invocations: Arc<AtomicUsize>,
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
            usage: ThreadUsage::default(),
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
        Ok(DelegationStep::Awaiting {
            continuation: serde_json::json!({"task": request.child_run_id.0}),
        })
    }

    async fn resume(
        &self,
        _: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
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
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                ModelBinding {
                    provider_identity_ref: "provider".to_string(),
                    model_ref: "model".to_string(),
                    backend_ref: "backend".to_string(),
                },
            ),
            tool_descriptors: tool_ids
                .iter()
                .map(|id| ToolDescriptor::pinned("formal", *id, *id, serde_json::json!({})))
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
    RunActivation {
        run_id: RunId(RUN_ID.to_string()),
        thread_id: ThreadId(THREAD_ID.to_string()),
        snapshot,
        input: vec![Message {
            id: MessageId("formal-input".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("run")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
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
            vec![ActiveToolBatch::write(&Some(batch.clone()))],
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
    StartParallelDelegations,
    AwaitCall,
    ResumeExecuting,
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
    if changed.len() > 1
        && changed.len() == agent_calls.len()
        && changed.iter().all(|call| {
            agent_calls.contains(*call)
                && before.call_state[*call] == "Requested"
                && after.call_state[*call] == "Executing"
                && after.attempts[*call] == before.attempts[*call] + 1
        })
    {
        return TransitionId::StartParallelDelegations;
    }
    if changed.len() == 1 {
        let call = changed[0];
        let source = before.call_state[call].as_str();
        let target = after.call_state[call].as_str();
        if target == "Completed" {
            return match (source == "Requested", after.batch_state == "Finalized") {
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
        .with_run_delegation(Arc::new(AwaitingDelegation {
            trace: trace.clone(),
        }));
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
    let trace = TracingCoordinator::default();
    let policy = ToolRecoveryPolicy::default();
    let call = tool_call("never_call", "never_tool");
    seed_executing_batch(&trace, call, policy, Vec::new()).await;

    let snapshot = snapshot(&["never_tool"]);
    let (tool, invocations) = trace_tool("never_tool", &trace);
    let runtime = Runtime::new().with_llm(Arc::new(TextOnly)).with_tool(tool);
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
    let batch = trace
        .latest_batch()
        .expect("recovered batch remains durable");
    assert!(matches!(
        batch.calls()[0].phase,
        ToolCallPhase::Indeterminate { .. }
    ));
    emit_trace("never_replay_recovery", &trace);
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
        }));
    register_snapshot(&runtime, &snapshot);
    let crashing = CrashAfterChildResult {
        trace: trace.clone(),
        armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
