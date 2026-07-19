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

use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use tokio_util::sync::CancellationToken;

use crate::capture::CaptureDecision;
use crate::data_subject::{CaptureSink, DataSubjectId};
use crate::live_inbox::LiveInbox;
use crate::pause::PauseSignal;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;

/// The content-capture wiring for one attempt (ADR-0050): the resolved decision
/// (level + redactor) plus, when content persistence is on, the subject it is
/// attributed to and the sink it is written to. Grouped so the privacy cluster
/// travels as one cohesive unit rather than three loose context fields.
#[derive(Clone, Default)]
pub struct CaptureContext {
    /// The resolved capture decision (level + redactor). Default is `Structured`
    /// (no content); the host resolves the real decision per run/turn.
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
    /// Live best-effort progress delivery; absent means no live streaming.
    pub stream_sink: Option<Arc<dyn StreamSink>>,
    /// Durable write boundary for this attempt; absent means no persistence.
    pub commit: Option<Arc<dyn CommitCoordinator>>,
    /// Durable snapshot store for an interrupted inference stream. When set, the
    /// engine flushes the in-flight partial at an interruption boundary so a
    /// later process resumes mid-step instead of re-running it; absent means an
    /// interrupted step is recovered in-process only and lost on a crash.
    pub stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    /// Committed-history read port. When set, a fresh run seeds its transcript
    /// with the thread's committed messages, so a new turn continues the
    /// conversation; absent means the run starts from its input alone.
    pub reader: Option<Arc<dyn ThreadReader>>,
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
    /// The model executor to use for THIS attempt, overriding the runtime's bound
    /// default. Absent means use the runtime's session-resolved executor. Present
    /// routes this attempt's inference through the given executor — the run's model,
    /// resolved to a provider at the resolve seam (which owns how the model is
    /// reached: local credentials or a gateway offering). Symmetric with
    /// `tool_executor`: a per-run egress override the kernel consults without learning
    /// why it was chosen.
    pub model_executor: Option<Arc<dyn crate::llm::LlmExecutor>>,
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
}

impl RuntimeRunContext {
    pub fn new() -> Self {
        Self::default()
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
        child.cancellation = self
            .cancellation
            .as_ref()
            .map(CancellationToken::child_token);
        child.stream_sink = None;
        child.pause = None;
        child.live_inbox = None;
        child
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
    pub fn with_reader(mut self, reader: Arc<dyn ThreadReader>) -> Self {
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
