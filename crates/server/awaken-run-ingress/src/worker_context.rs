//! Private live dependencies used by a durable dispatch worker.
//!
//! This is infrastructure wiring, not a Run-domain object. Durable Run data is
//! represented by `RunDispatch`; this module only rebuilds `RuntimeRunContext`
//! values from worker-owned services.

use std::sync::Arc;

use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_ingress_contract::ModelAccessRef;
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
pub type ModelResolverFn =
    Arc<dyn Fn(&str, Option<&ModelAccessRef>) -> Option<Arc<dyn LlmExecutor>> + Send + Sync>;

// The serializable durable-run instruction moved to the dispatch contract
// (ADR-0039 2.1); re-exported so `awaken_run_ingress_contract::RunDispatch` is stable.

/// Worker-owned live wiring. The durable host holds one of these and rebuilds a
/// [`RuntimeRunContext`] for every execute/resume attempt, binding the run to the
/// durable commit boundary (the single write authority, G1/G13) and the optional
/// best-effort stream sink. Cancellation is created per attempt by the worker.
#[derive(Clone)]
pub(crate) struct WorkerContext {
    commit: Arc<dyn CommitCoordinator>,
    reader: Option<Arc<dyn ThreadReader>>,
    /// Attempt-local capabilities inherited from ingress. Durable execution
    /// replaces only commit/read authority, cancellation, and pause below; tool
    /// placement, capture, observability, and retry accounting stay identical to
    /// a directly admitted Run.
    context: RuntimeRunContext,
    /// Resolves a run's effective model ref to its executor (R1), so a database-less
    /// worker builds the run's configured model per attempt. Absent means the run
    /// falls back to the runtime's bound (host default) executor — the pre-provider
    /// single-model behaviour.
    model_resolver: Option<ModelResolverFn>,
}

impl WorkerContext {
    /// Wire an attempt to its durable commit boundary.
    pub(crate) fn new(commit: Arc<dyn CommitCoordinator>) -> Self {
        Self {
            commit,
            reader: None,
            context: RuntimeRunContext::new(),
            model_resolver: None,
        }
    }

    /// Inherit the ordinary attempt capabilities supplied by an upper Run or
    /// protocol. Commit/read authority is deliberately reinstalled when an
    /// attempt is built, so callers cannot bypass the claimed-Run fence.
    #[must_use]
    pub(crate) fn with_context(mut self, context: RuntimeRunContext) -> Self {
        self.context = context;
        self
    }

    /// Install the model→executor resolver (R1): a worker-driven run resolves its
    /// effective model ref (override, else its snapshot binding) to an executor
    /// through this, so a database-less worker runs the configured model without a
    /// config service. The worker names a model and gets an executor — it never
    /// learns *how* the model is reached.
    #[must_use]
    pub(crate) fn with_model_resolver(mut self, resolve: ModelResolverFn) -> Self {
        self.model_resolver = Some(resolve);
        self
    }

    /// Resolve `model_ref` to its executor via the injected provider, or `None` to
    /// fall back to the runtime's bound default (no provider / unresolved ref).
    pub(crate) fn resolve_model(
        &self,
        model_ref: &str,
        model_access: Option<&ModelAccessRef>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.model_resolver
            .as_ref()
            .and_then(|resolve| resolve(model_ref, model_access))
    }

    /// Provide the per-session live inbox so worker-driven runs drain mid-run
    /// steer at their boundaries (ADR-0054 P2). The same neutral inbox the offer
    /// side reaches, so steer/redirect works on the durable path.
    #[must_use]
    pub(crate) fn with_live_inbox(mut self, inbox: LiveInbox) -> Self {
        self.context = self.context.with_live_inbox(inbox);
        self
    }

    /// Provide the committed-history read port so a fresh run continues the
    /// thread's conversation. Usually the same store as the commit.
    #[must_use]
    pub(crate) fn with_reader(mut self, reader: Arc<dyn ThreadReader>) -> Self {
        self.reader = Some(reader);
        self
    }

    /// Attach a best-effort live stream sink (live progress is never truth).
    #[must_use]
    pub(crate) fn with_stream_sink(mut self, sink: Arc<dyn StreamSink>) -> Self {
        self.context = self.context.with_stream_sink(sink);
        self
    }

    /// Attach the durable interrupted-stream checkpoint store (Phase 3), so a
    /// dispatch re-executed after a crash resumes its in-flight step from the
    /// flushed partial instead of re-running it.
    #[must_use]
    pub(crate) fn with_stream_checkpoint(mut self, store: Arc<dyn StreamCheckpointStore>) -> Self {
        self.context = self.context.with_stream_checkpoint(store);
        self
    }

    /// Build the runtime-facing context for one attempt, carrying the supplied
    /// cancellation token so the host can steer an in-flight run.
    pub(crate) fn runtime_context(&self, cancel: CancellationToken) -> RuntimeRunContext {
        let mut context = self
            .context
            .clone()
            .with_commit(self.commit.clone())
            .with_cancellation(cancel)
            // A fresh pause signal per attempt, registered by the executor so live
            // control can await this run at its next safe boundary (ADR-0054).
            .with_pause(PauseSignal::new());
        if let Some(reader) = &self.reader {
            context = context.with_reader(reader.clone());
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

    use super::WorkerContext;

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

    fn ctx() -> WorkerContext {
        WorkerContext::new(Arc::new(MemoryCommitCoordinator::new()))
    }

    #[test]
    fn returns_the_injected_providers_executor() {
        let labeled: Arc<dyn LlmExecutor> = Arc::new(Labeled("resolved"));
        let l = labeled.clone();
        let c = ctx().with_model_resolver(Arc::new(move |_ref, _access| Some(l.clone())));
        assert!(Arc::ptr_eq(
            &c.resolve_model("any-model", None).unwrap(),
            &labeled
        ));
    }

    #[test]
    fn is_none_without_a_resolver() {
        // No provider injected → None → the run uses the runtime's bound default.
        assert!(ctx().resolve_model("any-model", None).is_none());
    }

    #[test]
    fn is_none_when_the_resolver_declines_the_ref() {
        let c = ctx().with_model_resolver(Arc::new(|_ref, _access| None));
        assert!(c.resolve_model("unknown-model", None).is_none());
    }
}
