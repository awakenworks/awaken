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
use awaken_run_ingress_contract::InferenceAccess;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::live_inbox::LiveInbox;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use tokio_util::sync::CancellationToken;

/// Materializes the activation's admission-pinned inference access. A worker
/// receives the complete activation and opaque access value; it neither selects a
/// model nor distinguishes deployment topology.
pub type InferenceMaterializerFn = Arc<
    dyn Fn(&RunActivation, Option<&InferenceAccess>) -> Option<Arc<dyn LlmExecutor>> + Send + Sync,
>;

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
    /// Runtime-only materialization. Absent preserves an explicitly composed host
    /// executor; once installed, rejecting pinned access fails the run closed.
    inference_materializer: Option<InferenceMaterializerFn>,
}

impl WorkerContext {
    /// Wire an attempt to its durable commit boundary.
    pub(crate) fn new(commit: Arc<dyn CommitCoordinator>) -> Self {
        Self {
            commit,
            reader: None,
            context: RuntimeRunContext::new(),
            inference_materializer: None,
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

    /// Install runtime-only inference materialization.
    #[must_use]
    pub(crate) fn with_inference_materializer(mut self, resolve: InferenceMaterializerFn) -> Self {
        self.inference_materializer = Some(resolve);
        self
    }

    /// Materialize the pinned access, or use the explicitly bound executor when no
    /// materializer was composed.
    pub(crate) fn materialize_inference(
        &self,
        activation: &RunActivation,
        model_access: Option<&InferenceAccess>,
    ) -> awaken_runtime_contract::execution::Result<Option<Arc<dyn LlmExecutor>>> {
        let Some(resolve) = &self.inference_materializer else {
            return Ok(None);
        };
        resolve(activation, model_access).map(Some).ok_or_else(|| {
            awaken_runtime_contract::execution::Error::Resolution(format!(
                "inference materializer rejected pinned model {}",
                activation.effective_model_ref()
            ))
        })
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
    //! resolves the ref or declines. Effect: `materialize_inference` returns the provider's
    //! executor, else `None` — and `None` leaves the run on the runtime's bound
    //! (host default) executor, so a single-model deployment is unaffected.
    use std::sync::Arc;

    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime::memory::MemoryCommitCoordinator;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
    use awaken_runtime_contract::{ExecutableAgentSnapshot, ModelBinding, RunActivation};

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

    fn activation(model_ref: &str) -> RunActivation {
        RunActivation::new(
            RunId("run".into()),
            ThreadId("thread".into()),
            ExecutableAgentSnapshot::builder("snapshot")
                .model(ModelBinding::new("provider", model_ref, "backend"))
                .build(),
            Vec::new(),
        )
    }

    #[test]
    fn returns_the_injected_providers_executor() {
        let labeled: Arc<dyn LlmExecutor> = Arc::new(Labeled("resolved"));
        let l = labeled.clone();
        let c = ctx().with_inference_materializer(Arc::new(move |_ref, _access| Some(l.clone())));
        assert!(Arc::ptr_eq(
            &c.materialize_inference(&activation("any-model"), None)
                .unwrap()
                .unwrap(),
            &labeled
        ));
    }

    #[test]
    fn is_none_without_a_resolver() {
        // No provider injected → None → the run uses the runtime's bound default.
        assert!(
            ctx()
                .materialize_inference(&activation("any-model"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn is_none_when_the_resolver_declines_the_ref() {
        let c = ctx().with_inference_materializer(Arc::new(|_ref, _access| None));
        assert!(
            c.materialize_inference(&activation("unknown-model"), None)
                .is_err()
        );
    }
}
