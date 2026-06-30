//! The durable/live split for one durable execution attempt.
//!
//! [`RunExecutionRequest`] is the serializable instruction a durable queue
//! persists and replays — it carries no live handles (G3/G4), so a crash loses
//! nothing the queue cannot rebuild. [`RunExecutionContext`] is the per-attempt
//! live wiring (commit boundary, optional stream sink) the host recreates each
//! time it runs a request; it is additive over runtime control and never owns
//! the loop (G6).

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// The durable, serializable record of an accepted run. It is exactly the data
/// a durable dispatch queue stores and replays; it holds no `Arc<dyn ...>`,
/// registry, or live handle (G3). The runtime builds live execution objects
/// from the activation's pinned snapshot on each attempt (G4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunExecutionRequest {
    pub activation: RunActivation,
}

impl RunExecutionRequest {
    pub fn new(activation: RunActivation) -> Self {
        Self { activation }
    }

    pub fn run_id(&self) -> &RunId {
        &self.activation.run_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.activation.thread_id
    }
}

/// Per-attempt live wiring. The durable host holds one of these and rebuilds a
/// [`RuntimeRunContext`] for every execute/resume attempt, binding the run to the
/// durable commit boundary (the single write authority, G1/G13) and the optional
/// best-effort stream sink. Cancellation is created per attempt by the worker.
#[derive(Clone)]
pub struct RunExecutionContext {
    commit: Arc<dyn CommitCoordinator>,
    stream_sink: Option<Arc<dyn StreamSink>>,
}

impl RunExecutionContext {
    /// Wire an attempt to its durable commit boundary.
    pub fn new(commit: Arc<dyn CommitCoordinator>) -> Self {
        Self {
            commit,
            stream_sink: None,
        }
    }

    /// Attach a best-effort live stream sink (live progress is never truth).
    #[must_use]
    pub fn with_stream_sink(mut self, sink: Arc<dyn StreamSink>) -> Self {
        self.stream_sink = Some(sink);
        self
    }

    /// The durable commit boundary this context writes through.
    pub fn commit(&self) -> &Arc<dyn CommitCoordinator> {
        &self.commit
    }

    /// Build the runtime-facing context for one attempt, carrying the supplied
    /// cancellation token so the host can steer an in-flight run.
    pub(crate) fn runtime_context(&self, cancel: CancellationToken) -> RuntimeRunContext {
        let mut context = RuntimeRunContext::new(PersistenceMode::ReadWrite)
            .with_commit(self.commit.clone())
            .with_cancellation(cancel);
        if let Some(sink) = &self.stream_sink {
            context = context.with_stream_sink(sink.clone());
        }
        context
    }
}
