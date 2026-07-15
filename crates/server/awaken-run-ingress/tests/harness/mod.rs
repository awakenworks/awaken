//! Shared runtime harness for the durable-ingress integration tests.
//!
//! It builds a real `Runtime` with deterministic model providers — a plain text
//! model whose runs end naturally, and a tool-then-text model that parks on a
//! gate so the resume path can be driven. The commit coordinator and dispatch
//! store are supplied by each test (in-memory or Postgres), so this harness is
//! storage-agnostic and shared by every suite.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

use std::sync::atomic::AtomicBool;

use awaken_agent_contract::agent::run::Record as RunRecord;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime_contract::metrics::{InferenceMetric, MetricsRecorder};

pub const FP: &str = "catalog-a";
pub const SNAP: &str = "snapshot-1";
pub const THREAD: &str = "thread-1";
pub const TICKET: &str = "ticket-1";

/// Always answers with fixed text — a fresh run ends naturally in one step.
struct TextLlm(&'static str);
#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0.to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Calls `echo` once, then ends with text — drives the park/resume path.
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

struct SuspendGate;
#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(
        &self,
        _c: &PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Suspend {
            ticket_id: TICKET.to_string(),
        }
    }
}

/// Defers the tool call as a ScheduledAction instead of running it inline.
struct ScheduleGate;
#[async_trait::async_trait]
impl ToolGateHook for ScheduleGate {
    async fn gate(
        &self,
        _c: &PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Schedule {
            correlation_id: TICKET.to_string(),
            action_kind: None,
        }
    }
}

pub fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_identity_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            model_candidates: Vec::new(),
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

fn install(runtime: &Runtime) {
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
}

/// A runtime with a plain text model — fresh runs end naturally.
pub fn text_runtime() -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm("done"))));
    install(&runtime);
    runtime
}

/// Echoes the run's user-message text back as the assistant reply, so a test can
/// observe exactly which input reached the model.
struct EchoInputLlm;
#[async_trait::async_trait]
impl LlmExecutor for EchoInputLlm {
    async fn infer(&self, r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let echoed = r
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        Ok(ChatResponse {
            output: AssistantOutput::text(echoed),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A runtime whose model echoes the user input it received — a probe for which
/// messages actually reached the run.
pub fn input_echo_runtime() -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(EchoInputLlm)));
    install(&runtime);
    runtime
}

/// A runtime that parks on a tool gate, exposing the tool-run counter so a test
/// can assert the pending tool runs exactly once on resume.
pub fn tool_runtime() -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(SuspendGate)),
    );
    install(&runtime);
    (runtime, ran)
}

/// A runtime whose gate defers the tool call as a ScheduledAction (ADR-0020),
/// exposing the tool-run counter so a test can assert the deferred action runs.
pub fn schedule_runtime() -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(ScheduleGate)),
    );
    install(&runtime);
    (runtime, ran)
}

/// Always answers with fixed text (a fresh run ends in one step), but counts every
/// inference — so a lease test can assert a run's model was driven *exactly once*
/// even after a stale reclaim tries to re-run it.
struct CountingTextLlm {
    infers: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl LlmExecutor for CountingTextLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.infers.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A text runtime that counts inferences, exposing the counter so a test can prove
/// a settled run is not re-executed on a stale reclaim (exactly-once execution).
pub fn counting_text_runtime() -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let infers = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(CountingTextLlm {
        infers: infers.clone(),
    })));
    install(&runtime);
    (runtime, infers)
}

/// A model that calls `echo` until a tool result is present in the transcript, then
/// ends with text. Unlike [`ToolThenText`] it keys off *transcript content*, not a
/// shared call counter, so a re-execution over a fresh transcript repeats the same
/// tool-then-text arc — exactly what a mid-flight reclaim of a running run triggers.
struct ToolUntilResult;
#[async_trait::async_trait]
impl LlmExecutor for ToolUntilResult {
    async fn infer(&self, r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let has_tool_result = r.messages.iter().any(|m| matches!(m.role, Role::Tool));
        let output = if has_tool_result {
            AssistantOutput::text("all done".to_string())
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "ping"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// An echo tool that counts every invocation and blocks its FIRST invocation on a
/// release gate. A test uses it to freeze one owner mid-step — after it has
/// committed a `Running` fact but before it finishes — while a second owner whose
/// lease has lapsed reclaims and re-drives the same run.
struct BlockingEcho {
    ran: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
    first_seen: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl RawTool for BlockingEcho {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        // Only the first-ever invocation blocks; a later re-drive runs straight
        // through, so the test observes the second (double) execution.
        if !self
            .first_seen
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _permit = self.release.acquire().await;
        }
        Ok(ToolOutput::ok(call.call_id, "echoed: ping"))
    }
}

/// A runtime that runs one inline `echo` tool call then ends with text, but whose
/// tool blocks its first invocation until `release` grants a permit. Returns the
/// runtime and the shared tool-invocation counter, so a lease test can freeze a
/// run mid-flight and observe whether a reclaim re-runs the tool.
pub fn blocking_tool_runtime(
    release: Arc<tokio::sync::Semaphore>,
) -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolUntilResult))
            .with_tool(Arc::new(BlockingEcho {
                ran: ran.clone(),
                release,
                first_seen: std::sync::atomic::AtomicBool::new(false),
            })),
    );
    install(&runtime);
    (runtime, ran)
}

/// Build a pending input for the test thread. The one place the `PendingInput`
/// shape lives, so each suite's convenience builder delegates here.
pub fn pending(
    message_id: &str,
    run: &str,
    correlation: &str,
    result: ResumeResult,
) -> awaken_run_ingress::PendingInput {
    awaken_run_ingress::PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        correlation_id: correlation.to_string(),
        available_at_ms: None,
        result,
    }
}

pub fn activation(run: &str) -> RunActivation {
    activation_on(run, THREAD)
}

/// An activation for an explicit thread, so a routing test can enqueue runs on
/// distinct threads and assert each is driven on its own thread's runtime.
pub fn activation_on(run: &str, thread: &str) -> RunActivation {
    RunActivation {
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(thread.to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        model_access: Default::default(),
    }
}

/// A plain-text runtime wired to a metrics recorder, so a test can assert the
/// worker meters the dispatch lifecycle on the same recorder the runtime uses.
pub fn text_runtime_with_metrics(metrics: Arc<dyn MetricsRecorder>) -> Arc<Runtime> {
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(TextLlm("done")))
            .with_metrics(metrics),
    );
    install(&runtime);
    runtime
}

/// A park-on-gate runtime wired to a metrics recorder, so a test can assert a run
/// that settles `Parked` meters a `parked` settle.
pub fn tool_runtime_with_metrics(
    metrics: Arc<dyn MetricsRecorder>,
) -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(SuspendGate))
            .with_metrics(metrics),
    );
    install(&runtime);
    (runtime, ran)
}

/// A model that emits a `echo` tool call for its first `n` inferences, then ends
/// with text. Paired with [`ScheduleGate`] it lets a run commit *several*
/// consecutive ScheduledAction parks, so a test can drive the worker's
/// perform-scheduled while-loop across more than one hop.
struct ToolNThenText {
    remaining: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolNThenText {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let left = self.remaining.load(Ordering::SeqCst);
        let output = if left > 0 {
            self.remaining.fetch_sub(1, Ordering::SeqCst);
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("call-{left}"),
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

/// A runtime whose gate defers *every* tool call as a ScheduledAction, and whose
/// model schedules `n` tool calls before ending — so a single durable drive parks
/// and performs `n` consecutive ScheduledActions in one `drive_claimed`, exercising
/// the worker's chained perform-scheduled loop. Returns the tool-run counter.
pub fn schedule_n_runtime(n: usize) -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolNThenText {
                remaining: AtomicUsize::new(n),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(ScheduleGate)),
    );
    install(&runtime);
    (runtime, ran)
}

/// A commit boundary that rejects every `commit` while `fail` is set, delegating
/// all *reads* (run record, transcript, waiting ticket) to a shared inner
/// [`MemoryCommitCoordinator`]. It is the fault-injection seam for the worker's
/// genuine-drive-failure path: a real storage failure during `execute`/`resume`/
/// `perform_scheduled` makes the drive return `Err` while committed truth still
/// shows the run non-terminal, so the worker must re-raise (not swallow) and leave
/// the dispatch un-settled for a later retry.
#[derive(Clone)]
pub struct FailingCommit {
    inner: Arc<awaken_runtime::memory::MemoryCommitCoordinator>,
    fail: Arc<AtomicBool>,
}

impl FailingCommit {
    /// Wrap `inner`; commits fail immediately when `fail` is true.
    pub fn new(inner: Arc<awaken_runtime::memory::MemoryCommitCoordinator>, fail: bool) -> Self {
        Self {
            inner,
            fail: Arc::new(AtomicBool::new(fail)),
        }
    }

    /// Flip the injected commit failure on or off.
    pub fn set_failing(&self, failing: bool) {
        self.fail.store(failing, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl CommitCoordinator for FailingCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(CommitError::Rejected("injected commit failure".to_string()));
        }
        self.inner.commit(commit).await
    }
}

impl RunStore for FailingCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        self.inner.get(id)
    }
}

impl ThreadReader for FailingCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.inner.committed_messages(thread_id)
    }
    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        self.inner.waiting_ticket(run_id)
    }
    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        self.inner.committed_state(thread_id)
    }
}

/// A `MetricsRecorder` that counts every dispatch-lifecycle callback, so a test can
/// assert the worker meters a claim, a drive-duration, and a settle (labelled by
/// outcome) exactly once on every drive exit path.
#[derive(Default)]
pub struct RecordingMetrics {
    pub claimed: AtomicUsize,
    pub drives: AtomicUsize,
    pub settled_done: AtomicUsize,
    pub settled_parked: AtomicUsize,
}

impl MetricsRecorder for RecordingMetrics {
    fn record_inference(&self, _metric: InferenceMetric<'_>) {}
    fn record_tool(&self, _tool: &str, _outcome: &str, _duration: std::time::Duration) {}
    fn record_dispatch_claimed(&self) {
        self.claimed.fetch_add(1, Ordering::SeqCst);
    }
    fn record_dispatch_settled(&self, outcome: &str) {
        match outcome {
            "done" => self.settled_done.fetch_add(1, Ordering::SeqCst),
            "parked" => self.settled_parked.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
    }
    fn record_dispatch_drive(&self, _duration: std::time::Duration) {
        self.drives.fetch_add(1, Ordering::SeqCst);
    }
}

/// A dispatch store that injects transient `claim` failures: while `fail_claims` is
/// positive each `claim` decrements it and returns an error, then normal service
/// resumes. Every other operation delegates to the inner [`MemoryDispatchStore`].
/// The seam for proving a daemon/pool drain loop swallows a transient store error
/// and recovers on a later tick rather than dying.
pub struct FlakyDispatchStore {
    inner: Arc<awaken_run_ingress::MemoryDispatchStore>,
    fail_claims: AtomicUsize,
}

impl FlakyDispatchStore {
    /// Wrap a fresh in-memory store that fails its first `fail_claims` claims.
    pub fn new(fail_claims: usize) -> Self {
        Self {
            inner: Arc::new(awaken_run_ingress::MemoryDispatchStore::new()),
            fail_claims: AtomicUsize::new(fail_claims),
        }
    }

    /// How many injected claim failures remain (test introspection).
    pub fn remaining_failures(&self) -> usize {
        self.fail_claims.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl awaken_run_ingress::DispatchQueue for FlakyDispatchStore {
    async fn enqueue_with(
        &self,
        request: awaken_run_ingress::RunExecutionRequest,
        options: awaken_run_ingress::SubmitOptions,
    ) -> Result<(), awaken_run_ingress::DispatchError> {
        self.inner.enqueue_with(request, options).await
    }
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        if self.fail_claims.load(Ordering::SeqCst) > 0 {
            self.fail_claims.fetch_sub(1, Ordering::SeqCst);
            return Err(awaken_run_ingress::DispatchError::Rejected(
                "injected transient claim failure".to_string(),
            ));
        }
        self.inner.claim(owner, lease_ms, now_ms).await
    }
    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, awaken_run_ingress::DispatchError> {
        self.inner
            .renew_lease(run_id, owner, lease_ms, now_ms)
            .await
    }
    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, awaken_run_ingress::DispatchError> {
        self.inner.renew_owned_leases(owner, lease_ms, now_ms).await
    }
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: awaken_run_ingress::DispatchOutcome,
        consumed: &[String],
    ) -> Result<awaken_run_ingress::SettleOutcome, awaken_run_ingress::DispatchError> {
        self.inner.settle(run_id, epoch, outcome, consumed).await
    }
    async fn reap(
        &self,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, awaken_run_ingress::DispatchError> {
        self.inner.reap(max_attempts, now_ms).await
    }
    async fn dead_letters(&self) -> Result<Vec<RunId>, awaken_run_ingress::DispatchError> {
        self.inner.dead_letters().await
    }
    async fn requeue(&self, run_id: &RunId) -> Result<bool, awaken_run_ingress::DispatchError> {
        self.inner.requeue(run_id).await
    }
    async fn cancel(
        &self,
        run_id: &RunId,
    ) -> Result<Option<ThreadId>, awaken_run_ingress::DispatchError> {
        self.inner.cancel(run_id).await
    }
    async fn parked_run(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<RunId>, awaken_run_ingress::DispatchError> {
        self.inner.parked_run(thread_id).await
    }
    async fn purge_dead_letters(&self) -> Result<usize, awaken_run_ingress::DispatchError> {
        self.inner.purge_dead_letters().await
    }
    async fn purge_dead_letters_before(
        &self,
        cutoff_ms: u64,
    ) -> Result<usize, awaken_run_ingress::DispatchError> {
        self.inner.purge_dead_letters_before(cutoff_ms).await
    }
    async fn superseded(&self) -> Result<Vec<RunId>, awaken_run_ingress::DispatchError> {
        self.inner.superseded().await
    }
    async fn list_dispatches(
        &self,
    ) -> Result<Vec<awaken_run_ingress::DispatchSummary>, awaken_run_ingress::DispatchError> {
        self.inner.list_dispatches().await
    }
}

#[async_trait::async_trait]
impl awaken_run_ingress::Inbox for FlakyDispatchStore {
    async fn append(
        &self,
        input: awaken_run_ingress::PendingInput,
    ) -> Result<bool, awaken_run_ingress::DispatchError> {
        self.inner.append(input).await
    }
    async fn list(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<awaken_run_ingress::PendingRecord>, awaken_run_ingress::DispatchError> {
        self.inner.list(thread_id).await
    }
    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<awaken_run_ingress::CasOutcome, awaken_run_ingress::DispatchError> {
        self.inner.retract(message_id, expected_revision).await
    }
    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<awaken_run_ingress::CasOutcome, awaken_run_ingress::DispatchError> {
        self.inner.edit(message_id, expected_revision, result).await
    }
}

#[async_trait::async_trait]
impl awaken_run_ingress::Outbox for FlakyDispatchStore {
    async fn stage(
        &self,
        input: awaken_run_ingress::PendingInput,
    ) -> Result<bool, awaken_run_ingress::DispatchError> {
        self.inner.stage(input).await
    }
    async fn relay(&self) -> Result<usize, awaken_run_ingress::DispatchError> {
        self.inner.relay().await
    }
}

// --- Live Postgres helpers, shared by the live suites -----------------------

use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// The test database URL: `AWAKEN_TEST_DATABASE_URL`, or the local dev container.
pub fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

/// Connect, or return `None` with a skip notice when no Postgres is reachable so
/// the suite still passes on a machine without a database.
pub async fn pool() -> Option<PgPool> {
    match PgPool::connect(&database_url()).await {
        Ok(pool) => Some(pool),
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            None
        }
    }
}

/// The test database URL with `search_path` pinned to `schema`, for the one test
/// that exercises `connect()` (which opens its own pool, so it cannot use
/// `schema_pool`'s `after_connect` hook).
pub fn database_url_in_schema(schema: &str) -> String {
    let base = database_url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-c%20search_path%3D{schema}")
}

/// A pool isolated to a fresh, empty Postgres schema, so parallel tests do not
/// collide on the runtime's fixed table names. The production store takes no
/// table prefix (one runtime is one component); test isolation lives entirely in
/// the test, via `search_path` — it never leaks into the store's API. Returns
/// `None` (skip) when no Postgres is reachable.
pub async fn schema_pool(schema: &'static str) -> Option<PgPool> {
    let admin = pool().await?;
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    admin.close().await;
    PgPoolOptions::new()
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                conn.execute(format!("SET search_path = {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url())
        .await
        .ok()
}

/// Shared spec for revision-guarded pending edit/retract (M3a): every backend
/// must match this behaviour, so the test body lives here once.
pub async fn assert_pending_revision_cas<S: awaken_run_ingress::Inbox>(store: &S) {
    use awaken_run_ingress::{CasOutcome, PendingInput};
    let thread = ThreadId(THREAD.to_string());
    let input = |result| PendingInput {
        message_id: "m1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: thread.clone(),
        correlation_id: TICKET.to_string(),
        available_at_ms: None,
        result,
    };
    store
        .append(input(ResumeResult::Input("a".to_string())))
        .await
        .unwrap();

    let records = store.list(&thread).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision, 1);

    // A stale-revision edit is rejected; the correct revision applies and bumps.
    assert_eq!(
        store
            .edit("m1", 99, ResumeResult::Input("b".to_string()))
            .await
            .unwrap(),
        CasOutcome::RevisionMismatch
    );
    assert_eq!(
        store
            .edit("m1", 1, ResumeResult::Input("b".to_string()))
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    let records = store.list(&thread).await.unwrap();
    assert_eq!(records[0].revision, 2);
    assert_eq!(
        records[0].input.result,
        ResumeResult::Input("b".to_string())
    );

    // Retract is likewise guarded; a stale revision fails, the current one wins.
    assert_eq!(
        store.retract("m1", 1).await.unwrap(),
        CasOutcome::RevisionMismatch
    );
    assert_eq!(store.retract("m1", 2).await.unwrap(), CasOutcome::Applied);
    assert_eq!(store.retract("m1", 2).await.unwrap(), CasOutcome::NotFound);
    assert!(store.list(&thread).await.unwrap().is_empty());
}

/// Shared spec for the cross-thread outbox + relay (M3b): every backend must
/// match. A staged delivery is not visible as pending until relayed; relay is
/// idempotent and moves it to the *target* thread's pending input.
pub async fn assert_cross_thread_outbox<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::PendingInput;
    let target = ThreadId("thread-2".to_string());
    let input = PendingInput {
        message_id: "x1".to_string(),
        run_id: RunId("run-2".to_string()),
        thread_id: target.clone(),
        correlation_id: "c2".to_string(),
        available_at_ms: None,
        result: ResumeResult::Input("hi".to_string()),
    };

    // Staging is idempotent and does not yet appear as pending on the target.
    assert!(store.stage(input.clone()).await.unwrap());
    assert!(!store.stage(input.clone()).await.unwrap());
    assert!(store.list(&target).await.unwrap().is_empty());

    // Relay moves it to the target thread's pending input, and is then drained.
    assert_eq!(store.relay().await.unwrap(), 1);
    let records = store.list(&target).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].input.message_id, "x1");
    assert_eq!(store.relay().await.unwrap(), 0, "the outbox was drained");
}

/// Shared spec for scheduled delivery (M4): a future-dated pending input is not
/// claimable until its time has come; every backend must gate the wake the same.
pub async fn assert_scheduled_due<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, PendingInput, RunExecutionRequest};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    // Claim the fresh run, then park it so it can be woken by a delivery.
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    store
        .settle(&run, 1, DispatchOutcome::Parked, &[])
        .await
        .unwrap();

    // Schedule a delivery for t=1000.
    store
        .append(PendingInput {
            message_id: "sched".to_string(),
            run_id: run.clone(),
            thread_id: ThreadId(THREAD.to_string()),
            correlation_id: TICKET.to_string(),
            available_at_ms: Some(1_000),
            result: ResumeResult::Decision {
                allow: true,
                note: None,
            },
        })
        .await
        .unwrap();

    // Before its time, the run is not claimable; at its time, it wakes with the
    // now-due input in hand.
    assert!(
        store.claim("w", 1_000, 500).await.unwrap().is_none(),
        "a future delivery is not yet claimable"
    );
    let claimed = store
        .claim("w", 1_000, 1_000)
        .await
        .unwrap()
        .expect("a due delivery is claimable");
    assert_eq!(claimed.pending.len(), 1);
    assert_eq!(claimed.pending[0].message_id, "sched");
}

/// Shared spec for the crash-retry budget and dead-letter (M5): a run reclaimed
/// past its budget is dead-lettered and no longer claimed, and `requeue` brings
/// it back. Every backend must match.
pub async fn assert_dead_letter<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // A fresh claim does not spend the budget; each later recovery (expired
    // lease) does. With max_attempts = 2, two recoveries exhaust it.
    assert!(store.claim("w", 100, 0).await.unwrap().is_some());
    assert_eq!(store.reap(2, 200).await.unwrap(), 0, "still within budget");
    assert!(store.claim("w", 100, 200).await.unwrap().is_some());
    assert_eq!(store.reap(2, 400).await.unwrap(), 0);
    assert!(store.claim("w", 100, 400).await.unwrap().is_some());

    // Budget exhausted: reap dead-letters it; it is no longer claimable.
    assert_eq!(store.reap(2, 600).await.unwrap(), 1, "dead-lettered");
    assert!(
        store.claim("w", 100, 700).await.unwrap().is_none(),
        "a dead-lettered run is not claimed"
    );
    assert_eq!(store.dead_letters().await.unwrap(), vec![run.clone()]);

    // Requeue restores it to a fresh budget.
    assert!(store.requeue(&run).await.unwrap());
    assert!(store.dead_letters().await.unwrap().is_empty());
    assert!(
        store.claim("w", 100, 800).await.unwrap().is_some(),
        "a requeued run is claimable again"
    );
}

/// Shared spec for durable cancel: a pending or parked dispatch is cancellable
/// (returns its thread id and is removed); a running one is not. Every backend
/// must match.
pub async fn assert_cancel<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;
    let thread = Some(ThreadId(THREAD.to_string()));

    // A pending run is cancellable and then gone.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        thread
    );
    assert!(store.claim("w", 100, 0).await.unwrap().is_none());
    // Cancelling an unknown run is a no-op.
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        None
    );

    // A running run is not durably cancellable (use live control instead).
    store
        .enqueue(RunExecutionRequest::new(activation("run-2")))
        .await
        .unwrap();
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    assert_eq!(
        store.cancel(&RunId("run-2".to_string())).await.unwrap(),
        None,
        "a running run is not durably cancelled"
    );
    // run-2 finishes so the thread frees for the next run — single-writer-per-thread
    // (ADR-0022) forbids run-3 claiming while run-2 is still in flight.
    store
        .settle(
            &RunId("run-2".to_string()),
            1,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    // A parked run on a thread is resolvable by thread (send_message addressing).
    let thread_id = ThreadId(THREAD.to_string());
    assert!(store.parked_run(&thread_id).await.unwrap().is_none());
    store
        .enqueue(RunExecutionRequest::new(activation("run-3")))
        .await
        .unwrap();
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    store
        .settle(
            &RunId("run-3".to_string()),
            1,
            awaken_run_ingress::DispatchOutcome::Parked,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        store.parked_run(&thread_id).await.unwrap(),
        Some(RunId("run-3".to_string()))
    );
}

/// Shared spec for priority, dedupe, and dead-letter GC. Every backend matches.
pub async fn assert_priority_dedupe_gc<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest, SubmitOptions};
    // Each run on its own thread: priority/dedupe/GC are thread-orthogonal, and
    // single-writer-per-thread (ADR-0022) forbids claiming two runs of one thread at
    // once, which these assertions do.
    let req = |id: &str| RunExecutionRequest::new(activation_on(id, id));

    // Priority: the higher-priority fresh run is claimed first.
    store
        .enqueue_with(req("low"), SubmitOptions::default())
        .await
        .unwrap();
    store
        .enqueue_with(
            req("high"),
            SubmitOptions {
                priority: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "high"
    );
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "low"
    );
    store
        .settle(&RunId("high".to_string()), 1, DispatchOutcome::Done, &[])
        .await
        .unwrap();
    store
        .settle(&RunId("low".to_string()), 1, DispatchOutcome::Done, &[])
        .await
        .unwrap();

    // Dedupe: a second enqueue carrying a live dedupe key is a no-op.
    let key = SubmitOptions {
        dedupe_key: Some("k".to_string()),
        ..Default::default()
    };
    store.enqueue_with(req("d1"), key.clone()).await.unwrap();
    store.enqueue_with(req("d2"), key).await.unwrap();
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "d1"
    );
    assert!(
        store.claim("w", 1_000, 0).await.unwrap().is_none(),
        "the duplicate was not enqueued"
    );
    store
        .settle(&RunId("d1".to_string()), 1, DispatchOutcome::Done, &[])
        .await
        .unwrap();

    // GC: a dead-lettered run is purged.
    store
        .enqueue_with(req("poison"), SubmitOptions::default())
        .await
        .unwrap();
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(store.reap(0, 100).await.unwrap(), 1);
    assert_eq!(
        store.dead_letters().await.unwrap(),
        vec![RunId("poison".to_string())]
    );
    assert_eq!(store.purge_dead_letters().await.unwrap(), 1);
    assert!(store.dead_letters().await.unwrap().is_empty());
}

/// Shared spec for the dispatch query surface (ADR-0025): list_dispatches reports
/// each row's status and attempts. Every backend matches.
pub async fn assert_list_dispatches<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchStatus, RunExecutionRequest};

    // A fresh run is Pending; once claimed it is Running.
    store
        .enqueue(RunExecutionRequest::new(activation("r1")))
        .await
        .unwrap();
    let listed = store.list_dispatches().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, RunId("r1".to_string()));
    assert_eq!(listed[0].status, DispatchStatus::Pending);
    assert_eq!(listed[0].attempt_count, 0);

    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    let listed = store.list_dispatches().await.unwrap();
    assert_eq!(listed[0].status, DispatchStatus::Running);
}

/// Shared spec for the daemon's bulk lease renewal (ADR-0024): renewing an owner's
/// in-flight leases keeps them from being reclaimed. Every backend matches.
pub async fn assert_renew_owned_leases<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;

    // owner-a claims two runs at t=0 with a 100ms lease (expire at 100). Distinct
    // threads: single-writer-per-thread (ADR-0022) means one owner holds at most one
    // in-flight run per thread, so "owner-a holds two leases" needs two threads.
    for run in ["r1", "r2"] {
        store
            .enqueue(RunExecutionRequest::new(activation_on(run, run)))
            .await
            .unwrap();
        assert!(store.claim("owner-a", 100, 0).await.unwrap().is_some());
    }

    // Renewing owner-a's leases at t=60 extends both to 160.
    assert_eq!(
        store.renew_owned_leases("owner-a", 100, 60).await.unwrap(),
        2
    );
    // At t=120 the original lease would have expired, but the renewed one has not.
    assert!(
        store.claim("owner-b", 100, 120).await.unwrap().is_none(),
        "renewed leases are not yet reclaimable"
    );
    // Past the renewed expiry, recovery reclaims.
    assert!(store.claim("owner-b", 100, 200).await.unwrap().is_some());
}

/// Shared spec for the near-expiry heartbeat (ADR-0024, O3): a bulk renewal only
/// touches leases within half a lease of expiring, so a fresh claim — a full lease
/// out — is left untouched and its original lease still expires on schedule. Every
/// backend matches.
pub async fn assert_renew_skips_far_from_expiry<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;

    // owner-a claims r1 at t=0 with a 100ms lease (expires at 100).
    store
        .enqueue(RunExecutionRequest::new(activation("r1")))
        .await
        .unwrap();
    assert!(store.claim("owner-a", 100, 0).await.unwrap().is_some());

    // At t=10 the lease still has 90ms left — more than half the 100ms lease — so
    // the bulk renewal skips it and reports zero renewed.
    assert_eq!(
        store.renew_owned_leases("owner-a", 100, 10).await.unwrap(),
        0,
        "a far-from-expiry lease is not renewed"
    );

    // Because it was left untouched, the original lease still expires at 100, so at
    // t=101 recovery reclaims it — proving the skip did not silently extend it.
    assert!(
        store.claim("owner-b", 100, 101).await.unwrap().is_some(),
        "the skipped lease expired on its original schedule"
    );
}

/// Shared spec for time-windowed dead-letter GC (ADR-0023): GC removes only
/// dead-letters older than the cutoff; younger ones stay. Every backend matches.
pub async fn assert_dead_letter_ttl_gc<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;

    // A run is dead-lettered at t=1000 (claimed with a 1ms lease at t=0, then
    // reaped at budget 0 once the lease has expired).
    store
        .enqueue(RunExecutionRequest::new(activation("poison")))
        .await
        .unwrap();
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(store.reap(0, 1_000).await.unwrap(), 1);
    assert_eq!(
        store.dead_letters().await.unwrap(),
        vec![RunId("poison".to_string())]
    );

    // A GC cutoff before the dead-letter time spares it; a cutoff at/after it purges.
    assert_eq!(store.purge_dead_letters_before(999).await.unwrap(), 0);
    assert_eq!(
        store.dead_letters().await.unwrap().len(),
        1,
        "a younger dead-letter is spared"
    );
    assert_eq!(store.purge_dead_letters_before(1_000).await.unwrap(), 1);
    assert!(store.dead_letters().await.unwrap().is_empty());
}

/// Shared spec for epoch supersession (ADR-0022): a superseding submit abandons
/// the thread's prior parked work; only the newest run stays claimable. Every
/// backend matches.
pub async fn assert_supersession<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest, SubmitOptions};

    // An older run parks on the thread.
    store
        .enqueue(RunExecutionRequest::new(activation("old")))
        .await
        .unwrap();
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    store
        .settle(&RunId("old".to_string()), 1, DispatchOutcome::Parked, &[])
        .await
        .unwrap();

    // A superseding submit on the same thread supersedes the parked run.
    store
        .enqueue_with(
            RunExecutionRequest::new(activation("new")),
            SubmitOptions {
                supersede: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.superseded().await.unwrap(),
        vec![RunId("old".to_string())]
    );

    // Only the newest run is claimable; the superseded parked run is never woken.
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "new"
    );
}

/// Shared spec for the idle-thread inbox (ADR-0021): unbound input is listed for
/// its thread, and a Done settle that consumed it removes it. Every backend matches.
pub async fn assert_idle_thread_inbox<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest};
    use awaken_runtime_contract::resume::ResumeResult;

    // Unbound idle-thread input (empty run/correlation) is listed for the thread.
    let unbound = pending("u1", "", "", ResumeResult::Input("hi".to_string()));
    assert!(store.append(unbound).await.unwrap());
    let listed = store.list(&ThreadId(THREAD.to_string())).await.unwrap();
    assert!(
        listed
            .iter()
            .any(|r| r.input.message_id == "u1" && r.input.run_id.0.is_empty()),
        "the unbound input is listed for its thread"
    );

    // A fresh run drains it: a Done settle that consumed it removes it.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    store.claim("w", 1_000, 0).await.unwrap();
    store
        .settle(
            &RunId("run-1".to_string()),
            1,
            DispatchOutcome::Done,
            &["u1".to_string()],
        )
        .await
        .unwrap();
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .iter()
            .all(|r| r.input.message_id != "u1"),
        "the consumed unbound input is removed on Done"
    );
}

/// Shared spec for lease renewal (the multi-node liveness knob). A run's owner
/// extends its lease so another node's recovery cannot steal it; a non-owner
/// cannot renew; an un-renewed lease still expires. Every backend matches.
pub async fn assert_lease_renewal<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(store.claim("owner-a", 100, 0).await.unwrap().is_some());

    // owner-a renews at t=50 (extends to 150); a recovery claim at t=120 cannot
    // steal it because the lease has not expired.
    assert!(store.renew_lease(&run, "owner-a", 100, 50).await.unwrap());
    assert!(
        store.claim("owner-b", 100, 120).await.unwrap().is_none(),
        "a renewed lease is not yet expired"
    );
    // A non-owner cannot renew.
    assert!(!store.renew_lease(&run, "owner-b", 100, 130).await.unwrap());

    // Once the renewed lease expires, recovery reclaims for the new owner.
    assert_eq!(
        store
            .claim("owner-b", 100, 200)
            .await
            .unwrap()
            .map(|c| c.lease.owner),
        Some("owner-b".to_string())
    );
}

/// Shared spec (every backend must match): the fencing token. A claim bumps the
/// lease epoch monotonically; a settle applies only under the current epoch. A
/// stale owner whose lease lapsed and was re-claimed cannot settle the dispatch out
/// from under the reclaimer — its settle is fenced and changes nothing.
pub async fn assert_settle_fences_stale_epoch<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest, SettleOutcome};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // Owner A claims: the fresh row's epoch bumps 0 -> 1.
    let a = store
        .claim("owner-a", 100, 0)
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch, 1, "the first claim bumps the fence to 1");

    // A's lease lapses; owner B recovers it — the epoch bumps 1 -> 2.
    let b = store
        .claim("owner-b", 100, 200)
        .await
        .unwrap()
        .expect("B reclaims the expired lease");
    assert_eq!(b.lease.owner, "owner-b");
    assert_eq!(
        b.lease.epoch, 2,
        "the recovery re-claim bumps the fence to 2"
    );

    // A wakes and tries to settle under its STALE epoch 1: fenced, nothing changes.
    assert_eq!(
        store
            .settle(&run, a.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced,
        "the stale owner's settle is rejected by the fence"
    );
    // The dispatch is untouched — B (the current owner) still holds a live claim, so
    // a fresh claim before B's lease expires finds nothing runnable.
    assert!(
        store.claim("owner-c", 100, 250).await.unwrap().is_none(),
        "the fenced settle did not delete the row B is running"
    );

    // B settles under the CURRENT epoch 2: applied, the dispatch is removed.
    assert_eq!(
        store
            .settle(&run, b.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "the current owner's settle applies"
    );
    assert!(
        store.claim("owner-c", 100, 300).await.unwrap().is_none(),
        "the run is gone after the applied settle"
    );

    // A re-settle by B (now a stale epoch against a deleted row) is a fenced no-op —
    // idempotent, never a spurious change.
    assert_eq!(
        store
            .settle(&run, b.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced,
        "settling an already-removed dispatch is fenced, not an error"
    );
}

/// Shared spec: dedupe suppresses a duplicate only while a *live* dispatch holds
/// the key — a run that has DEAD-LETTERED no longer blocks a re-submit under the
/// same key (`status <> 'dead_letter'` in the SQL backends, `status !=
/// Status::DeadLetter` in memory). Without this a poison run's key would wedge the
/// work forever: the retry could never be enqueued. Every backend must match.
pub async fn assert_dedupe_ignores_dead_lettered<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{RunExecutionRequest, SubmitOptions};
    let key = || SubmitOptions {
        dedupe_key: Some("k".to_string()),
        ..Default::default()
    };

    // A first run takes the key, is claimed with a 1ms lease, then reaped (budget 0)
    // into the dead-letter status once its lease expires.
    store
        .enqueue_with(RunExecutionRequest::new(activation("run-1")), key())
        .await
        .unwrap();
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(store.reap(0, 100).await.unwrap(), 1, "run-1 dead-lettered");
    assert_eq!(
        store.dead_letters().await.unwrap(),
        vec![RunId("run-1".to_string())]
    );

    // A re-submit under the SAME key is NOT deduped away — the only holder is dead —
    // so run-2 enqueues and is the claimable work (run-1, dead-lettered, is not).
    store
        .enqueue_with(RunExecutionRequest::new(activation("run-2")), key())
        .await
        .unwrap();
    assert_eq!(
        store
            .claim("w", 1_000, 200)
            .await
            .unwrap()
            .expect("the re-submit is claimable, not deduped")
            .request
            .run_id()
            .0,
        "run-2",
    );
}

/// Shared spec (single-writer-per-thread, ADR-0022, on the WAKE path): a parked run
/// with due input is NOT woken while its own thread already has another run in
/// flight — waking it would put two concurrent runs on one thread. The suppressed
/// run becomes claimable only once the in-flight run settles and frees the thread.
/// Every backend must match.
pub async fn assert_wake_suppressed_while_thread_running<S: awaken_run_ingress::Dispatch>(
    store: &S,
) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest};
    use awaken_runtime_contract::resume::ResumeResult;
    let thread = ThreadId(THREAD.to_string());

    // run-1 parks on the thread.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(store.claim("w", 10_000, 0).await.unwrap().is_some());
    store
        .settle(&RunId("run-1".to_string()), 1, DispatchOutcome::Parked, &[])
        .await
        .unwrap();

    // run-2 (same thread) is claimed and left in flight, so the thread is now busy.
    store
        .enqueue(RunExecutionRequest::new(activation("run-2")))
        .await
        .unwrap();
    assert!(
        store.claim("w", 10_000, 1).await.unwrap().is_some(),
        "run-2 claims the free thread"
    );

    // Deliver input that answers run-1's park. run-1 is now wakeable *by input* — but
    // its thread is running run-2, so a claim must NOT wake it (no second run/thread).
    assert!(
        store
            .append(pending(
                "m-wake",
                "run-1",
                TICKET,
                ResumeResult::Input("go".into())
            ))
            .await
            .unwrap()
    );
    assert!(
        store.claim("w", 10_000, 2).await.unwrap().is_none(),
        "the parked run is not woken while its thread already runs another",
    );

    // run-2 settles Done, freeing the thread; now the wake fires and hands run-1 its
    // due input. run-2 was claimed exactly once, so its lease epoch is 1.
    store
        .settle(&RunId("run-2".to_string()), 1, DispatchOutcome::Done, &[])
        .await
        .unwrap();
    let claimed = store
        .claim("w", 10_000, 3)
        .await
        .unwrap()
        .expect("the freed thread lets run-1 wake");
    assert_eq!(claimed.request.run_id().0, "run-1");
    assert_eq!(claimed.pending.len(), 1);
    assert_eq!(claimed.pending[0].message_id, "m-wake");
}

/// G5-T6 (cause-effect graphing): two workers RACE to recover the SAME expired lease.
/// Exactly one wins the epoch bump; the other finds nothing runnable — never double
/// ownership. The in-memory store serializes via its mutex, sqlite/postgres via row
/// locking, so the single-winner invariant holds on every backend.
pub async fn assert_concurrent_recovery_yields_one_winner<S>(store: Arc<S>)
where
    S: awaken_run_ingress::Dispatch + Send + Sync + 'static,
{
    use awaken_run_ingress::RunExecutionRequest;
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    // Owner A claims with a short lease (ttl 100 from t=0); it has lapsed by t=200.
    let a = store
        .claim("owner-a", 100, 0)
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch, 1);

    // Two workers race to recover the one expired lease at t=200.
    let s1 = store.clone();
    let s2 = store.clone();
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { s1.claim("owner-b", 100, 200).await }),
        tokio::spawn(async move { s2.claim("owner-c", 100, 200).await }),
    );
    let winners: Vec<_> = [r1.unwrap().expect("claim b"), r2.unwrap().expect("claim c")]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one worker recovers the expired lease (no double ownership)"
    );
    assert_eq!(
        winners[0].lease.epoch, 2,
        "the recovery re-claim bumps the fence 1 -> 2"
    );
}

/// Shared spec: a `Parked` settle is fenced the same way — a stale owner cannot
/// re-park (and reset the crash-retry budget / clear the lease) behind a reclaimer.
pub async fn assert_parked_settle_fences_stale_epoch<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest, SettleOutcome};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    let a = store
        .claim("owner-a", 100, 0)
        .await
        .unwrap()
        .expect("A claims");
    let b = store
        .claim("owner-b", 100, 200)
        .await
        .unwrap()
        .expect("B reclaims");
    assert!(b.lease.epoch > a.lease.epoch);

    // A's stale Parked settle is fenced: it must not clear B's lease or reset the
    // attempt budget of the row B is actively running.
    assert_eq!(
        store
            .settle(&run, a.lease.epoch, DispatchOutcome::Parked, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced,
    );
    assert!(
        store.claim("owner-c", 100, 250).await.unwrap().is_none(),
        "the fenced Parked settle left B's running claim intact"
    );

    // B parks under the current epoch: applied, and the run is now wakeable.
    assert_eq!(
        store
            .settle(&run, b.lease.epoch, DispatchOutcome::Parked, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied,
    );
}
