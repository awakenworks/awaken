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
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use tokio_util::sync::CancellationToken;

/// Resolves a run's effective model ref to its executor (R1), so a database-less
/// worker builds the run's configured model with no config service and no
/// per-session binding — the host injects a closure wrapping its `ExecutorProvider`.
/// `None` = no provider or an unresolved ref, so the run falls back to the runtime's
/// bound (host default) executor. This is the *provider* seam: the worker names a
/// model and gets back an executor, never learning how the model is reached.
pub type ModelResolverFn = Arc<dyn Fn(&str) -> Option<Arc<dyn LlmExecutor>> + Send + Sync>;

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
    /// Resolves a run's effective model ref to its executor (R1), so a database-less
    /// worker builds the run's configured model per attempt. Absent means the run
    /// falls back to the runtime's bound (host default) executor — the pre-provider
    /// single-model behaviour.
    model_resolver: Option<ModelResolverFn>,
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
            model_resolver: None,
        }
    }

    /// Install the model→executor resolver (R1): a worker-driven run resolves its
    /// effective model ref (override, else its snapshot binding) to an executor
    /// through this, so a database-less worker runs the configured model without a
    /// config service. The worker names a model and gets an executor — it never
    /// learns *how* the model is reached.
    #[must_use]
    pub fn with_model_resolver(mut self, resolve: ModelResolverFn) -> Self {
        self.model_resolver = Some(resolve);
        self
    }

    /// Resolve `model_ref` to its executor via the injected provider, or `None` to
    /// fall back to the runtime's bound default (no provider / unresolved ref).
    pub(crate) fn resolve_model(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        self.model_resolver
            .as_ref()
            .and_then(|resolve| resolve(model_ref))
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

#[cfg(test)]
mod resolve_seam_tests {
    //! Cause-effect coverage for the per-run model resolve seam a database-less
    //! worker carries. Cause: a model resolver is injected or not, and (when it is)
    //! resolves the ref or declines. Effect: `resolve_model` returns the provider's
    //! executor, else `None` — and `None` leaves the run on the runtime's bound
    //! (host default) executor, so a single-model deployment is unaffected.
    use std::sync::Arc;

    use awaken_runtime::memory::MemoryCommitCoordinator;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};

    use super::RunExecutionContext;

    struct Labeled(&'static str);
    #[async_trait::async_trait]
    impl LlmExecutor for Labeled {
        async fn infer(
            &self,
            _r: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text(self.0),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn ctx() -> RunExecutionContext {
        RunExecutionContext::new(Arc::new(MemoryCommitCoordinator::new()))
    }

    #[test]
    fn returns_the_injected_providers_executor() {
        let labeled: Arc<dyn LlmExecutor> = Arc::new(Labeled("resolved"));
        let l = labeled.clone();
        let c = ctx().with_model_resolver(Arc::new(move |_ref| Some(l.clone())));
        assert!(Arc::ptr_eq(
            &c.resolve_model("any-model").unwrap(),
            &labeled
        ));
    }

    #[test]
    fn is_none_without_a_resolver() {
        // No provider injected → None → the run uses the runtime's bound default.
        assert!(ctx().resolve_model("any-model").is_none());
    }

    #[test]
    fn is_none_when_the_resolver_declines_the_ref() {
        let c = ctx().with_model_resolver(Arc::new(|_ref| None));
        assert!(c.resolve_model("unknown-model").is_none());
    }
}
