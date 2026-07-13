//! The durable/live split for one durable execution attempt.
//!
//! [`RunExecutionRequest`] is the serializable instruction a durable queue
//! persists and replays — it carries no live handles (G3/G4), so a crash loses
//! nothing the queue cannot rebuild. [`RunExecutionContext`] is the per-attempt
//! live wiring (commit boundary, optional stream sink) the host recreates each
//! time it runs a request; it is additive over runtime control and never owns
//! the loop (G6).

use std::sync::Arc;

use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::store::stream_checkpoint::StreamCheckpointStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::live_inbox::LiveInbox;
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use tokio_util::sync::CancellationToken;

// The serializable durable-run instruction moved to the dispatch contract
// (ADR-0039 2.1); re-exported so `crate::request::RunExecutionRequest` is stable.
pub use awaken_run_ingress_contract::request::RunExecutionRequest;

/// Per-attempt live wiring. The durable host holds one of these and rebuilds a
/// [`RuntimeRunContext`] for every execute/resume attempt, binding the run to the
/// durable commit boundary (the single write authority, G1/G13) and the optional
/// best-effort stream sink. Cancellation is created per attempt by the worker.
#[derive(Clone)]
pub struct RunExecutionContext {
    commit: Arc<dyn CommitCoordinator>,
    reader: Option<Arc<dyn ThreadReader>>,
    stream_sink: Option<Arc<dyn StreamSink>>,
    stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    /// The per-session live inbox a worker-driven run drains at safe loop
    /// boundaries (ADR-0054 P2). Absent means the durable path accepts no
    /// mid-run steer — the pre-P2 behaviour. Neutral: the worker never learns a
    /// protocol; it drains folded `Message`s like the direct path.
    live_inbox: Option<LiveInbox>,
}

impl RunExecutionContext {
    /// Wire an attempt to its durable commit boundary.
    pub fn new(commit: Arc<dyn CommitCoordinator>) -> Self {
        Self {
            commit,
            reader: None,
            stream_sink: None,
            stream_checkpoint: None,
            live_inbox: None,
        }
    }

    /// Provide the per-session live inbox so worker-driven runs drain mid-run
    /// steer at their boundaries (ADR-0054 P2). The same neutral inbox the offer
    /// side reaches, so steer/redirect works on the durable path.
    #[must_use]
    pub fn with_live_inbox(mut self, inbox: LiveInbox) -> Self {
        self.live_inbox = Some(inbox);
        self
    }

    /// Provide the committed-history read port so a fresh run continues the
    /// thread's conversation. Usually the same store as the commit.
    #[must_use]
    pub fn with_reader(mut self, reader: Arc<dyn ThreadReader>) -> Self {
        self.reader = Some(reader);
        self
    }

    /// Attach a best-effort live stream sink (live progress is never truth).
    #[must_use]
    pub fn with_stream_sink(mut self, sink: Arc<dyn StreamSink>) -> Self {
        self.stream_sink = Some(sink);
        self
    }

    /// Attach the durable interrupted-stream checkpoint store (Phase 3), so a
    /// dispatch re-executed after a crash resumes its in-flight step from the
    /// flushed partial instead of re-running it.
    #[must_use]
    pub fn with_stream_checkpoint(mut self, store: Arc<dyn StreamCheckpointStore>) -> Self {
        self.stream_checkpoint = Some(store);
        self
    }

    /// The durable commit boundary this context writes through.
    pub fn commit(&self) -> &Arc<dyn CommitCoordinator> {
        &self.commit
    }

    /// Build the runtime-facing context for one attempt, carrying the supplied
    /// cancellation token so the host can steer an in-flight run.
    pub(crate) fn runtime_context(&self, cancel: CancellationToken) -> RuntimeRunContext {
        let mut context = RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_cancellation(cancel)
            // A fresh pause signal per attempt, registered by the executor so live
            // control can park this run at its next safe boundary (ADR-0054).
            .with_pause(PauseSignal::new());
        if let Some(reader) = &self.reader {
            context = context.with_reader(reader.clone());
        }
        if let Some(sink) = &self.stream_sink {
            context = context.with_stream_sink(sink.clone());
        }
        if let Some(store) = &self.stream_checkpoint {
            context = context.with_stream_checkpoint(store.clone());
        }
        if let Some(inbox) = &self.live_inbox {
            context = context.with_live_inbox(inbox.clone());
        }
        context
    }
}
