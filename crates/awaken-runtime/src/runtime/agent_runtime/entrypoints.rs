use std::sync::Arc;

use awaken_runtime_contract::contract::event_sink::{EventSink, NullEventSink};

use super::AgentRuntime;
use crate::loop_runner::{AgentLoopError, AgentRunResult};
use crate::resolution::{ReplayableScope, ResolvedRun, ResolvedRunPlan};
use crate::run::{RunActivation, ThreadContextSnapshot};

impl AgentRuntime {
    /// Run an agent loop until it returns an [`AgentRunResult`].
    ///
    /// This is a convenience wrapper for one-shot CLI programs and examples
    /// that only need the final [`AgentRunResult`]. Use [`Self::run`] with an
    /// [`EventSink`] when streaming events to SSE, WebSocket, protocol adapters,
    /// or tests.
    pub async fn run_to_completion(
        &self,
        activation: RunActivation,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run(activation, Arc::new(NullEventSink)).await
    }

    /// Run an agent loop.
    ///
    /// This is the legacy live entry point. Persistent server dispatch first
    /// resolves the activation and then calls [`Self::run_replayable`].
    pub async fn run(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(activation, sink, None, None).await
    }

    /// Run an activation using a replayable plan produced by
    /// [`Self::resolve_activation`] with `ResolutionPolicy::PersistentServer`.
    pub async fn run_replayable(
        &self,
        activation: RunActivation,
        plan: ResolvedRun<ReplayableScope>,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(
            activation,
            sink,
            None,
            Some(ResolvedRunPlan::Replayable(plan)),
        )
        .await
    }

    /// Run an activation using an explicit resolved plan. Embedded/live callers
    /// may pass a live-only plan; persistent callers must use
    /// [`Self::run_replayable`].
    pub async fn run_live(
        &self,
        activation: RunActivation,
        plan: ResolvedRunPlan,
        sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(activation, sink, None, Some(plan)).await
    }

    #[doc(hidden)]
    pub async fn run_with_thread_context(
        &self,
        activation: RunActivation,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(activation, sink, thread_ctx, None).await
    }

    #[doc(hidden)]
    pub async fn run_replayable_with_thread_context(
        &self,
        activation: RunActivation,
        plan: ResolvedRun<ReplayableScope>,
        sink: Arc<dyn EventSink>,
        thread_ctx: Option<ThreadContextSnapshot>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        self.run_inner(
            activation,
            sink,
            thread_ctx,
            Some(ResolvedRunPlan::Replayable(plan)),
        )
        .await
    }
}
