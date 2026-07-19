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
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ThreadUsage, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolError, ToolOutput, ToolRecoveryCapability, ToolRecoveryMode, ToolRecoveryPolicy,
};
use awaken_runtime_contract::tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchId, ToolBatchPhase, ToolCallPhase,
};
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

fn assert_execution_was_committed(trace: &TracingCoordinator, call_id: &str) {
    let batch = trace.latest_batch().expect("a durable batch before invoke");
    let call = batch
        .calls
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
    async fn gate(&self, _: &PermissionContext, _: &Store) -> GateOutcome {
        GateOutcome::Suspend {
            ticket_id: "formal-approval".to_string(),
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

#[async_trait::async_trait]
impl DelegationExecutor for CompletingDelegation {
    fn tool_id(&self) -> &str {
        "agent_run"
    }

    fn target_agent_id(&self, _: &serde_json::Value) -> Result<String, DelegationExecutionError> {
        Ok("researcher".to_string())
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
impl DelegationExecutor for AwaitingDelegation {
    fn tool_id(&self) -> &str {
        "agent_run"
    }

    fn target_agent_id(&self, _: &serde_json::Value) -> Result<String, DelegationExecutionError> {
        Ok("researcher".to_string())
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
        root_agent_id: AgentId("formal-agent".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fingerprint.clone(),
            instructions: String::new(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: ModelBinding {
                provider_identity_ref: "provider".to_string(),
                model_ref: "model".to_string(),
                backend_ref: "backend".to_string(),
            },
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
        initiator: None,
        model_ref_override: None,
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
    let mut batch = ToolBatch::new(
        ToolBatchId("formal-recovery-batch".to_string()),
        run_id.clone(),
        [(call.clone(), policy)],
    )
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

fn install(runtime: &Runtime, snapshot: &ExecutableAgentSnapshot) {
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "formal-publication".to_string(),
            fingerprint: snapshot.fingerprint.clone(),
            source_revisions: vec!["formal-revision".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: snapshot.fingerprint.clone(),
                runtime_version: "formal".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("catalog installs");
    runtime.register_snapshot(snapshot.clone());
}

#[derive(Serialize)]
struct TraceDocument {
    name: String,
    calls: Vec<String>,
    agent_calls: Vec<String>,
    max_attempts: u16,
    states: Vec<TraceState>,
}

#[derive(Clone, Serialize)]
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

fn ticket_kind(commit: &ThreadCommit) -> String {
    match commit.resume_ticket().map(|ticket| &ticket.reason) {
        None => "None",
        Some(AwaitReason::ToolPermission) => "Approval",
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
                .calls
                .iter()
                .map(|entry| entry.call.call_id.clone())
                .collect();
            max_attempts = batch
                .calls
                .iter()
                .map(|entry| entry.recovery_policy.max_attempts)
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
                    .calls
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
                .and_then(|ticket| ticket.call_id.clone())
                .unwrap_or_else(|| "no_call".to_string()),
            call_state,
            attempts: attempts.clone(),
            batch_state: match batch.as_ref().map(|batch| batch.phase) {
                None => "Absent",
                Some(ToolBatchPhase::Open) => "Open",
                Some(ToolBatchPhase::Finalized) => "Finalized",
            }
            .to_string(),
            link_state: links,
            version: index + 1,
        });
    }

    TraceDocument {
        name: name.to_string(),
        calls,
        agent_calls: agent_calls.into_iter().collect(),
        max_attempts,
        states,
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
    install(&runtime, &snapshot);

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
    install(&runtime, &snapshot);
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
        .with_delegation_executor(Arc::new(AwaitingDelegation {
            trace: trace.clone(),
        }));
    install(&runtime, &snapshot);
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
        trace.latest_batch().expect("delegation batch").calls[0]
            .recovery_policy
            .mode,
        ToolRecoveryMode::DurableRequest,
        "agent_run pins durable-request recovery automatically"
    );
    emit_trace("delegated_child_cancel", &trace);
}

#[tokio::test]
async fn replay_safe_crash_recovery_produces_a_refinement_trace() {
    let trace = TracingCoordinator::default();
    let policy = ToolRecoveryPolicy {
        mode: ToolRecoveryMode::ReplaySafe,
        max_attempts: 3,
    };
    let call = tool_call("replay_call", "replay_tool");
    seed_executing_batch(&trace, call, policy.clone(), Vec::new()).await;

    let mut snapshot = snapshot(&["replay_tool"]);
    snapshot.resolved_spec.tool_descriptors[0] = snapshot.resolved_spec.tool_descriptors[0]
        .clone()
        .with_recovery(policy);
    let (tool, invocations) =
        trace_tool_with_capability("replay_tool", &trace, ToolRecoveryCapability::ReplaySafe);
    let runtime = Runtime::new().with_llm(Arc::new(TextOnly)).with_tool(tool);
    install(&runtime, &snapshot);

    let outcome = runtime
        .execute(
            activation(snapshot),
            RuntimeRunContext::new()
                .with_commit(Arc::new(trace.clone()))
                .with_reader(Arc::new(trace.inner.clone())),
        )
        .await
        .expect("replay-safe recovery runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let batch = trace
        .latest_batch()
        .expect("recovered batch remains durable");
    assert!(matches!(batch.calls[0].phase, ToolCallPhase::Completed(_)));
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
    install(&runtime, &snapshot);

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
        batch.calls[0].phase,
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
        .with_delegation_executor(Arc::new(CompletingDelegation {
            trace: trace.clone(),
            invocations: invocations.clone(),
        }));
    install(&runtime, &snapshot);

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
