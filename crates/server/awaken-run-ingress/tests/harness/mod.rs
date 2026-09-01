//! Shared runtime harness for the durable-ingress integration tests.
//!
//! It builds a real `Runtime` with deterministic model providers — a plain text
//! model whose runs end naturally, and a tool-then-text model that awaits on a
//! gate so the resume path can be driven. The commit coordinator and dispatch
//! store are supplied by each test (in-memory or Postgres), so this harness is
//! storage-agnostic and shared by every suite.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress_testkit::ConformanceClock;
use awaken_runtime::Runtime;
#[cfg(feature = "test-support")]
use awaken_runtime_contract::CredentialRealizationCapabilities;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

use std::sync::atomic::AtomicBool;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::run::Record as RunRecord;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_runtime_contract::metrics::{InferenceMetric, MetricsRecorder};

pub const FP: &str = "catalog-a";
pub const SNAP: &str = "snapshot-1";
pub const THREAD: &str = "thread-1";
pub const TICKET: &str = "ticket-1";

/// One deterministic edge clock shared by claim, renewal, ownership checks, and
/// settlement in a direct Worker drive.
pub fn clock(now_ms: u64) -> Arc<dyn awaken_run_ingress::Clock> {
    Arc::new(awaken_run_ingress::ManualClock::new(now_ms))
}

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

/// Calls `echo` once, then ends with text — drives the await/resume path.
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
        _c: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::RequireConfirmation {
            correlation_id: TICKET.to_string(),
        }
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
            correlation_id: TICKET.to_string(),
            action_kind: None,
        }
    }
}

pub fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        metadata: Default::default(),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
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

/// A runtime with a plain text model — fresh runs end naturally.
pub fn text_runtime() -> Arc<Runtime> {
    Arc::new(Runtime::new().with_llm(Arc::new(TextLlm("done"))))
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
    Arc::new(Runtime::new().with_llm(Arc::new(EchoInputLlm)))
}

/// A runtime that awaits on a tool gate, exposing the tool-run counter so a test
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
        // Only a tool result after the latest User input belongs to this Run.
        // Looking across the whole Thread would make a prior Run's tool result
        // incorrectly skip execution for a later sequential Run.
        let has_tool_result = r
            .messages
            .iter()
            .rev()
            .take_while(|message| !matches!(message.role, Role::User))
            .any(|message| matches!(message.role, Role::Tool));
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
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
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
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        self.entered.notify_one();
        // Only the first-ever invocation blocks; a later re-drive runs straight
        // through, so the test observes the second (double) execution.
        if !self
            .first_seen
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let _permit = self.release.acquire().await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
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
    let (runtime, ran, _entered) = blocking_tool_runtime_with_entry_signal(release);
    (runtime, ran)
}

/// The same one-authority blocking runtime with explicit physical-concurrency
/// counters for the same-Thread executor-admission specification.
pub fn concurrency_tracking_tool_runtime(
    release: Arc<tokio::sync::Semaphore>,
) -> (
    Arc<Runtime>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (runtime, ran, _entered, active, maximum) = blocking_tool_runtime_parts(release);
    (runtime, ran, active, maximum)
}

/// The observable form of [`blocking_tool_runtime`]. The entry signal is a
/// causal synchronization point for tests that must inspect state while the
/// first tool invocation is blocked; unlike polling the counter, it is not
/// sensitive to executor scheduling under a fully loaded workspace test run.
pub fn blocking_tool_runtime_with_entry_signal(
    release: Arc<tokio::sync::Semaphore>,
) -> (Arc<Runtime>, Arc<AtomicUsize>, Arc<tokio::sync::Notify>) {
    let (runtime, ran, entered, _active, _maximum) = blocking_tool_runtime_parts(release);
    (runtime, ran, entered)
}

type BlockingToolRuntimeParts = (
    Arc<Runtime>,
    Arc<AtomicUsize>,
    Arc<tokio::sync::Notify>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
);

fn blocking_tool_runtime_parts(release: Arc<tokio::sync::Semaphore>) -> BlockingToolRuntimeParts {
    let ran = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolUntilResult))
            .with_tool(Arc::new(BlockingEcho {
                ran: ran.clone(),
                active: active.clone(),
                maximum: maximum.clone(),
                entered: entered.clone(),
                release,
                first_seen: std::sync::atomic::AtomicBool::new(false),
            })),
    );
    (runtime, ran, entered, active, maximum)
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
        context_messages: Vec::new(),
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
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

/// A plain-text runtime wired to a metrics recorder, so a test can assert the
/// worker meters the dispatch lifecycle on the same recorder the runtime uses.
pub fn text_runtime_with_metrics(metrics: Arc<dyn MetricsRecorder>) -> Arc<Runtime> {
    Arc::new(
        Runtime::new()
            .with_llm(Arc::new(TextLlm("done")))
            .with_metrics(metrics),
    )
}

/// A await-on-gate runtime wired to a metrics recorder, so a test can assert a run
/// that settles `Awaiting` meters a `awaiting` settle.
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
    (runtime, ran)
}

/// A model that emits a `echo` tool call for its first `n` inferences, then ends
/// with text. Paired with [`ScheduleGate`] it lets a run commit *several*
/// consecutive ScheduledAction awaits, so a test can drive the worker's
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
/// model schedules `n` tool calls before ending — so a single durable drive awaits
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
    (runtime, ran)
}

/// A commit boundary that rejects every `commit` while `fail` is set, delegating
/// all *reads* (run record, transcript, awaiting ticket) to a shared inner
/// [`MemoryCommitCoordinator`]. It is the fault-injection seam for the worker's
/// genuine-drive-failure path: a real storage failure during `execute`/`resume`/
/// `perform_scheduled` makes the drive return `Err` while committed truth still
/// shows the run non-terminal, so the worker must re-raise (not swallow) and leave
/// the dispatch un-settled for a later retry.
#[derive(Clone)]
pub struct FailingCommit {
    inner: Arc<awaken_store_inmem::MemoryCommitCoordinator>,
    fail: Arc<AtomicBool>,
}

impl FailingCommit {
    /// Wrap `inner`; commits fail immediately when `fail` is true.
    pub fn new(inner: Arc<awaken_store_inmem::MemoryCommitCoordinator>, fail: bool) -> Self {
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

impl CommittedThreadView for FailingCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.inner.committed_messages(thread_id)
    }
    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.inner.run(run_id)
    }
    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.inner.latest_run(thread_id)
    }
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.inner.resume_ticket(run_id)
    }
    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.inner.open_wait_for_thread(thread_id)
    }
    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        self.inner.committed_state(thread_id)
    }
}

#[async_trait::async_trait]
impl RunRecoverySource for FailingCommit {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.inner
            .recovery_snapshot(thread_id, claimed_run_id)
            .await
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
    pub settled_awaiting: AtomicUsize,
    pub queue_depth: AtomicU64,
    pub recovered: AtomicUsize,
    pub commits_applied: AtomicUsize,
    pub fenced: AtomicUsize,
    pub in_flight: AtomicI64,
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
            "awaiting" => self.settled_awaiting.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
    }
    fn record_dispatch_drive(&self, _duration: std::time::Duration) {
        self.drives.fetch_add(1, Ordering::SeqCst);
    }
    fn record_dispatch_queue_depth(&self, depth: u64) {
        self.queue_depth.store(depth, Ordering::SeqCst);
    }
    fn record_dispatch_recovered(&self) {
        self.recovered.fetch_add(1, Ordering::SeqCst);
    }
    fn record_dispatch_commit(&self, outcome: &str, _duration: std::time::Duration) {
        if outcome == "applied" {
            self.commits_applied.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn record_dispatch_fenced(&self) {
        self.fenced.fetch_add(1, Ordering::SeqCst);
    }
    fn record_dispatch_in_flight(&self, delta: i64) {
        self.in_flight.fetch_add(delta, Ordering::SeqCst);
    }
}

/// A dispatch store that injects transient `claim` failures: while `fail_claims` is
/// positive each `claim` decrements it and returns an error, then normal service
/// resumes. Every other operation delegates to the inner [`MemoryDispatchStore`].
/// The seam for proving a daemon/pool drain loop swallows a transient store error
/// and recovers on a later tick rather than dying.
/// Compile-surface decision table: `test-support` enabled exposes the volatile
/// failure injector to the memory suites; disabled leaves Postgres-only suites
/// compilable without reopening the in-memory production authority.
#[cfg(feature = "test-support")]
pub struct FlakyDispatchStore {
    inner: Arc<awaken_run_ingress::MemoryDispatchStore>,
    fail_claims: AtomicUsize,
    claim_attempts: AtomicUsize,
    renewal_attempts: AtomicUsize,
    fail_renewals: AtomicUsize,
    renewal_gate: Option<Arc<tokio::sync::Semaphore>>,
    renewal_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    fail_retry_exhaustion_claims: AtomicUsize,
    retry_exhaustion_claim_attempts: AtomicUsize,
}

#[cfg(feature = "test-support")]
impl FlakyDispatchStore {
    /// Wrap a fresh in-memory store that fails its first `fail_claims` claims.
    pub fn new(fail_claims: usize) -> Self {
        Self {
            inner: Arc::new(awaken_run_ingress::MemoryDispatchStore::new()),
            fail_claims: AtomicUsize::new(fail_claims),
            claim_attempts: AtomicUsize::new(0),
            renewal_attempts: AtomicUsize::new(0),
            fail_renewals: AtomicUsize::new(0),
            renewal_gate: None,
            renewal_response_gate: None,
            fail_retry_exhaustion_claims: AtomicUsize::new(0),
            retry_exhaustion_claim_attempts: AtomicUsize::new(0),
        }
    }

    pub fn with_retry_exhaustion_failures(self, failures: usize) -> Self {
        self.fail_retry_exhaustion_claims
            .store(failures, Ordering::SeqCst);
        self
    }

    pub fn with_renewal_failures(self, failures: usize) -> Self {
        self.fail_renewals.store(failures, Ordering::SeqCst);
        self
    }

    pub fn with_renewal_gate(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
        self.renewal_gate = Some(gate);
        self
    }

    pub fn with_renewal_response_gate(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
        self.renewal_response_gate = Some(gate);
        self
    }

    /// How many injected claim failures remain (test introspection).
    pub fn remaining_failures(&self) -> usize {
        self.fail_claims.load(Ordering::SeqCst)
    }

    /// Total ordinary queue claims, including injected failures and empty polls.
    pub fn claim_attempts(&self) -> usize {
        self.claim_attempts.load(Ordering::SeqCst)
    }

    /// Total exact-claim renewal writes, used to prove one renewal task owns a
    /// Pool claim across resolver-to-Worker handoff.
    pub fn renewal_attempts(&self) -> usize {
        self.renewal_attempts.load(Ordering::SeqCst)
    }

    pub fn retry_exhaustion_claim_attempts(&self) -> usize {
        self.retry_exhaustion_claim_attempts.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
#[cfg(feature = "test-support")]
impl awaken_run_ingress::DispatchQueue for FlakyDispatchStore {
    async fn lock_commit_epoch(
        &self,
        claim: &awaken_run_ingress::RunClaim,
    ) -> Result<Option<awaken_run_ingress::CommitEpochGuard>, awaken_run_ingress::DispatchError>
    {
        self.inner.lock_commit_epoch(claim).await
    }

    async fn enqueue_with(
        &self,
        request: awaken_run_ingress::RunDispatch,
        options: awaken_run_ingress::SubmitOptions,
    ) -> Result<(), awaken_run_ingress::DispatchError> {
        self.inner.enqueue_with(request, options).await
    }
    async fn claim_new_run(
        &self,
        request: awaken_run_ingress::RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.inner
            .claim_new_run(request, owner, lease_ms, now_ms, capabilities)
            .await
    }
    async fn deliver_and_claim(
        &self,
        input: awaken_run_ingress::PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.inner
            .deliver_and_claim(input, owner, lease_ms, now_ms, capabilities)
            .await
    }
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.claim_attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_claims.load(Ordering::SeqCst) > 0 {
            self.fail_claims.fetch_sub(1, Ordering::SeqCst);
            return Err(awaken_run_ingress::DispatchError::Rejected(
                "injected transient claim failure".to_string(),
            ));
        }
        self.inner
            .claim(owner, lease_ms, now_ms, capabilities)
            .await
    }
    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.retry_exhaustion_claim_attempts
            .fetch_add(1, Ordering::SeqCst);
        if self
            .fail_retry_exhaustion_claims
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(awaken_run_ingress::DispatchError::Rejected(
                "injected retry-exhaustion claim failure".to_string(),
            ));
        }
        self.inner
            .claim_retry_exhausted(owner, lease_ms, now_ms, max_attempts)
            .await
    }
    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
        capabilities: &CredentialRealizationCapabilities,
    ) -> Result<Option<awaken_run_ingress::Claimed>, awaken_run_ingress::DispatchError> {
        self.inner
            .claim_run(run_id, owner, lease_ms, now_ms, capabilities)
            .await
    }
    async fn renew_lease(
        &self,
        claim: &awaken_run_ingress::RunClaim,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, awaken_run_ingress::DispatchError> {
        self.renewal_attempts.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.renewal_gate {
            gate.acquire()
                .await
                .map_err(|error| awaken_run_ingress::DispatchError::Rejected(error.to_string()))?
                .forget();
        }
        if self
            .fail_renewals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(awaken_run_ingress::DispatchError::Rejected(
                "injected transient renewal failure".to_string(),
            ));
        }
        let result = self.inner.renew_lease(claim, lease_ms, now_ms).await;
        if let Some(gate) = &self.renewal_response_gate {
            gate.acquire()
                .await
                .map_err(|error| awaken_run_ingress::DispatchError::Rejected(error.to_string()))?
                .forget();
        }
        result
    }
    async fn begin_attempt(
        &self,
        claim: &awaken_run_ingress::RunClaim,
        now_ms: u64,
    ) -> Result<awaken_run_ingress::AttemptAdmission, awaken_run_ingress::DispatchError> {
        self.inner.begin_attempt(claim, now_ms).await
    }
    async fn finish_attempt(
        &self,
        claim: &awaken_run_ingress::RunClaim,
    ) -> Result<awaken_run_ingress::SettleOutcome, awaken_run_ingress::DispatchError> {
        self.inner.finish_attempt(claim).await
    }
    async fn relinquish_claim(
        &self,
        claim: &awaken_run_ingress::RunClaim,
    ) -> Result<awaken_run_ingress::SettleOutcome, awaken_run_ingress::DispatchError> {
        self.inner.relinquish_claim(claim).await
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
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, awaken_run_ingress::DispatchError> {
        self.inner
            .quarantine_retry_exhausted(max_attempts, now_ms)
            .await
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
#[cfg(feature = "test-support")]
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
#[cfg(feature = "test-support")]
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
        context_messages: Vec::new(),
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
        context_messages: Vec::new(),
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

/// Cause/effect decision table CE-SM4..SM7 for every dispatch backend.
///
/// | rule | aggregate | same id | payload | effect |
/// | SM4  | outbox    | yes     | same    | idempotent false |
/// | SM7  | outbox    | yes     | changed | explicit conflict |
/// | SM4  | inbox     | yes     | same    | idempotent false |
/// | SM7  | inbox     | yes     | changed | explicit conflict |
/// | SM4  | deliver+claim | yes  | same    | idempotent pending append |
/// | SM7  | deliver+claim | yes  | changed | explicit conflict |
/// | TM5  | inbox     | yes     | u64::MAX schedule | normalize, then retry false |
pub async fn assert_message_idempotency_conflicts<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::PendingInput;

    let input = |message_id: &str, content: &str| PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId("run-idempotency".to_string()),
        thread_id: ThreadId("thread-idempotency".to_string()),
        correlation_id: "correlation-idempotency".to_string(),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input(content.to_string()),
    };

    let outbox = input("outbox-key", "one");
    assert!(store.stage(outbox.clone()).await.unwrap());
    assert!(!store.stage(outbox).await.unwrap());
    let outbox_conflict = store
        .stage(input("outbox-key", "changed"))
        .await
        .expect_err("same outbox key with changed payload must conflict");
    assert!(outbox_conflict.to_string().contains("idempotency key"));

    let inbox = input("inbox-key", "one");
    assert!(store.append(inbox.clone()).await.unwrap());
    assert!(!store.append(inbox).await.unwrap());
    let inbox_conflict = store
        .append(input("inbox-key", "changed"))
        .await
        .expect_err("same inbox key with changed payload must conflict");
    assert!(inbox_conflict.to_string().contains("idempotency key"));

    let direct = input("direct-key", "one");
    assert!(
        store
            .deliver_and_claim(direct.clone(), "owner", 10, 0, &Default::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .deliver_and_claim(direct, "owner", 10, 0, &Default::default())
            .await
            .unwrap()
            .is_none()
    );
    let direct_conflict = store
        .deliver_and_claim(
            input("direct-key", "changed"),
            "owner",
            10,
            0,
            &Default::default(),
        )
        .await
        .expect_err("direct delivery must share pending idempotency validation");
    assert!(direct_conflict.to_string().contains("idempotency key"));

    let mut boundary = input("boundary-key", "future");
    boundary.available_at_ms = Some(u64::MAX);
    assert!(store.append(boundary.clone()).await.unwrap());
    assert!(
        !store.append(boundary).await.unwrap(),
        "retry compares the normalized payload rather than the raw u64"
    );
    let boundary = store
        .list(&ThreadId("thread-idempotency".to_string()))
        .await
        .unwrap()
        .into_iter()
        .find(|record| record.input.message_id == "boundary-key")
        .expect("boundary row");
    assert_eq!(boundary.input.available_at_ms, Some(i64::MAX as u64));
}

/// Shared spec for scheduled delivery (M4): a future-dated pending input is not
/// claimable until its time has come; every backend must gate the wake the same.
pub async fn assert_scheduled_due<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::{DispatchOutcome, PendingInput, RunDispatch};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // Claim the fresh run, then await it so it can be woken by a delivery.
    let claimed = store
        .claim("w", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("running owner");
    store
        .settle(&run, claimed.lease.epoch, DispatchOutcome::Awaiting, &[])
        .await
        .unwrap();

    // The returned lease deadline is expressed in the backend's authority
    // clock, so the same schedule remains future-dated for logical and live
    // database clocks.
    let delivery_at = claimed.lease.expires_ms;
    store
        .append(PendingInput {
            message_id: "sched".to_string(),
            run_id: run.clone(),
            thread_id: ThreadId(THREAD.to_string()),
            correlation_id: TICKET.to_string(),
            available_at_ms: Some(delivery_at),
            result: ResumeResult::allow(),
            context_messages: Vec::new(),
        })
        .await
        .unwrap();

    // Before its time, the run is not claimable; at its time, it wakes with the
    // now-due input in hand.
    assert!(
        store
            .claim(
                "w",
                1_000,
                delivery_at.saturating_sub(1),
                &Default::default(),
            )
            .await
            .unwrap()
            .is_none(),
        "a future delivery is not yet claimable"
    );
    clock.advance_past(delivery_at.saturating_sub(1)).await;
    let claimed = store
        .claim("w", 1_000, delivery_at, &Default::default())
        .await
        .unwrap()
        .expect("a due delivery is claimable");
    assert_eq!(claimed.pending.len(), 1);
    assert_eq!(claimed.pending[0].message_id, "sched");
}

/// Cause/effect rules CE-TM4..TM8/TM11/TM12. The public clock is `u64`, while
/// SQL stores signed BIGINT; all backends must saturate at `i64::MAX`, never
/// panic/wrap, never run a far-future delivery early, and keep a huge lease live.
pub async fn assert_millis_boundaries<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::{DispatchOutcome, PendingInput, RunDispatch};

    let max_signed = i64::MAX as u64;
    for (nth, available_at) in [max_signed, max_signed + 1, u64::MAX]
        .into_iter()
        .enumerate()
    {
        let run_name = format!("millis-schedule-{nth}");
        let run = RunId(run_name.clone());
        store
            .enqueue(RunDispatch::new(activation(&run_name)))
            .await
            .unwrap();
        let claimed = store
            .claim("owner", 100, 0, &Default::default())
            .await
            .unwrap()
            .expect("fresh run");
        store
            .settle(&run, claimed.lease.epoch, DispatchOutcome::Awaiting, &[])
            .await
            .unwrap();
        store
            .append(PendingInput {
                message_id: format!("millis-message-{nth}"),
                run_id: run.clone(),
                thread_id: ThreadId(THREAD.to_string()),
                correlation_id: TICKET.to_string(),
                available_at_ms: Some(available_at),
                context_messages: Vec::new(),
                result: ResumeResult::Input("future".to_string()),
            })
            .await
            .unwrap();

        assert!(
            store
                .claim("too-early", 100, 1_000, &Default::default())
                .await
                .unwrap()
                .is_none(),
            "TM4..TM6: {available_at} must remain in the future"
        );
        if clock.exact_boundary_is_controllable() {
            let due = store
                .claim("at-boundary", 100, u64::MAX, &Default::default())
                .await
                .unwrap()
                .expect("normalized signed maximum is inclusively due");
            assert_eq!(due.pending.len(), 1);
            store
                .settle(
                    &run,
                    due.lease.epoch,
                    DispatchOutcome::Done,
                    &[format!("millis-message-{nth}")],
                )
                .await
                .unwrap();
        } else {
            // A live wall clock cannot be advanced to i64::MAX. PostgreSQL
            // still proves saturation above and that the row never runs early;
            // the exact inclusive boundary remains covered by controllable
            // stores and the shared pure clock classifier.
            store.cancel(&run).await.unwrap();
            let cancelled = store
                .claim("boundary-cleanup", 100, 1_000, &Default::default())
                .await
                .unwrap()
                .expect("cancelled far-future row is control-claimable");
            store
                .settle(
                    &run,
                    cancelled.lease.epoch,
                    DispatchOutcome::Done,
                    &[format!("millis-message-{nth}")],
                )
                .await
                .unwrap();
        }
    }

    let lease_run = RunId("millis-lease".to_string());
    store
        .enqueue(RunDispatch::new(activation("millis-lease")))
        .await
        .unwrap();
    let lease = store
        .claim("lease-owner", u64::MAX, 1_000, &Default::default())
        .await
        .unwrap()
        .expect("huge lease claim");
    assert_eq!(lease.request.run_id(), &lease_run);
    assert_eq!(lease.lease.expires_ms, max_signed);
    assert!(
        store
            .claim("thief", 100, 2_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "TM7: a saturated huge lease cannot be stolen early"
    );
    assert!(
        store
            .renew_lease(
                &awaken_run_ingress::RunClaim::from(&lease.lease),
                u64::MAX,
                max_signed - 1,
            )
            .await
            .unwrap(),
        "TM12: huge renewal succeeds without overflow"
    );
    assert!(
        store
            .claim("thief", 100, max_signed, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "lease expiry remains exclusive at the exact deadline"
    );
}

/// Shared spec for the crash-retry budget and dead-letter (M5): a run reclaimed
/// past its budget is dead-lettered and no longer claimed, `requeue` brings an
/// ordinary row back, and a terminal cancellation fence makes it non-runnable.
/// Every backend must match.
pub async fn assert_dead_letter<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::RunDispatch;
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // A fresh claim does not spend the budget; each later recovery (expired
    // lease) does. With max_attempts = 2, two recoveries exhaust it.
    let first = store
        .claim("w", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("fresh poison claim");
    clock.advance_past(first.lease.expires_ms).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(2, first.lease.expires_ms.saturating_add(1))
            .await
            .unwrap(),
        0,
        "still within budget"
    );
    let second = store
        .claim(
            "w",
            100,
            first.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("first poison recovery");
    clock.advance_past(second.lease.expires_ms).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(2, second.lease.expires_ms.saturating_add(1))
            .await
            .unwrap(),
        0
    );
    let third = store
        .claim(
            "w",
            100,
            second.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("second poison recovery");
    clock.advance_past(third.lease.expires_ms).await;

    // Explicit quarantine moves the exhausted row to DeadLetter; it is no longer claimable.
    assert_eq!(
        store
            .quarantine_retry_exhausted(2, third.lease.expires_ms.saturating_add(1))
            .await
            .unwrap(),
        1,
        "quarantined"
    );
    assert!(
        store
            .claim("w", 100, 700, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a dead-lettered run is not claimed"
    );
    assert_eq!(store.dead_letters().await.unwrap(), vec![run.clone()]);

    // Requeue restores it to a fresh budget.
    assert!(store.requeue(&run).await.unwrap());
    assert!(store.dead_letters().await.unwrap().is_empty());
    let requeued = store
        .claim("w", 100, 800, &Default::default())
        .await
        .unwrap()
        .expect("a requeued run is claimable again");

    // Cause/effect decision table for terminal fencing of retained poison rows:
    // C1 an ordinary DeadLetter has no cancel bit; C2 terminal quiescence calls
    // the existing durable cancel operation; E1 ordinary requeue succeeds; E2
    // cancellation retains DeadLetter but seals requeue and all future claims.
    //
    // | Rule | DeadLetter | cancel requested | Effect |
    // | D1 | yes | no | E1 |
    // | D2 | yes | yes | E2 |
    clock.advance_past(requeued.lease.expires_ms).await;
    let requeued_second = store
        .claim(
            "w",
            100,
            requeued.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("first sealed-cycle recovery");
    clock.advance_past(requeued_second.lease.expires_ms).await;
    let requeued_third = store
        .claim(
            "w",
            100,
            requeued_second.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("second sealed-cycle recovery");
    clock.advance_past(requeued_third.lease.expires_ms).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(2, requeued_third.lease.expires_ms.saturating_add(1),)
            .await
            .unwrap(),
        1
    );
    assert!(
        store.cancel(&run).await.unwrap().is_some(),
        "D2 terminal fence seals the retained row"
    );
    let sealed = store
        .list_dispatches()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.run_id == run)
        .expect("D2 retained DeadLetter");
    assert_eq!(sealed.state, awaken_run_ingress::DispatchState::DeadLetter);
    assert!(sealed.cancellation_requested, "D2 durable fence bit");
    assert!(!store.requeue(&run).await.unwrap(), "D2/E2");
    assert!(
        store
            .claim("w", 100, 1600, &Default::default())
            .await
            .unwrap()
            .is_none()
    );
}

/// Shared spec for durable cancel: intent is retained and claimable until a
/// fenced Done settlement, including across a running lease's expiry. Every
/// backend must match.
pub async fn assert_cancel<S: awaken_run_ingress::Dispatch>(store: &S) {
    // Cause/effect graph: C1 row is Pending/Leased/Awaiting/Done; C2 cancellation
    // is absent/persisted; C3 claim epoch is current/stale. Effects: E1 the
    // claim-fenced guard exposes the same cancellation bit as the claimed row;
    // E2 cancellation remains claimable until Done; E3 a revoked epoch cannot
    // commit/settle; E4 Done is terminal and idempotent.
    //
    // | Rule | State | Cancel | Claim | Effect |
    // |---|---|---|---|---|
    // | C1 | Leased | no | current | guard false |
    // | C2 | Pending/Awaiting | yes | current | E1+E2, guard true |
    // | C3 | Leased then cancelled | yes | stale | E3 |
    // | C4 | Done | any | any | E4 |
    use awaken_run_ingress::RunDispatch;
    let thread = Some(ThreadId(THREAD.to_string()));

    // A pending run records an idempotent intent; it is not deleted before the
    // worker has committed the terminal fact.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        thread
    );
    assert!(
        store
            .list_dispatches()
            .await
            .unwrap()
            .iter()
            .any(|summary| summary.run_id.0 == "run-1" && summary.cancellation_requested),
        "operations can distinguish cancellation intent from ordinary pending work"
    );
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        thread,
        "repeating an uncommitted intent is idempotent"
    );
    let cancelled = store
        .claim("w", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("cancellation intent is claimable");
    assert!(cancelled.cancellation_requested);
    let cancelled_guard = store
        .lock_commit_epoch(&awaken_run_ingress::RunClaim::from(&cancelled.lease))
        .await
        .unwrap()
        .expect("C2 current cancellation claim has a guard");
    assert!(cancelled_guard.cancellation_requested(), "C2/E1");
    drop(cancelled_guard);
    store
        .settle(
            &RunId("run-1".to_string()),
            cancelled.lease.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    // Only after terminal settlement is it unknown.
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        None
    );

    // A running cancellation revokes the old epoch and becomes immediately
    // claimable; the former owner cannot commit or settle afterward.
    store
        .enqueue(RunDispatch::new(activation("run-2")))
        .await
        .unwrap();
    let old_owner = store
        .claim("w", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("running owner");
    let live_guard = store
        .lock_commit_epoch(&awaken_run_ingress::RunClaim::from(&old_owner.lease))
        .await
        .unwrap()
        .expect("C1 current ordinary claim has a guard");
    assert!(!live_guard.cancellation_requested(), "C1/E1");
    drop(live_guard);
    assert_eq!(
        store.cancel(&RunId("run-2".to_string())).await.unwrap(),
        thread,
        "live cancellation is persisted before signalling"
    );
    assert!(
        store
            .lock_commit_epoch(&awaken_run_ingress::RunClaim::from(&old_owner.lease))
            .await
            .unwrap()
            .is_none(),
        "the revoked owner loses commit authority immediately"
    );
    let reclaimed = store
        .claim("replacement", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("revoked cancellation intent is immediately recovered");
    assert!(reclaimed.cancellation_requested);
    let reclaimed_guard = store
        .lock_commit_epoch(&awaken_run_ingress::RunClaim::from(&reclaimed.lease))
        .await
        .unwrap()
        .expect("C2 reclaimed cancellation has a guard");
    assert!(reclaimed_guard.cancellation_requested(), "C2/E1");
    drop(reclaimed_guard);
    assert!(
        reclaimed.lease.epoch >= 3,
        "revoke and re-claim both advance the fence"
    );
    assert_eq!(
        store
            .settle(
                &RunId("run-2".to_string()),
                1,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap(),
        awaken_run_ingress::SettleOutcome::Fenced,
        "the cancelled owner cannot settle after revocation"
    );
    store
        .settle(
            &RunId("run-2".to_string()),
            reclaimed.lease.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    // An Awaiting dispatch remains delivery work and cancellation makes that
    // work claimable; Thread truth, not this queue, owns resume addressing.
    let thread_id = ThreadId(THREAD.to_string());
    store
        .enqueue(RunDispatch::new(activation("run-3")))
        .await
        .unwrap();
    assert!(
        store
            .claim("w", 1_000, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    store
        .settle(
            &RunId("run-3".to_string()),
            1,
            awaken_run_ingress::DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        store.cancel(&RunId("run-3".to_string())).await.unwrap(),
        Some(thread_id.clone())
    );
    let awaiting_cancel = store
        .claim("w", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("awaiting cancellation is claimable without pending input");
    assert!(awaiting_cancel.cancellation_requested);
    store
        .settle(
            &RunId("run-3".to_string()),
            awaiting_cancel.lease.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    // Terminal control is independent of execution placement. A worker that does
    // not satisfy the run's pinned GPU capability can still claim cancellation,
    // and the replaceable ranking policy is not consulted for that control task.
    use awaken_run_ingress::{
        LeastLoadedPolicy, PlacementRequirements, WorkerIdentity, WorkerManifest, WorkerSnapshot,
        WorkerState,
    };
    let mut gpu = PlacementRequirements::remote_required();
    gpu.required_capabilities.insert("gpu".to_string());
    let manifest = WorkerManifest::default();
    let worker = WorkerSnapshot {
        identity: WorkerIdentity::new("control-worker", "boot-1", 1),
        capability_fingerprint: manifest.fingerprint().unwrap(),
        manifest,
        state: WorkerState::Ready,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        expires_at_ms: 10_000,
    };

    for (run, placed) in [("run-4", false), ("run-5", true)] {
        store
            .enqueue(
                RunDispatch::new(activation_on(run, &format!("{run}-thread")))
                    .with_placement(gpu.clone()),
            )
            .await
            .unwrap();
        store.cancel(&RunId(run.to_string())).await.unwrap();
        let claimed = if placed {
            store
                .claim_placed(
                    &worker,
                    vec![worker.clone()],
                    Arc::new(LeastLoadedPolicy),
                    100,
                    0,
                )
                .await
                .unwrap()
        } else {
            store.claim_compatible(&worker, 100, 0).await.unwrap()
        }
        .expect("placement-incompatible cancellation is still control-claimable");
        assert!(claimed.cancellation_requested);
        store
            .settle(
                &RunId(run.to_string()),
                claimed.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap();
    }
}

/// Shared spec for priority, dedupe, and dead-letter GC. Every backend matches.
pub async fn assert_priority_dedupe_gc<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::{DispatchOutcome, RunDispatch, SubmitOptions};
    // Each run on its own thread: priority/dedupe/GC are thread-orthogonal, and
    // single-writer-per-thread (ADR-0022) forbids claiming two runs of one thread at
    // once, which these assertions do.
    let req = |id: &str| RunDispatch::new(activation_on(id, id));

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
            .claim("w", 1_000, 0, &Default::default())
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
            .claim("w", 1_000, 0, &Default::default())
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
            .claim("w", 1_000, 0, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "d1"
    );
    assert!(
        store
            .claim("w", 1_000, 0, &Default::default())
            .await
            .unwrap()
            .is_none(),
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
    let poison = store
        .claim("w", 1, 0, &Default::default())
        .await
        .unwrap()
        .expect("poison claim");
    clock.advance_past(poison.lease.expires_ms).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(0, poison.lease.expires_ms.saturating_add(1))
            .await
            .unwrap(),
        1
    );
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
    use awaken_run_ingress::{DispatchState, RunDispatch};

    // A fresh run is Pending; once claimed it is Running.
    store
        .enqueue(RunDispatch::new(activation("r1")))
        .await
        .unwrap();
    let listed = store.list_dispatches().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, RunId("r1".to_string()));
    assert_eq!(listed[0].state, DispatchState::Pending);
    assert_eq!(listed[0].attempt_count, 0);
    assert!(!listed[0].sandbox_bound);

    let claimed = store
        .claim("w", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("fresh run is claimable");
    let listed = store.list_dispatches().await.unwrap();
    assert_eq!(listed[0].state, DispatchState::Leased);
    assert!(!listed[0].sandbox_bound);

    assert!(
        store
            .bind_sandbox(
                &awaken_run_ingress::RunClaim::from(&claimed.lease),
                "opaque"
            )
            .await
            .unwrap()
            .applied()
    );
    assert!(store.list_dispatches().await.unwrap()[0].sandbox_bound);
}

/// Cause/effect table for pre-execution admission rollback:
///
/// | claim | subordinate admission | effect |
/// |---|---|---|
/// | current owner/epoch | temporarily unavailable | return to pending without crash budget |
/// | stale owner/epoch | replacement already claimed | fence without changing replacement |
pub async fn assert_relinquish_claim<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchState, RunClaim, RunDispatch, SettleOutcome};

    store
        .enqueue(RunDispatch::new(activation("relinquish-run")))
        .await
        .unwrap();
    let first = store
        .claim("owner-a", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("R1 initial claim");
    let first_claim = RunClaim::from(&first.lease);
    assert_eq!(
        store.relinquish_claim(&first_claim).await.unwrap(),
        SettleOutcome::Applied,
        "R1 exact owner returns the unstarted Run"
    );
    let pending = store.list_dispatches().await.unwrap();
    assert_eq!(pending[0].state, DispatchState::Pending, "R1");
    assert_eq!(pending[0].attempt_count, 0, "R1 is not a crash recovery");

    let replacement = store
        .claim("owner-b", 1_000, 1, &Default::default())
        .await
        .unwrap()
        .expect("R1 released Run is immediately claimable");
    assert_eq!(replacement.lease.epoch, first.lease.epoch + 1);
    assert_eq!(
        store.relinquish_claim(&first_claim).await.unwrap(),
        SettleOutcome::Fenced,
        "R2 stale owner cannot release the replacement"
    );
    let leased = store.list_dispatches().await.unwrap();
    assert_eq!(leased[0].state, DispatchState::Leased, "R2");
    assert_eq!(leased[0].attempt_count, 0, "R2");
}

/// Physical-attempt admission cause/effect decision table:
///
/// | Rule | durable claim | occupied slot | command | effect |
/// |---|---|---|---|---|
/// | A1 | current A | empty | begin A | applied; slot=A |
/// | A2 | current A | A | begin A retry | already-applied |
/// | A3 | current A | A | settle/relinquish | fenced; slot retained |
/// | A4 | replacement B | stale A | begin B | blocked; no executor entry |
/// | A5 | replacement B | stale A | finish A | applied; slot empty |
/// | A6 | current B | empty | begin B | applied; A cannot clear B |
/// | A7 | current B | empty after finish B | finish B retry | applied; remains empty |
///
/// Constraints: lease expiry can replace the mutation claim but is not proof
/// that the old physical future returned. `finish_attempt` is the only ordinary
/// transition that frees this slot, and every backend must implement the same
/// aggregate rather than a side lock.
pub async fn assert_physical_attempt_admission<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::{
        AttemptAdmission, DispatchOutcome, RunClaim, RunDispatch, SettleOutcome,
    };

    clock.set(0);
    store
        .enqueue(RunDispatch::new(activation("physical-slot-run")))
        .await
        .expect("enqueue physical-slot fixture");
    let first = store
        .claim("owner-a", 10, 0, &Default::default())
        .await
        .expect("A claim query")
        .expect("A claim");
    let claim_a = RunClaim::from(&first.lease);
    assert_eq!(
        store.begin_attempt(&claim_a, 0).await.expect("A1"),
        AttemptAdmission::Applied,
        "A1"
    );
    assert_eq!(
        store.begin_attempt(&claim_a, 1).await.expect("A2"),
        AttemptAdmission::AlreadyApplied,
        "A2 idempotent transport retry"
    );
    assert!(
        store
            .list_dispatches()
            .await
            .expect("A2 operational projection")[0]
            .physical_attempt_active,
        "A2 operators can distinguish a stuck physical attempt from a lease"
    );
    assert_eq!(
        store.relinquish_claim(&claim_a).await.expect("A3 release"),
        SettleOutcome::Fenced,
        "A3 an executing claim cannot return to Pending"
    );
    assert_eq!(
        store
            .settle(&claim_a.run_id, claim_a.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("A3 settle"),
        SettleOutcome::Fenced,
        "A3 outcome cannot erase the quiescence fact"
    );
    clock.advance_past(first.lease.expires_ms).await;
    let after_expiry = first.lease.expires_ms.saturating_add(1);
    assert_eq!(
        store
            .quarantine_retry_exhausted(0, after_expiry)
            .await
            .expect("A3 quarantine"),
        0,
        "A3 retry exhaustion cannot dead-letter a physically active attempt"
    );
    assert!(
        store
            .claim_retry_exhausted("terminalizer", 10, after_expiry, 0)
            .await
            .expect("A3 terminal claim query")
            .is_none(),
        "A3 retry terminalization cannot replace a physically active attempt"
    );

    let second = store
        .claim("owner-b", 10, after_expiry, &Default::default())
        .await
        .expect("B reclaim query")
        .expect("B reclaims after expiry");
    let claim_b = RunClaim::from(&second.lease);
    assert_eq!(
        store
            .begin_attempt(&claim_b, after_expiry)
            .await
            .expect("A4"),
        AttemptAdmission::Blocked,
        "A4 lease takeover alone cannot admit a second physical attempt"
    );
    assert_eq!(
        store.finish_attempt(&claim_a).await.expect("A5"),
        SettleOutcome::Applied,
        "A5 stale mutation owner may acknowledge its exact physical slot"
    );
    assert_eq!(
        store
            .begin_attempt(&claim_b, after_expiry)
            .await
            .expect("A6"),
        AttemptAdmission::Applied,
        "A6 B enters only after A ACK"
    );
    assert_eq!(
        store
            .finish_attempt(&claim_a)
            .await
            .expect("A6 stale retry"),
        SettleOutcome::Fenced,
        "A6 A cannot clear B's slot"
    );
    assert_eq!(
        store.finish_attempt(&claim_b).await.expect("A6 B finish"),
        SettleOutcome::Applied,
        "A6 B releases its exact slot"
    );
    assert_eq!(
        store
            .finish_attempt(&claim_b)
            .await
            .expect("A7 B finish response-loss retry"),
        SettleOutcome::Applied,
        "A7 the exact finish is idempotent after its response is lost"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("A6 operational projection")[0]
            .physical_attempt_active,
        "A7 quiescence remains visible without exposing owner fencing material"
    );
}

/// Shared spec for time-windowed dead-letter GC (ADR-0023): GC removes only
/// dead-letters older than the cutoff; younger ones stay. Every backend matches.
pub async fn assert_dead_letter_ttl_gc<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::RunDispatch;

    // A run is dead-lettered at t=1000 (claimed with a 1ms lease at t=0, then
    // manually quarantined at budget 0 once the lease has expired).
    store
        .enqueue(RunDispatch::new(activation("poison")))
        .await
        .unwrap();
    let claimed = store
        .claim("w", 1, 0, &Default::default())
        .await
        .unwrap()
        .expect("dead-letter source claim");
    clock.advance_past(claimed.lease.expires_ms).await;
    assert_eq!(
        store
            .quarantine_retry_exhausted(0, claimed.lease.expires_ms.saturating_add(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.dead_letters().await.unwrap(),
        vec![RunId("poison".to_string())]
    );

    // A GC cutoff before the dead-letter time spares it; a cutoff at/after it purges.
    assert_eq!(store.purge_dead_letters_before(0).await.unwrap(), 0);
    assert_eq!(
        store.dead_letters().await.unwrap().len(),
        1,
        "a younger dead-letter is spared"
    );
    assert_eq!(store.purge_dead_letters_before(u64::MAX).await.unwrap(), 1);
    assert!(store.dead_letters().await.unwrap().is_empty());
}

/// Shared spec for epoch supersession (ADR-0022): a superseding submit abandons
/// the thread's prior awaiting work; only the newest run stays claimable. Every
/// backend matches.
pub async fn assert_supersession<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunDispatch, SubmitOptions};

    // An older run awaits on the thread.
    store
        .enqueue(RunDispatch::new(activation("old")))
        .await
        .unwrap();
    assert!(
        store
            .claim("w", 1_000, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    store
        .settle(&RunId("old".to_string()), 1, DispatchOutcome::Awaiting, &[])
        .await
        .unwrap();

    // A superseding submit on the same thread supersedes the awaiting run.
    store
        .enqueue_with(
            RunDispatch::new(activation("new")),
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

    // Only the newest run is claimable; the superseded awaiting run is never woken.
    let newest = store
        .claim("w", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newest.request.run_id().0, "new");
    store
        .settle(
            &RunId("new".to_string()),
            newest.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    // Once cancellation is accepted, a later superseding submission cannot erase
    // it. Terminal control is claimed before the newer ordinary run.
    store
        .enqueue(RunDispatch::new(activation("cancel-old")))
        .await
        .unwrap();
    assert!(
        store
            .cancel(&RunId("cancel-old".to_string()))
            .await
            .unwrap()
            .is_some()
    );
    store
        .enqueue_with(
            RunDispatch::new(activation("after-cancel")),
            SubmitOptions {
                supersede: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cancellation = store
        .claim("w", 1_000, 0, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancellation.request.run_id().0, "cancel-old");
    assert!(cancellation.cancellation_requested);
}

/// Shared spec for the idle-thread inbox (ADR-0021): unbound input is listed for
/// its thread, and a Done settle that consumed it removes it. Every backend matches.
pub async fn assert_idle_thread_inbox<S: awaken_run_ingress::Dispatch>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunDispatch};
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
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    store
        .claim("w", 1_000, 0, &Default::default())
        .await
        .unwrap();
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

/// Shared exact-renewal cause/effect table. C1=run/owner match, C2=epoch match,
/// C3=lease is later reclaimed, including by the same owner string. E1=extend
/// only the exact claim; E2=return false without mutating the current lease.
///
/// | Rule | Run/owner | Epoch | Current claim | Effect |
/// | LR1 | match | match | original | E1 |
/// | LR2 | owner differs | any | original | E2 |
/// | LR3 | match | stale | same-owner replacement | E2 |
/// | LR4 | match | current | same-owner replacement | E1 |
///
/// Constraint: `lease_epoch` is the same fencing authority used by commit and
/// settlement; renewal cannot infer exact ownership from a reusable owner name.
/// Every backend executes the same rules.
pub async fn assert_lease_renewal<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::RunDispatch;
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let first = store
        .claim("owner-a", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("owner-a claim");

    // owner-a renews at t=50 (extends to 150); a recovery claim at t=120 cannot
    // steal it because the lease has not expired.
    let renew_at = first.lease.expires_ms.saturating_sub(40);
    clock.advance_past(renew_at.saturating_sub(1)).await;
    assert!(
        store
            .renew_lease(
                &awaken_run_ingress::RunClaim::from(&first.lease),
                100,
                renew_at,
            )
            .await
            .unwrap()
    );
    clock.advance_past(first.lease.expires_ms).await;
    assert!(
        store
            .claim(
                "owner-b",
                100,
                first.lease.expires_ms.saturating_add(1),
                &Default::default(),
            )
            .await
            .unwrap()
            .is_none(),
        "a renewed lease is not yet expired"
    );
    // A non-owner cannot renew.
    assert!(
        !store
            .renew_lease(
                &awaken_run_ingress::RunClaim {
                    run_id: run.clone(),
                    owner: "owner-b".into(),
                    epoch: first.lease.epoch,
                },
                100,
                first.lease.expires_ms.saturating_add(2),
            )
            .await
            .unwrap()
    );

    // Once the renewed lease expires, recovery reclaims for the new owner.
    clock
        .advance_past(first.lease.expires_ms.saturating_add(100))
        .await;
    let recovered = store
        .claim(
            "owner-b",
            100,
            first.lease.expires_ms.saturating_add(101),
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("LR2 a different owner recovers the expired lease");
    assert_eq!(recovered.lease.owner, "owner-b");
    assert_eq!(
        store
            .settle(
                &run,
                recovered.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap(),
        awaken_run_ingress::SettleOutcome::Applied,
    );

    let same_owner_run = RunId("run-same-owner-epoch".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-same-owner-epoch")))
        .await
        .unwrap();
    let same_owner_start = recovered.lease.expires_ms.saturating_add(1);
    clock.advance_past(same_owner_start).await;
    let old = store
        .claim("reused-owner", 100, same_owner_start, &Default::default())
        .await
        .unwrap()
        .expect("LR3 original same-owner claim");
    let replacement_at = old.lease.expires_ms.saturating_add(1);
    clock.advance_past(replacement_at).await;
    let replacement = store
        .claim("reused-owner", 100, replacement_at, &Default::default())
        .await
        .unwrap()
        .expect("LR3 same owner string recovers under a new epoch");
    assert!(
        replacement.lease.epoch > old.lease.epoch,
        "LR3 precondition"
    );
    assert!(
        !store
            .renew_lease(
                &awaken_run_ingress::RunClaim::from(&old.lease),
                100,
                replacement_at,
            )
            .await
            .unwrap(),
        "LR3/E2 stale epoch cannot renew a same-owner replacement"
    );
    assert!(
        store
            .renew_lease(
                &awaken_run_ingress::RunClaim::from(&replacement.lease),
                100,
                replacement_at,
            )
            .await
            .unwrap(),
        "LR4/E1 current epoch renews"
    );
    assert_eq!(replacement.request.run_id(), &same_owner_run);
}

/// Shared spec (every backend must match): the fencing token. A claim bumps the
/// lease epoch monotonically; a settle applies only under the current epoch. A
/// stale owner whose lease lapsed and was re-claimed cannot settle the dispatch out
/// from under the reclaimer — its settle is fenced and changes nothing.
pub async fn assert_settle_fences_stale_epoch<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::{DispatchOutcome, RunDispatch, SettleOutcome};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // Owner A claims: the fresh row's epoch bumps 0 -> 1.
    let a = store
        .claim("owner-a", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch, 1, "the first claim bumps the fence to 1");

    // A's lease lapses; owner B recovers it — the epoch bumps 1 -> 2.
    clock.advance_past(a.lease.expires_ms).await;
    let b = store
        .claim(
            "owner-b",
            100,
            a.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
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
        store
            .claim(
                "owner-c",
                100,
                b.lease.expires_ms.saturating_sub(1),
                &Default::default(),
            )
            .await
            .unwrap()
            .is_none(),
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
        store
            .claim("owner-c", 100, 300, &Default::default())
            .await
            .unwrap()
            .is_none(),
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
    use awaken_run_ingress::{RunDispatch, SubmitOptions};
    let key = || SubmitOptions {
        dedupe_key: Some("k".to_string()),
        ..Default::default()
    };

    // A first run takes the key, is claimed with a 1ms lease, then quarantined (budget 0)
    // into the dead-letter status once its lease expires.
    store
        .enqueue_with(RunDispatch::new(activation("run-1")), key())
        .await
        .unwrap();
    assert!(
        store
            .claim("w", 1, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store.quarantine_retry_exhausted(0, 100).await.unwrap(),
        1,
        "run-1 quarantined"
    );
    assert_eq!(
        store.dead_letters().await.unwrap(),
        vec![RunId("run-1".to_string())]
    );

    // A re-submit under the SAME key is NOT deduped away — the only holder is dead —
    // so run-2 enqueues and is the claimable work (run-1, dead-lettered, is not).
    store
        .enqueue_with(RunDispatch::new(activation("run-2")), key())
        .await
        .unwrap();
    assert_eq!(
        store
            .claim("w", 1_000, 200, &Default::default())
            .await
            .unwrap()
            .expect("the re-submit is claimable, not deduped")
            .request
            .run_id()
            .0,
        "run-2",
    );
}

/// ADR-0022 wake-path cause/effect graph. C1 one Run is Awaiting; C2 a fresh peer
/// is Pending; C3 terminal cancellation claims that peer; C4 matching input is
/// delivered to the Awaiting Run; C5 the cancellation claim settles. E1 C2 stays
/// queued behind C1; E2 cancellation may temporarily own the Thread; E3 C4 cannot
/// wake C1 while C3 is Running; E4 C5 releases C1 to wake exactly once.
///
/// | Rule | Awaiting | Peer | Input | Effect |
/// |---|---|---|---|---|
/// | W1 | yes | fresh Pending | none | E1 |
/// | W2 | yes | cancelled Pending | none | E2 |
/// | W3 | yes | Running(cancel) | due | E3 |
/// | W4 | yes | Done | due | E4 |
pub async fn assert_wake_suppressed_while_thread_running<S: awaken_run_ingress::Dispatch>(
    store: &S,
) {
    use awaken_run_ingress::{DispatchOutcome, RunDispatch};
    use awaken_runtime_contract::resume::ResumeResult;

    // run-1 awaits on the thread.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("w", 10_000, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    store
        .settle(
            &RunId("run-1".to_string()),
            1,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .unwrap();

    // A fresh run-2 stays queued behind run-1. Cancellation is the existing
    // terminal-control exception that can claim it, constructing a reachable
    // Running-peer state without violating ordinary admission.
    store
        .enqueue(RunDispatch::new(activation("run-2")))
        .await
        .unwrap();
    assert!(
        store
            .claim("w", 10_000, 1, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "W1: a fresh peer stays queued behind Awaiting"
    );
    store
        .cancel(&RunId("run-2".to_string()))
        .await
        .unwrap()
        .expect("W2 cancellation target exists");
    assert!(
        store
            .claim("w", 10_000, 1, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "W2: cancellation may claim past an Awaiting peer"
    );

    // Deliver input that answers run-1's await. run-1 is now wakeable *by input* — but
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
        store
            .claim("w", 10_000, 2, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "W3: the awaiting run is not woken while its thread already runs another",
    );

    // run-2 settles Done, freeing the thread; now the wake fires and hands run-1 its
    // due input. run-2 was claimed exactly once, so its lease epoch is 1.
    store
        .settle(&RunId("run-2".to_string()), 1, DispatchOutcome::Done, &[])
        .await
        .unwrap();
    let claimed = store
        .claim("w", 10_000, 3, &Default::default())
        .await
        .unwrap()
        .expect("W4: the freed thread lets run-1 wake");
    assert_eq!(claimed.request.run_id().0, "run-1");
    assert_eq!(claimed.pending.len(), 1);
    assert_eq!(claimed.pending[0].message_id, "m-wake");
}

/// G5-T6 (cause-effect graphing): two workers RACE to recover the SAME expired lease.
/// Exactly one wins the epoch bump; the other finds nothing runnable — never double
/// ownership. The in-memory store serializes via its mutex, sqlite/postgres via row
/// locking, so the single-winner invariant holds on every backend.
pub async fn assert_concurrent_recovery_yields_one_winner<S>(
    store: Arc<S>,
    clock: &dyn ConformanceClock,
) where
    S: awaken_run_ingress::Dispatch + Send + Sync + 'static,
{
    use awaken_run_ingress::RunDispatch;
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // Owner A claims with a short lease (ttl 100 from t=0); it has lapsed by t=200.
    let a = store
        .claim("owner-a", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    assert_eq!(a.lease.epoch, 1);

    // Two workers race to recover the one expired lease at t=200.
    clock.advance_past(a.lease.expires_ms).await;
    let recovery_now = a.lease.expires_ms.saturating_add(1);
    let s1 = store.clone();
    let s2 = store.clone();
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            s1.claim("owner-b", 100, recovery_now, &Default::default())
                .await
        }),
        tokio::spawn(async move {
            s2.claim("owner-c", 100, recovery_now, &Default::default())
                .await
        }),
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

/// Shared spec: a `Awaiting` settle is fenced the same way — a stale owner cannot
/// re-await (and reset the crash-retry budget / clear the lease) behind a reclaimer.
pub async fn assert_awaiting_settle_fences_stale_epoch<S: awaken_run_ingress::Dispatch>(
    store: &S,
    clock: &dyn ConformanceClock,
) {
    use awaken_run_ingress::{DispatchOutcome, RunDispatch, SettleOutcome};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    let a = store
        .claim("owner-a", 100, 0, &Default::default())
        .await
        .unwrap()
        .expect("A claims");
    clock.advance_past(a.lease.expires_ms).await;
    let b = store
        .claim(
            "owner-b",
            100,
            a.lease.expires_ms.saturating_add(1),
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("B reclaims");
    assert!(b.lease.epoch > a.lease.epoch);

    // A's stale Awaiting settle is fenced: it must not clear B's lease or reset the
    // attempt budget of the row B is actively running.
    assert_eq!(
        store
            .settle(&run, a.lease.epoch, DispatchOutcome::Awaiting, &[])
            .await
            .unwrap(),
        SettleOutcome::Fenced,
    );
    assert!(
        store
            .claim(
                "owner-c",
                100,
                b.lease.expires_ms.saturating_sub(1),
                &Default::default(),
            )
            .await
            .unwrap()
            .is_none(),
        "the fenced Awaiting settle left B's running claim intact"
    );

    // B awaits under the current epoch: applied, and the run is now wakeable.
    assert_eq!(
        store
            .settle(&run, b.lease.epoch, DispatchOutcome::Awaiting, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied,
    );
}
