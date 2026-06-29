//! Per-attempt live wiring kept separate from durable activation data.
//!
//! `RunActivation` is immutable, serializable run input; `RuntimeRunContext`
//! carries the process-local handles for one execution attempt — the stream
//! sink, the commit coordinator, and the cancellation token (runtime-behavior.md
//! role catalog; G2/G3). None of these may appear in `RunActivation`.

use std::sync::Arc;

use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use tokio_util::sync::CancellationToken;

use crate::activation::PersistenceMode;

#[derive(Clone, Default)]
pub struct RuntimeRunContext {
    pub persistence: PersistenceMode,
    /// Live best-effort progress delivery; absent means no live streaming.
    pub stream_sink: Option<Arc<dyn StreamSink>>,
    /// Durable write boundary for this attempt; absent means no persistence.
    pub commit: Option<Arc<dyn CommitCoordinator>>,
    /// Cooperative cancellation observed at step boundaries.
    pub cancellation: Option<CancellationToken>,
}

impl RuntimeRunContext {
    pub fn new(persistence: PersistenceMode) -> Self {
        Self {
            persistence,
            ..Default::default()
        }
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

    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// True once cancellation has been requested for this attempt.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}
