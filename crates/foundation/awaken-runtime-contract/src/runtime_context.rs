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

use crate::live_inbox::LiveInbox;
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
    /// Live input-direction mirror of `stream_sink`: an editable in-process
    /// queue the engine drains at safe loop boundaries; absent means the
    /// attempt accepts no mid-run input. Best-effort like the sink — the
    /// durable pending-input path stays the at-least-once channel.
    pub live_inbox: Option<LiveInbox>,
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

    #[must_use]
    pub fn with_live_inbox(mut self, inbox: LiveInbox) -> Self {
        self.live_inbox = Some(inbox);
        self
    }

    /// True once cancellation has been requested for this attempt.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}
