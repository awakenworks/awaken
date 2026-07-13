//! Per-attempt live wiring kept separate from durable activation data.
//!
//! `RunActivation` is immutable, serializable run input; `RuntimeRunContext`
//! carries the process-local handles for one execution attempt — the stream
//! sink, the commit coordinator, and the cancellation token (runtime-behavior.md
//! role catalog; G2/G3). None of these may appear in `RunActivation`.

use std::sync::Arc;

use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use tokio_util::sync::CancellationToken;

use crate::capture::CaptureDecision;
use crate::data_subject::{CaptureSink, DataSubjectId};
use crate::live_inbox::LiveInbox;
use crate::pause::PauseSignal;
use awaken_agent_contract::store::stream_checkpoint::StreamCheckpointStore;

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
    /// requested, the next boundary parks the run (`WaitingReason::ManualPause`)
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
    /// The resolved content-capture decision for this attempt (ADR-0050 D5):
    /// the level plus redactor gating what prompt/completion/tool content the
    /// engine records onto telemetry. Default is `Structured` (no content); the
    /// host resolves the real decision per run/turn and sets it here.
    pub capture: CaptureDecision,
    /// The data subject this attempt's content is attributed to (ADR-0050),
    /// opaque. Only when both this and `capture_sink` are set — and the capture
    /// level permits content — does the engine write captured content.
    pub data_subject: Option<DataSubjectId>,
    /// Where captured content is written (subject-tagged, erasable). Best-effort;
    /// absent means content is recorded to spans only, not a queryable store.
    pub capture_sink: Option<Arc<dyn CaptureSink>>,
}

impl RuntimeRunContext {
    pub fn new() -> Self {
        Self::default()
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

    /// Provide the pause signal so an operator can park this attempt at its next
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

    /// Set the resolved content-capture decision for this attempt (ADR-0050).
    #[must_use]
    pub fn with_capture(mut self, capture: CaptureDecision) -> Self {
        self.capture = capture;
        self
    }

    /// Attribute this attempt's captured content to `subject` and write it to
    /// `sink` (ADR-0050). Both are needed for the engine to persist content.
    #[must_use]
    pub fn with_capture_sink(mut self, subject: DataSubjectId, sink: Arc<dyn CaptureSink>) -> Self {
        self.data_subject = Some(subject);
        self.capture_sink = Some(sink);
        self
    }

    /// The subject + sink to persist captured content to, when BOTH are set.
    #[must_use]
    pub fn content_sink(&self) -> Option<(&Arc<dyn CaptureSink>, &DataSubjectId)> {
        self.capture_sink.as_ref().zip(self.data_subject.as_ref())
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
