//! Private live dependencies used by a durable dispatch worker.
//!
//! This is infrastructure wiring, not a Run-domain object. Durable Run data is
//! represented by `RunDispatch`; this module only rebuilds `RuntimeRunContext`
//! values from worker-owned services.

use std::sync::Arc;

use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::live_inbox::LiveInbox;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use tokio_util::sync::CancellationToken;

use crate::RunClaim;
use crate::claimed_stream::{
    ClaimBoundStreamSink, ClaimedStreamPublisher, LocalClaimedStreamPublisher,
};

/// Materializes the activation's admission-pinned inference access. A worker
/// receives the complete activation and opaque access value; it neither selects a
/// model nor distinguishes deployment topology.
pub type InferenceMaterializerFn = Arc<
    dyn Fn(&RunActivation, &RuntimeRunContext) -> Result<Option<Arc<dyn LlmExecutor>>, String>
        + Send
        + Sync,
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
    reader: Option<Arc<dyn CommittedThreadView>>,
    /// Attempt-local capabilities inherited from ingress. Durable execution
    /// replaces only commit/read authority, cancellation, and pause below; tool
    /// placement, capture, observability, and retry accounting stay identical to
    /// a directly admitted Run.
    context: RuntimeRunContext,
    stream_publisher: Option<Arc<dyn ClaimedStreamPublisher>>,
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
            stream_publisher: None,
            inference_materializer: None,
        }
    }

    /// Inherit the ordinary attempt capabilities supplied by an upper Run or
    /// protocol. Run-bound authorities are deliberately retained from this
    /// Worker (checkpoint/live inbox) or reinstalled when an attempt is built
    /// (commit/read/ownership), so a delegated child cannot nest its own claim
    /// around the parent's already-fenced handles or steer through its inbox.
    #[must_use]
    pub(crate) fn with_context(mut self, mut context: RuntimeRunContext) -> Self {
        let stream_checkpoint = self.context.stream_checkpoint.take();
        let live_inbox = self.context.live_inbox.take();
        context.commit = None;
        context.reader = None;
        context.stream_checkpoint = stream_checkpoint;
        context.live_inbox = live_inbox;
        context.ownership = None;
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
        context: &RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<Option<Arc<dyn LlmExecutor>>> {
        let Some(resolve) = &self.inference_materializer else {
            return Ok(None);
        };
        resolve(activation, context).map_err(|error| {
            awaken_runtime_contract::execution::Error::Resolution(format!(
                "inference materializer rejected pinned model {}: {error}",
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
    pub(crate) fn with_reader(mut self, reader: Arc<dyn CommittedThreadView>) -> Self {
        self.reader = Some(reader);
        self
    }

    /// Attach a best-effort live stream sink (live progress is never truth).
    #[must_use]
    pub(crate) fn with_stream_sink(mut self, sink: Arc<dyn StreamSink>) -> Self {
        self.stream_publisher = Some(Arc::new(LocalClaimedStreamPublisher::new(sink)));
        self
    }

    /// Attach the durable worker's claim-aware live publisher. Remote workers use
    /// this boundary to authenticate and fence each event at the Coordinator.
    #[must_use]
    pub(crate) fn with_claimed_stream_publisher(
        mut self,
        publisher: Arc<dyn ClaimedStreamPublisher>,
    ) -> Self {
        self.stream_publisher = Some(publisher);
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
    pub(crate) fn runtime_context(
        &self,
        cancel: CancellationToken,
        claim: Option<&RunClaim>,
    ) -> RuntimeRunContext {
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
        if let (Some(publisher), Some(claim)) = (&self.stream_publisher, claim) {
            context = context.with_stream_sink(Arc::new(ClaimBoundStreamSink::new(
                claim.clone(),
                publisher.clone(),
            )));
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

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use awaken_runtime_contract::live_inbox::{LiveInbox, Offer};
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
    use awaken_runtime_contract::runtime_context::{
        AttemptOwnershipError, AttemptOwnershipVerifier,
    };
    use awaken_runtime_contract::{
        ExecutableAgentSnapshot, ModelBinding, RunActivation, RuntimeRunContext,
    };
    use awaken_store_inmem::MemoryCommitCoordinator;

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

    struct AllowOwnership;

    #[async_trait::async_trait]
    impl AttemptOwnershipVerifier for AllowOwnership {
        async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
            Ok(())
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
        let c = ctx().with_inference_materializer(Arc::new(move |_activation, _context| {
            Ok(Some(l.clone()))
        }));
        assert!(Arc::ptr_eq(
            &c.materialize_inference(&activation("any-model"), &RuntimeRunContext::new())
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
                .materialize_inference(&activation("any-model"), &RuntimeRunContext::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn is_none_when_the_resolver_declines_the_ref() {
        let c = ctx()
            .with_inference_materializer(Arc::new(|_activation, _context| Err("unknown".into())));
        assert!(
            c.materialize_inference(&activation("unknown-model"), &RuntimeRunContext::new())
                .is_err()
        );
    }

    #[test]
    fn inherited_context_cannot_replace_worker_run_authorities() {
        // Cause/effect graph: C1 the Worker has/has-not a paired checkpoint A;
        // C2 an upper context either has no Run authority or carries parent
        // checkpoint/commit/read/ownership/live-inbox B. Effects: E1 the built
        // attempt keeps checkpoint A when present and otherwise has no inner
        // checkpoint; E2 commit/read are reinstalled from Worker A; E3 no parent
        // ownership handle crosses into the child attempt; E4 the Worker-owned
        // live inbox survives upper-context composition, while a parent inbox
        // never crosses into a Worker that owns none. Constraint:
        // `DispatchWorker` later installs the child's outer claim-fenced
        // checkpoint and ownership adapters; its one inbox is the durable
        // ingress offer/drain identity.
        //
        // | Rule | Worker authority | inherited authority | built attempt |
        // |---|---|---|---|
        // | R1 | A | absent | A checkpoint/inbox (ordinary root) |
        // | R2 | A | B | A checkpoint/inbox only (configured root) |
        // | R3 | absent | B | no inner checkpoint/inbox (delegated child) |
        // Decision rule: R1-R3 exhaust worker authority present/absent and
        // inherited authority present/absent without allowing parent replacement.
        let worker_commit = Arc::new(MemoryCommitCoordinator::new());
        let worker_commit_port: Arc<dyn CommitCoordinator> = worker_commit.clone();
        let worker_reader_port: Arc<dyn CommittedThreadView> = worker_commit.clone();
        let worker_checkpoint: Arc<dyn StreamCheckpointStore> =
            Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new());
        let worker_inbox = LiveInbox::new();
        let parent_commit = Arc::new(MemoryCommitCoordinator::new());
        let parent_checkpoint: Arc<dyn StreamCheckpointStore> =
            Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new());
        let parent_inbox = LiveInbox::new();
        let parent_context = || {
            RuntimeRunContext::new()
                .with_commit(parent_commit.clone())
                .with_reader(parent_commit.clone())
                .with_stream_checkpoint(parent_checkpoint.clone())
                .with_live_inbox(parent_inbox.clone())
                .with_ownership(Arc::new(AllowOwnership))
        };

        let worker = WorkerContext::new(worker_commit_port.clone())
            .with_reader(worker_reader_port.clone())
            .with_stream_checkpoint(worker_checkpoint.clone())
            .with_live_inbox(worker_inbox.clone());

        for (rule, inherited) in [("R1", RuntimeRunContext::new()), ("R2", parent_context())] {
            let attempt = worker
                .clone()
                .with_context(inherited)
                .runtime_context(tokio_util::sync::CancellationToken::new(), None);

            assert!(
                Arc::ptr_eq(
                    attempt.commit.as_ref().expect("E2 commit"),
                    &worker_commit_port,
                ),
                "{rule}/E2"
            );
            assert!(
                Arc::ptr_eq(
                    attempt.reader.as_ref().expect("E2 reader"),
                    &worker_reader_port,
                ),
                "{rule}/E2"
            );
            assert!(
                Arc::ptr_eq(
                    attempt.stream_checkpoint.as_ref().expect("E1 checkpoint"),
                    &worker_checkpoint,
                ),
                "{rule}/E1"
            );
            assert!(attempt.ownership.is_none(), "{rule}/E3");
            assert!(matches!(
                attempt
                    .live_inbox
                    .expect("E4 Worker inbox")
                    .offer(Message::text(
                        MessageId(format!("{rule}-message")),
                        Role::User,
                        rule,
                    )),
                Offer::Accepted(_)
            ));
            assert!(parent_inbox.list().is_empty(), "{rule}/E4");
        }
        assert_eq!(worker_inbox.list().len(), 2, "R1+R2/E4");

        let child_attempt = WorkerContext::new(worker_commit_port.clone())
            .with_reader(worker_reader_port.clone())
            .with_context(parent_context())
            .runtime_context(tokio_util::sync::CancellationToken::new(), None);
        assert!(child_attempt.stream_checkpoint.is_none(), "R3/E1");
        assert!(
            Arc::ptr_eq(
                child_attempt.commit.as_ref().expect("R3/E2 commit"),
                &worker_commit_port,
            ),
            "R3/E2"
        );
        assert!(
            Arc::ptr_eq(
                child_attempt.reader.as_ref().expect("R3/E2 reader"),
                &worker_reader_port,
            ),
            "R3/E2"
        );
        assert!(child_attempt.ownership.is_none(), "R3/E3");
        assert!(child_attempt.live_inbox.is_none(), "R3/E4");
        assert!(parent_inbox.list().is_empty(), "R3/E4");
    }
}
