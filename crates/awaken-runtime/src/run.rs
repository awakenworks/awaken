//! Owned run activation boundary.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_runtime_contract::contract::commit_coordinator::CommitCoordinator;
use awaken_runtime_contract::contract::run::{
    RunInput, RunInputSnapshot, RunIntent, RunKind, RunOptions, RunTraceContext,
};
use awaken_runtime_contract::contract::storage::{RunRecord, RunRequestOrigin};
use awaken_runtime_contract::contract::suspension::ToolCallResume;
use awaken_runtime_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_runtime_contract::contract::{
    inference::InferenceOverride, message::Message, tool::ToolDescriptor,
};
use futures::channel::mpsc;

use crate::cancellation::CancellationToken;
use crate::inbox::{InboxReceiver, InboxSender};
use crate::loop_runner::PendingBoundaryHandler;
use crate::registry::RegistrySet;
use crate::resolution::Resolver;

/// Read-only snapshot of cached thread state, passed from mailbox to runtime.
#[non_exhaustive]
pub struct ThreadContextSnapshot {
    pub messages: Vec<Message>,
    pub latest_run: Option<RunRecord>,
    pub run_cache: HashMap<String, RunRecord>,
}

impl ThreadContextSnapshot {
    #[must_use]
    pub fn new(
        messages: Vec<Message>,
        latest_run: Option<RunRecord>,
        run_cache: HashMap<String, RunRecord>,
    ) -> Self {
        Self {
            messages,
            latest_run,
            run_cache,
        }
    }
}

/// In-process inbox pair owned by a single run.
pub struct RunInbox {
    pub sender: InboxSender,
    pub receiver: InboxReceiver,
}

/// Runtime control handles that cannot be persisted.
#[derive(Default)]
pub struct RunControl {
    pub cancellation_token: Option<CancellationToken>,
    pub decision_rx: Option<mpsc::UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
    pub inbox: Option<RunInbox>,
    pub pending_boundary: Option<Arc<dyn PendingBoundaryHandler>>,
    pub seeded_decisions: Vec<(String, ToolCallResume)>,
    /// Optional per-run commit coordinator override. The server dispatch path
    /// supplies a staging coordinator that folds canonical event drafts into
    /// each checkpoint commit, so the runtime never observes the staging
    /// buffer. When `None`, the runtime's build-time coordinator is used.
    pub commit_coordinator_override: Option<Arc<dyn CommitCoordinator>>,
}

/// Thread-context bundle threaded into runtime execution.
/// `thread_context_cache` is an optional caller-side fast path; absent means
/// the runtime loads thread context from the store as usual. Canonical event
/// staging is owned by the (server-supplied) commit coordinator, not here.
#[derive(Default)]
pub struct CaptureWiring {
    pub thread_context_cache: Option<Arc<ThreadContextSnapshot>>,
}

/// Submit-side facts the runtime must adopt to keep durable writes
/// idempotent and identity chains stable.
///
/// - `is_continuation`: the activation continues a prior run (resume /
///   handoff). The runtime uses this to skip re-persisting messages.
/// - `messages_already_persisted`: submit paths set this when they have
///   already appended new messages to the thread log.
/// - `run_id_hint` / `dispatch_id_hint`: mailbox-allocated identifiers
///   the runtime adopts instead of minting fresh ones, preserving the
///   dispatch ↔ run ↔ event chain.
#[derive(Default)]
pub struct PersistenceHints {
    pub is_continuation: bool,
    pub messages_already_persisted: bool,
    pub run_id_hint: Option<String>,
    pub dispatch_id_hint: Option<String>,
}

/// Frozen resolver objects inherited from a pinned root run. Sub-runs
/// spawned from a replayable parent use this to resolve against the same
/// registry the parent ran under, independent of the live registry
/// snapshot.
#[derive(Default)]
pub struct ResolverInheritance {
    pub pinned_registry_set: Option<RegistrySet>,
    pub run_resolver: Option<Arc<dyn Resolver>>,
}

/// Owned request to execute or resume a run.
pub struct RunActivation {
    pub intent: RunIntent,
    pub input: RunInput,
    pub options: RunOptions,
    pub trace: RunTraceContext,
    pub control: RunControl,
    /// Event capture and thread-context inputs the runtime threads into
    /// execution; orthogonal to user intent and trace metadata.
    pub capture: CaptureWiring,
    /// Submit-side persistence facts the runtime must honour for
    /// idempotency / id stability.
    pub persistence: PersistenceHints,
    /// Pinned resolver objects inherited from the parent for sub-run
    /// scope continuity.
    pub inherited: ResolverInheritance,
}

impl RunActivation {
    /// Build an activation with new message bodies.
    #[must_use]
    pub fn new(thread_id: impl Into<String>, messages: Vec<Message>) -> Self {
        let thread_id = thread_id.into();
        Self {
            intent: RunIntent::new(thread_id),
            input: RunInput::NewMessages(messages),
            options: RunOptions::default(),
            trace: RunTraceContext::default(),
            control: RunControl::default(),
            capture: CaptureWiring::default(),
            persistence: PersistenceHints::default(),
            inherited: ResolverInheritance::default(),
        }
    }

    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.intent.thread_id
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        match &self.input {
            RunInput::NewMessages(messages) => messages,
            RunInput::AlreadyPersisted(_) => &[],
        }
    }

    #[must_use]
    pub fn messages_already_persisted(&self) -> bool {
        self.persistence.messages_already_persisted
            || matches!(self.input, RunInput::AlreadyPersisted(_))
    }

    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        self.intent.agent_id.as_deref()
    }

    #[must_use]
    pub fn run_id_hint(&self) -> Option<&str> {
        self.persistence.run_id_hint.as_deref()
    }

    #[must_use]
    pub fn dispatch_id_hint(&self) -> Option<&str> {
        self.persistence.dispatch_id_hint.as_deref()
    }

    #[must_use]
    pub fn resume_run_id(&self) -> Option<&str> {
        match &self.intent.kind {
            RunKind::HitlResume { run_id } | RunKind::ContinuationFromRun { run_id } => {
                Some(run_id)
            }
            RunKind::NewIntent => None,
        }
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.intent.agent_id = Some(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: InferenceOverride) -> Self {
        self.options.overrides = Some(overrides);
        self
    }

    #[must_use]
    pub fn with_decisions(mut self, decisions: Vec<(String, ToolCallResume)>) -> Self {
        self.control.seeded_decisions = decisions;
        self
    }

    #[must_use]
    pub fn with_frontend_tools(mut self, tools: Vec<ToolDescriptor>) -> Self {
        self.options.frontend_tools = tools;
        self
    }

    #[must_use]
    pub fn with_legacy_origin(mut self, origin: RunRequestOrigin) -> Self {
        self.trace.origin = origin.into();
        self
    }

    #[must_use]
    pub fn with_origin(self, origin: RunRequestOrigin) -> Self {
        self.with_legacy_origin(origin)
    }

    #[must_use]
    pub fn with_run_mode(mut self, run_mode: RunMode) -> Self {
        self.trace.run_mode = run_mode;
        self
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter: AdapterKind) -> Self {
        self.trace.adapter = adapter;
        self
    }

    #[must_use]
    pub fn with_parent_run_id(mut self, parent_run_id: impl Into<String>) -> Self {
        self.trace.parent_run_id = Some(parent_run_id.into());
        self
    }

    #[must_use]
    pub fn with_parent_thread_id(mut self, parent_thread_id: impl Into<String>) -> Self {
        self.trace.parent_thread_id = Some(parent_thread_id.into());
        self
    }

    #[must_use]
    pub fn with_hitl_resume_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.intent.kind = RunKind::HitlResume {
            run_id: run_id.into(),
        };
        self.trace.run_mode = RunMode::Resume;
        self
    }

    #[must_use]
    pub fn with_continue_run_id(self, run_id: impl Into<String>) -> Self {
        self.with_hitl_resume_run_id(run_id)
    }

    #[must_use]
    pub fn with_continuation_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.intent.kind = RunKind::ContinuationFromRun {
            run_id: run_id.into(),
        };
        self.persistence.is_continuation = true;
        self
    }

    #[must_use]
    pub fn with_dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.trace.dispatch_id = Some(dispatch_id.into());
        self
    }

    #[must_use]
    pub fn with_trace_dispatch_id(self, dispatch_id: impl Into<String>) -> Self {
        self.with_dispatch_id(dispatch_id)
    }

    #[must_use]
    pub fn with_run_id_hint(mut self, run_id_hint: impl Into<String>) -> Self {
        self.persistence.run_id_hint = Some(run_id_hint.into());
        self
    }

    #[must_use]
    pub fn with_dispatch_id_hint(mut self, dispatch_id_hint: impl Into<String>) -> Self {
        self.persistence.dispatch_id_hint = Some(dispatch_id_hint.into());
        self
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.trace.session_id = Some(session_id.into());
        self
    }

    #[must_use]
    pub fn with_transport_request_id(mut self, id: impl Into<String>) -> Self {
        self.trace.transport_request_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_inbox(mut self, sender: InboxSender, receiver: InboxReceiver) -> Self {
        self.control.inbox = Some(RunInbox { sender, receiver });
        self
    }

    #[must_use]
    pub fn with_pending_boundary_handler(
        mut self,
        handler: Arc<dyn PendingBoundaryHandler>,
    ) -> Self {
        self.control.pending_boundary = Some(handler);
        self
    }

    #[must_use]
    pub fn with_already_persisted_input(mut self, input: RunInputSnapshot) -> Self {
        self.input = RunInput::AlreadyPersisted(input);
        self.persistence.messages_already_persisted = true;
        self
    }

    #[must_use]
    pub fn with_messages_already_persisted(mut self, value: bool) -> Self {
        self.persistence.messages_already_persisted = value;
        self
    }

    #[must_use]
    pub fn with_pinned_registry_set(mut self, registry_set: RegistrySet) -> Self {
        self.inherited.pinned_registry_set = Some(registry_set);
        self
    }

    #[must_use]
    pub fn with_run_resolver(mut self, resolver: Arc<dyn Resolver>) -> Self {
        self.inherited.run_resolver = Some(resolver);
        self
    }

    /// Attach a per-run commit coordinator override (server staging
    /// coordinator). Supersedes the runtime's build-time coordinator for this
    /// run only, so canonical drafts staged by the dispatch sink commit
    /// atomically with the checkpoint without the runtime observing them.
    #[must_use]
    pub fn with_commit_coordinator_override(
        mut self,
        coordinator: Arc<dyn CommitCoordinator>,
    ) -> Self {
        self.control.commit_coordinator_override = Some(coordinator);
        self
    }

    #[must_use]
    pub fn with_thread_context_cache(mut self, cache: Arc<ThreadContextSnapshot>) -> Self {
        self.capture.thread_context_cache = Some(cache);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunActivationError {
    #[error("run activation is missing thread_id")]
    MissingThreadId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolution::{ResolutionRequest, ResolveError, ResolvedRunPlan};

    fn assert_send_static<T: Send + 'static>(_: T) {}

    struct NoopResolver;

    #[async_trait::async_trait]
    impl Resolver for NoopResolver {
        async fn resolve(
            &self,
            _request: ResolutionRequest,
        ) -> Result<ResolvedRunPlan, ResolveError> {
            Err(ResolveError::Runtime("noop".into()))
        }
    }

    struct StubCoordinator;

    #[async_trait::async_trait]
    impl CommitCoordinator for StubCoordinator {
        fn scope(
            &self,
        ) -> awaken_runtime_contract::contract::commit_coordinator::TransactionScopeId {
            awaken_runtime_contract::contract::commit_coordinator::TransactionScopeId::new("test")
                .unwrap()
        }
        fn thread_run_store(
            &self,
        ) -> Arc<dyn awaken_runtime_contract::contract::storage::ThreadRunStore> {
            unreachable!("routing test does not commit")
        }
        async fn commit_checkpoint(
            &self,
            _plan: awaken_runtime_contract::contract::commit_coordinator::CheckpointCommitPlan,
        ) -> Result<
            awaken_runtime_contract::contract::commit_coordinator::CheckpointCommitOutcome,
            awaken_runtime_contract::contract::commit_coordinator::CommitError,
        > {
            unreachable!("routing test does not commit")
        }
    }

    #[test]
    fn activation_is_owned_send_static() {
        let activation = RunActivation::new("thread", vec![Message::user("hi")]);
        assert_send_static(activation);
    }

    /// Pins the routing between builder methods and the three split
    /// `CaptureWiring` / `PersistenceHints` / `ResolverInheritance`
    /// sub-structs introduced when `RunExecutionWiring` was decomposed.
    /// Any future renaming that accidentally drops a setter into the wrong
    /// bucket will trip these field-by-field assertions.
    #[test]
    fn builder_methods_route_to_correct_wiring_sub_struct() {
        use crate::registry::RegistrySet;
        use crate::registry::memory::{
            MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
            MapToolRegistry,
        };

        let coordinator: Arc<dyn CommitCoordinator> = Arc::new(StubCoordinator);
        let cache = Arc::new(ThreadContextSnapshot::new(
            Vec::new(),
            None,
            Default::default(),
        ));
        let registry_set = RegistrySet {
            agents: Arc::new(MapAgentSpecRegistry::new()),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(MapModelRegistry::new()),
            providers: Arc::new(MapProviderRegistry::new()),
            plugins: Arc::new(MapPluginSource::new()),
            #[cfg(feature = "a2a")]
            backends: Arc::new(crate::registry::memory::MapBackendRegistry::new()),
        };

        let activation = RunActivation::new("thread", vec![Message::user("hi")])
            .with_commit_coordinator_override(Arc::clone(&coordinator))
            .with_thread_context_cache(Arc::clone(&cache))
            .with_run_id_hint("hinted-run-id")
            .with_dispatch_id_hint("hinted-dispatch-id")
            .with_continuation_run_id("parent-run")
            .with_messages_already_persisted(true)
            .with_pinned_registry_set(registry_set)
            .with_run_resolver(Arc::new(NoopResolver));

        // Control sub-struct: per-run commit coordinator override.
        assert!(
            activation.control.commit_coordinator_override.is_some(),
            "with_commit_coordinator_override routes to RunControl"
        );
        // Capture sub-struct: thread-context fast path.
        assert!(
            activation.capture.thread_context_cache.is_some(),
            "with_thread_context_cache routes to CaptureWiring"
        );
        // The other two sub-structs must not contain capture-shaped fields.

        // Persistence sub-struct: submit-side idempotency + id injection.
        assert_eq!(
            activation.persistence.run_id_hint.as_deref(),
            Some("hinted-run-id"),
            "with_run_id_hint routes to PersistenceHints"
        );
        assert_eq!(
            activation.persistence.dispatch_id_hint.as_deref(),
            Some("hinted-dispatch-id"),
            "with_dispatch_id_hint routes to PersistenceHints"
        );
        assert!(
            activation.persistence.is_continuation,
            "with_continuation_run_id sets PersistenceHints::is_continuation"
        );
        assert!(
            activation.persistence.messages_already_persisted,
            "with_messages_already_persisted routes to PersistenceHints"
        );

        // Resolver inheritance sub-struct: sub-run scope pinning.
        assert!(
            activation.inherited.pinned_registry_set.is_some(),
            "with_pinned_registry_set routes to ResolverInheritance"
        );
        assert!(
            activation.inherited.run_resolver.is_some(),
            "with_run_resolver routes to ResolverInheritance"
        );

        // Reverse spot-check: capture must not be mutated by submit-side or
        // inheritance setters, and vice versa.
        let neutral = RunActivation::new("t", Vec::new()).with_run_id_hint("x");
        assert!(neutral.control.commit_coordinator_override.is_none());
        assert!(neutral.inherited.pinned_registry_set.is_none());
        assert!(neutral.inherited.run_resolver.is_none());
        assert_eq!(neutral.persistence.run_id_hint.as_deref(), Some("x"));
    }
}
