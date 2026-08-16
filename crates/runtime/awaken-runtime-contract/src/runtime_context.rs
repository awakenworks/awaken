//! Per-attempt live wiring kept separate from durable activation data.
//!
//! `RunActivation` is immutable, serializable run input; `RuntimeRunContext`
//! carries the process-local handles for one execution attempt — the stream
//! sink, the commit coordinator, and the cancellation token (runtime-behavior.md
//! role catalog; G2/G3). None of these may appear in `RunActivation`.
//!
//! This type belongs in the contract, not an implementation crate, because it is
//! the *parameter object* of the `RunExecutor` port (`execution::RunExecutor`):
//! every executor — native, ACP, A2A — receives one by value. Relocating it would
//! make the contract depend on the implementation crate through its own port
//! signature (a cycle). Its handles (including `live_inbox`) are neutral
//! in-process mechanism, never external infrastructure — so keeping them here is
//! a port carrying its own vocabulary, not a leak.

use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use tokio_util::sync::CancellationToken;

use crate::capture::CaptureDecision;
use crate::data_subject::{CaptureSink, DataSubjectId};
use crate::live_inbox::LiveInbox;
use crate::pause::PauseSignal;
use crate::permission::ToolPermissionPolicy;
use crate::terminal::RunTerminalObserver;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use thiserror::Error;

/// The only two destinations involved when an attempt is assembled. A
/// transcript prefix is execution input for the current model request; it is
/// never a candidate for the durable Thread append owned by the commit path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptContextDestination {
    ModelRequest,
    DurableThreadTruth,
}

/// Canonical request-context projection selector shared by every Runtime path.
/// Keeping this as a closed, allocation-free kernel makes the non-persistence
/// rule directly provable rather than relying on a comment at each caller.
#[must_use]
pub const fn transcript_prefix_projects_to(destination: TranscriptContextDestination) -> bool {
    matches!(destination, TranscriptContextDestination::ModelRequest)
}

#[cfg(kani)]
#[kani::proof]
fn transcript_prefix_is_exact_request_only_context_not_durable_truth() {
    let destination = if kani::any() {
        TranscriptContextDestination::ModelRequest
    } else {
        TranscriptContextDestination::DurableThreadTruth
    };
    let projected = transcript_prefix_projects_to(destination);
    assert_eq!(
        projected,
        destination == TranscriptContextDestination::ModelRequest
    );
    if destination == TranscriptContextDestination::DurableThreadTruth {
        assert!(!projected);
    }
}

/// Why a live attempt may no longer proceed under its dispatch ownership.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AttemptOwnershipError {
    /// The claim was settled, expired, or superseded by another attempt.
    #[error("attempt ownership was lost")]
    Lost,
    /// The ownership authority could not be reached or evaluated. Callers must
    /// fail closed rather than treating this as current ownership.
    #[error("attempt ownership is unavailable: {0}")]
    Unavailable(String),
}

/// Neutral live check that the current attempt still owns its execution slot.
///
/// Dispatch adapters capture their claim, clock, and transport behind this port;
/// Runtime and application decorators therefore never receive a lease epoch,
/// Worker registry, database, or HTTP type.
#[async_trait::async_trait]
pub trait AttemptOwnershipVerifier: Send + Sync {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError>;
}

/// The content-capture wiring for one attempt (ADR-0050): the resolved decision
/// (level + redactor) plus, when content persistence is on, the subject it is
/// attributed to and the sink it is written to. Grouped so the privacy cluster
/// travels as one cohesive unit rather than three loose context fields.
#[derive(Clone, Default)]
pub struct CaptureContext {
    /// The resolved capture decision (level + redactor). Default is `Structured`
    /// (no content); the host resolves the real decision per Run.
    pub decision: CaptureDecision,
    /// The data subject captured content is attributed to (opaque). Content is
    /// written only when this, `sink`, and a content-permitting level all hold.
    pub subject: Option<DataSubjectId>,
    /// Where captured content is written (subject-tagged, erasable). Best-effort;
    /// absent means content is recorded to spans only, not a queryable store.
    pub sink: Option<Arc<dyn CaptureSink>>,
}

#[derive(Clone, Default)]
pub struct RuntimeRunContext {
    /// Authorized Workspace ownership for this attempt. The ingress/session
    /// boundary supplies the same [`awaken_tenancy::ExecutionScopeRef`] that is
    /// persisted on durable dispatch; tools may consume it as trusted execution
    /// context, but models and provider-authored arguments can never set it.
    pub execution_scope: Option<awaken_tenancy::ExecutionScopeRef>,
    /// Request-only context assembled for this attempt. Executors may project
    /// these messages into the model request, but must never append them to the
    /// durable Thread transcript.
    pub request_context: Vec<Message>,
    /// Live best-effort progress delivery; absent means no live streaming.
    pub stream_sink: Option<Arc<dyn StreamSink>>,
    /// Durable write boundary for this attempt; absent means no persistence.
    pub commit: Option<Arc<dyn CommitCoordinator>>,
    /// Reactions to an already-committed terminal Run. Delivery is at-least-once:
    /// stable-id recovery may invoke an observer again, so observers own durable
    /// idempotent intents/receipts. Awaiting Runs are never delivered.
    pub terminal_observers: Vec<Arc<dyn RunTerminalObserver>>,
    /// Durable snapshot store for an interrupted inference stream. When set, the
    /// engine flushes the in-flight partial at an interruption boundary so a
    /// later process resumes mid-step instead of re-running it; absent means an
    /// interrupted step is recovered in-process only and lost on a crash.
    pub stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    /// Committed-history read port. When set, a fresh run seeds its transcript
    /// with the Thread's committed messages, so a new Run continues the
    /// conversation; absent means the run starts from its input alone.
    pub reader: Option<Arc<dyn CommittedThreadView>>,
    /// Cooperative cancellation observed at step boundaries.
    pub cancellation: Option<CancellationToken>,
    /// Cooperative pause observed at safe loop boundaries (ADR-0054). When set and
    /// requested, the next boundary awaits the run (`AwaitReason::ManualPause`)
    /// instead of continuing — an operator pause, never a mid-step freeze. Absent
    /// means the attempt cannot be paused in flight.
    pub pause: Option<PauseSignal>,
    /// Live input-direction mirror of `stream_sink`: an editable in-process
    /// queue the engine drains at safe loop boundaries; absent means the
    /// attempt accepts no mid-run input. Best-effort like the sink — the
    /// durable pending-input path stays the at-least-once channel.
    pub live_inbox: Option<LiveInbox>,
    /// Where this attempt's tool calls run (ADR-0044). Absent means the runtime
    /// executes tools in-process by id (the `LocalToolExecutor` degenerate case);
    /// present routes every already-gated call through this port — e.g. a remote
    /// hand. The kernel never learns placement; it calls the port either way.
    pub tool_executor: Option<Arc<dyn crate::tool::ToolExecutor>>,
    /// Host-owned materializer for model-visible tool results. Native and ACP
    /// executors both consult this after execution/projection and before commit,
    /// so oversized content has one sandbox-backed policy rather than per-tool
    /// truncation paths.
    pub tool_output_spiller: Option<Arc<dyn crate::tool::ToolOutputSpiller>>,
    /// Optional per-Run narrowing of the backend's tool permission authority.
    /// External runtimes use this for purpose-specific fail-closed overlays; it
    /// may restrict the configured policy but is never a capability grant.
    pub tool_permission_policy: Option<Arc<dyn ToolPermissionPolicy>>,
    /// The model executor to use for THIS attempt, overriding the runtime's bound
    /// default. Absent means use the runtime's session-resolved executor. Present
    /// routes this attempt's inference through the given executor — the run's model,
    /// resolved to a provider at the resolve seam (which owns how the model is
    /// reached: local credentials or a gateway offering). Symmetric with
    /// `tool_executor`: a per-run egress override the kernel consults without learning
    /// why it was chosen.
    pub model_executor: Option<Arc<dyn crate::llm::LlmExecutor>>,
    /// Attempt-bound resolver for logical model-visible content such as Files
    /// catalog ids. It runs once per candidate request before provider retry,
    /// so retries reuse the exact immutable bytes and cannot race later reads.
    pub model_content_materializer: Option<Arc<dyn crate::llm::ModelContentMaterializer>>,
    /// The content-capture wiring for this attempt (ADR-0050 D5): the resolved
    /// decision (level + redactor) gating what prompt/completion/tool content the
    /// engine records, plus the subject + sink it is attributed to and written to.
    pub capture: CaptureContext,
    /// A transient-retry counter the inference seam increments each time it
    /// transparently retries a retryable failure during this attempt. The host
    /// reads it after the run to surface `session.status_rescheduled` (auto-recovery
    /// observability). Absent means retries are not counted — optional wiring, like
    /// the stream sink; the retry behavior itself is unchanged either way.
    pub reschedules: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Completed logical model requests for this live attempt. This
    /// process-local observability seam is inherited by in-process delegated
    /// children so their requests are visible at the owning Session boundary.
    pub model_requests: Option<Arc<std::sync::Mutex<Vec<crate::llm::ModelRequestObservation>>>>,
    /// Stable Run ids that performed at least one transparent provider retry.
    /// In-process delegated children inherit this shared set.
    pub rescheduled_runs: Option<Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>>,
    /// Claim-bound live authority for this execution attempt. Application
    /// decorators may recheck it immediately before an external side effect.
    /// Absence means the ingress topology has no dispatch ownership concept.
    pub ownership: Option<Arc<dyn AttemptOwnershipVerifier>>,
    /// Secret-free projection of the credential decisions already committed by
    /// this attempt's dispatch claim, plus its claim-fenced receipt port. This is
    /// live wiring, never an alternative durable authority.
    pub credential_realization: Option<crate::AttemptCredentialRealization>,
    /// Process-local plugins bound by the realized Session rather than authored
    /// into the immutable Agent publication. This is the live-wiring seam for
    /// dynamically discovered capabilities such as a claim-prepared MCP server:
    /// the publication remains frozen while the plugin still passes through the
    /// runtime's ordinary capability-bound merge, tool presentation, and
    /// execution path.
    pub session_plugins: Vec<Arc<dyn crate::plugin::Plugin>>,
}

impl RuntimeRunContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the Workspace ownership already verified by Session admission.
    #[must_use]
    pub fn with_execution_scope(mut self, scope: awaken_tenancy::ExecutionScopeRef) -> Self {
        self.execution_scope = Some(scope);
        self
    }

    /// Derive the process-local wiring for a child Run initiated by this Run.
    ///
    /// A delegated child is an ordinary Agent Run, so it inherits the same
    /// persistence, execution, observability, and capture capabilities. The two
    /// exceptions are handles whose identity belongs to one live Run:
    ///
    /// - cancellation uses a child token, giving parent → child propagation
    ///   without allowing a child cancellation to cancel its parent;
    /// - live input, live output, and pause handles are not inherited because
    ///   their address is the parent Run; a separately controlled child receives
    ///   its own handles from ingress.
    ///
    /// A separately dispatched child receives its own context from ingress; this
    /// method is the in-process equivalent used by native delegation.
    #[must_use]
    pub fn for_child_run(&self) -> Self {
        let mut child = self.clone();
        child.request_context.clear();
        child.cancellation = self
            .cancellation
            .as_ref()
            .map(CancellationToken::child_token);
        child.stream_sink = None;
        child.pause = None;
        child.live_inbox = None;
        // A delegated child owns another Run/claim epoch and must receive its own
        // bindings from ingress rather than inheriting the parent's authority.
        child.credential_realization = None;
        child.model_content_materializer = None;
        child
    }

    /// Bind a neutral current-attempt ownership check.
    #[must_use]
    pub fn with_ownership(mut self, ownership: Arc<dyn AttemptOwnershipVerifier>) -> Self {
        self.ownership = Some(ownership);
        self
    }

    /// Bind the immutable claim-epoch credential decisions and receipt writer.
    #[must_use]
    pub fn with_credential_realization(
        mut self,
        realization: crate::AttemptCredentialRealization,
    ) -> Self {
        self.credential_realization = Some(realization);
        self
    }

    /// Bind the one model-content resolution boundary for this attempt.
    #[must_use]
    pub fn with_model_content_materializer(
        mut self,
        materializer: Arc<dyn crate::llm::ModelContentMaterializer>,
    ) -> Self {
        self.model_content_materializer = Some(materializer);
        self
    }

    /// Bind one plugin supplied by the realized Session environment.
    #[must_use]
    pub fn with_session_plugin(mut self, plugin: Arc<dyn crate::plugin::Plugin>) -> Self {
        self.session_plugins.push(plugin);
        self
    }

    #[must_use]
    pub fn with_stream_sink(mut self, sink: Arc<dyn StreamSink>) -> Self {
        self.stream_sink = Some(sink);
        self
    }

    #[must_use]
    pub fn with_commit(mut self, commit: Arc<dyn CommitCoordinator>) -> Self {
        self.commit = Some(commit);
        self
    }

    /// Add a committed-terminal observer for this Run. Observers are inherited
    /// by in-process delegated child Runs because those children are ordinary
    /// Runs sharing the same runtime extension composition.
    #[must_use]
    pub fn with_terminal_observer(mut self, observer: Arc<dyn RunTerminalObserver>) -> Self {
        self.terminal_observers.push(observer);
        self
    }

    /// Provide the durable checkpoint store so an interrupted inference stream
    /// survives a process crash and resumes mid-step in a later process.
    #[must_use]
    pub fn with_stream_checkpoint(mut self, store: Arc<dyn StreamCheckpointStore>) -> Self {
        self.stream_checkpoint = Some(store);
        self
    }

    /// Provide the committed-history read port so a fresh run continues the
    /// thread's conversation. Usually the same store as `commit`.
    #[must_use]
    pub fn with_reader(mut self, reader: Arc<dyn CommittedThreadView>) -> Self {
        self.reader = Some(reader);
        self
    }

    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// Provide the transient-retry counter the inference seam increments on each
    /// transparent retry, so the host can report `session.status_rescheduled`.
    #[must_use]
    pub fn with_reschedules(mut self, counter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.reschedules = Some(counter);
        self
    }

    /// Collect completed logical model requests for protocol observability.
    #[must_use]
    pub fn with_model_requests(
        mut self,
        observations: Arc<std::sync::Mutex<Vec<crate::llm::ModelRequestObservation>>>,
    ) -> Self {
        self.model_requests = Some(observations);
        self
    }

    #[must_use]
    pub fn with_rescheduled_runs(
        mut self,
        runs: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    ) -> Self {
        self.rescheduled_runs = Some(runs);
        self
    }

    /// Provide the pause signal so an operator can await this attempt at its next
    /// safe boundary (ADR-0054).
    #[must_use]
    pub fn with_pause(mut self, pause: PauseSignal) -> Self {
        self.pause = Some(pause);
        self
    }

    #[must_use]
    pub fn with_live_inbox(mut self, inbox: LiveInbox) -> Self {
        self.live_inbox = Some(inbox);
        self
    }

    /// Route this attempt's tool calls through `executor` (e.g. a remote hand)
    /// instead of the in-process registry (ADR-0044 D1).
    #[must_use]
    pub fn with_tool_executor(mut self, executor: Arc<dyn crate::tool::ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    /// Bind the Session's one tool-output materialization policy.
    #[must_use]
    pub fn with_tool_output_spiller(
        mut self,
        spiller: Arc<dyn crate::tool::ToolOutputSpiller>,
    ) -> Self {
        self.tool_output_spiller = Some(spiller);
        self
    }

    #[must_use]
    pub fn with_tool_permission_policy(mut self, policy: Arc<dyn ToolPermissionPolicy>) -> Self {
        self.tool_permission_policy = Some(policy);
        self
    }

    /// Override this attempt's model executor with the run's model resolved to a
    /// provider at the resolve seam, instead of the runtime's bound default. The
    /// single per-run egress seam a database-less worker uses.
    #[must_use]
    pub fn with_model_executor(mut self, executor: Arc<dyn crate::llm::LlmExecutor>) -> Self {
        self.model_executor = Some(executor);
        self
    }

    /// Set the resolved content-capture decision for this attempt (ADR-0050).
    #[must_use]
    pub fn with_capture(mut self, capture: CaptureDecision) -> Self {
        self.capture.decision = capture;
        self
    }

    /// Retain request attribution even when this Worker has no persistence sink.
    /// This keeps durable resume identity independent of capture deployment.
    #[must_use]
    pub fn with_data_subject(mut self, subject: DataSubjectId) -> Self {
        self.capture.subject = Some(subject);
        self
    }

    /// Attribute this attempt's captured content to `subject` and write it to
    /// `sink` (ADR-0050). Both are needed for the engine to persist content.
    #[must_use]
    pub fn with_capture_sink(mut self, subject: DataSubjectId, sink: Arc<dyn CaptureSink>) -> Self {
        self.capture.subject = Some(subject);
        self.capture.sink = Some(sink);
        self
    }

    /// The subject + sink to persist captured content to, when BOTH are set.
    #[must_use]
    pub fn content_sink(&self) -> Option<(&Arc<dyn CaptureSink>, &DataSubjectId)> {
        self.capture
            .sink
            .as_ref()
            .zip(self.capture.subject.as_ref())
    }

    /// True once cancellation has been requested for this attempt.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    /// True once an operator pause has been requested for this attempt (ADR-0054).
    /// Observed only at safe loop boundaries.
    pub fn is_pause_requested(&self) -> bool {
        self.pause.as_ref().is_some_and(PauseSignal::requested)
    }
}

#[cfg(test)]
mod child_run_tests {
    use super::*;

    #[test]
    fn child_run_inherits_capabilities_but_not_parent_live_input() {
        let pause = PauseSignal::new();
        let parent = RuntimeRunContext::new()
            .with_pause(pause)
            .with_live_inbox(LiveInbox::new());

        let child = parent.for_child_run();

        assert!(parent.pause.is_some());
        assert!(child.pause.is_none());
        assert!(parent.live_inbox.is_some());
        assert!(child.live_inbox.is_none());
    }

    #[test]
    fn cancellation_propagates_only_from_parent_to_child() {
        let parent_token = CancellationToken::new();
        let parent = RuntimeRunContext::new().with_cancellation(parent_token.clone());
        let child = parent.for_child_run();
        let child_token = child.cancellation.expect("child cancellation token");

        child_token.cancel();
        assert!(
            !parent_token.is_cancelled(),
            "child must not cancel its parent"
        );

        let second_child = parent.for_child_run();
        parent_token.cancel();
        assert!(
            second_child
                .cancellation
                .expect("second child cancellation token")
                .is_cancelled(),
            "parent cancellation must reach every child"
        );
    }
}
