//! Owned run activation boundary.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::run::{
    RunActivationSnapshot, RunInput, RunInputSnapshot, RunIntent, RunKind, RunOptions,
    RunResolutionScope, RunTraceContext,
};
use awaken_contract::contract::storage::{PinnedRegistryManifest, RunRecord, RunRequestOrigin};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};
use awaken_contract::contract::{
    inference::InferenceOverride, message::Message, tool::ToolDescriptor,
};
use futures::channel::mpsc;

use crate::EventBuffer;
use crate::cancellation::CancellationToken;
use crate::inbox::{InboxReceiver, InboxSender};
use crate::registry::RegistrySet;

/// Read-only snapshot of cached thread state, passed from mailbox to runtime.
#[non_exhaustive]
pub struct ThreadContextSnapshot {
    pub messages: Vec<Message>,
    pub latest_run: Option<RunRecord>,
    pub run_cache: HashMap<String, RunRecord>,
    /// Registry manifest seed for newly-created RunRecords inside the runtime.
    pub registry_manifest_seed: Option<PinnedRegistryManifest>,
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
            registry_manifest_seed: None,
        }
    }

    /// Attach a registry manifest seed inherited from the activation.
    #[must_use]
    pub fn with_registry_manifest_seed(mut self, seed: PinnedRegistryManifest) -> Self {
        self.registry_manifest_seed = Some(seed);
        self
    }
}

/// Forward a pinned registry manifest into the runtime's thread context so it
/// can seed the first RunRecord built by checkpoint persistence.
pub fn attach_registry_manifest_seed(
    thread_ctx: &mut Option<ThreadContextSnapshot>,
    manifest: Option<PinnedRegistryManifest>,
) {
    if let Some(seed) = manifest {
        thread_ctx
            .get_or_insert_with(|| ThreadContextSnapshot::new(Vec::new(), None, HashMap::new()))
            .registry_manifest_seed
            .get_or_insert(seed);
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
    pub seeded_decisions: Vec<(String, ToolCallResume)>,
}

/// Event-capture and thread-context bundle threaded into runtime
/// execution. `event_buffer == None` disables canonical capture for this
/// activation; persistent runs always carry `Some`. `thread_context_cache`
/// is an optional caller-side fast path; absent means the runtime loads
/// thread context from the store as usual.
#[derive(Default)]
pub struct CaptureWiring {
    pub event_buffer: Option<Arc<EventBuffer>>,
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
}

/// Legacy request shape kept for server code that is migrated in a later split PR.
pub struct RunRequest {
    pub messages: Vec<Message>,
    pub messages_already_persisted: bool,
    pub thread_id: String,
    pub agent_id: Option<String>,
    pub overrides: Option<InferenceOverride>,
    pub decisions: Vec<(String, ToolCallResume)>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub origin: RunRequestOrigin,
    pub run_mode: RunMode,
    pub adapter: AdapterKind,
    pub parent_run_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub continue_run_id: Option<String>,
    pub run_id_hint: Option<String>,
    pub dispatch_id_hint: Option<String>,
    pub dispatch_id: Option<String>,
    pub session_id: Option<String>,
    pub transport_request_id: Option<String>,
    pub run_inbox: Option<RunInbox>,
}

impl RunRequest {
    #[must_use]
    pub fn new(thread_id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            messages,
            messages_already_persisted: false,
            thread_id: thread_id.into(),
            agent_id: None,
            overrides: None,
            decisions: Vec::new(),
            frontend_tools: Vec::new(),
            origin: RunRequestOrigin::User,
            run_mode: RunMode::Foreground,
            adapter: AdapterKind::Internal,
            parent_run_id: None,
            parent_thread_id: None,
            continue_run_id: None,
            run_id_hint: None,
            dispatch_id_hint: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            run_inbox: None,
        }
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: InferenceOverride) -> Self {
        self.overrides = Some(overrides);
        self
    }

    #[must_use]
    pub fn with_decisions(mut self, decisions: Vec<(String, ToolCallResume)>) -> Self {
        self.decisions = decisions;
        self
    }

    #[must_use]
    pub fn with_frontend_tools(mut self, tools: Vec<ToolDescriptor>) -> Self {
        self.frontend_tools = tools;
        self
    }

    #[must_use]
    pub fn with_origin(mut self, origin: RunRequestOrigin) -> Self {
        self.origin = origin;
        self
    }

    #[must_use]
    pub fn with_run_mode(mut self, run_mode: RunMode) -> Self {
        self.run_mode = run_mode;
        self
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter: AdapterKind) -> Self {
        self.adapter = adapter;
        self
    }

    #[must_use]
    pub fn with_parent_run_id(mut self, parent_run_id: impl Into<String>) -> Self {
        self.parent_run_id = Some(parent_run_id.into());
        self
    }

    #[must_use]
    pub fn with_parent_thread_id(mut self, parent_thread_id: impl Into<String>) -> Self {
        self.parent_thread_id = Some(parent_thread_id.into());
        self
    }

    #[must_use]
    pub fn with_continue_run_id(mut self, continue_run_id: impl Into<String>) -> Self {
        self.continue_run_id = Some(continue_run_id.into());
        self
    }

    #[must_use]
    pub fn with_run_id_hint(mut self, run_id_hint: impl Into<String>) -> Self {
        self.run_id_hint = Some(run_id_hint.into());
        self
    }

    #[must_use]
    pub fn with_dispatch_id_hint(mut self, dispatch_id_hint: impl Into<String>) -> Self {
        self.dispatch_id_hint = Some(dispatch_id_hint.into());
        self
    }

    #[must_use]
    pub fn with_trace_dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = Some(dispatch_id.into());
        self
    }

    #[must_use]
    pub fn with_dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = Some(dispatch_id.into());
        self
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    #[must_use]
    pub fn with_transport_request_id(mut self, id: impl Into<String>) -> Self {
        self.transport_request_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_inbox(mut self, sender: InboxSender, receiver: InboxReceiver) -> Self {
        self.run_inbox = Some(RunInbox { sender, receiver });
        self
    }

    #[must_use]
    pub fn with_messages_already_persisted(mut self, value: bool) -> Self {
        self.messages_already_persisted = value;
        self
    }
}

impl From<RunRequest> for RunActivation {
    fn from(request: RunRequest) -> Self {
        let mut activation = RunActivation::new(request.thread_id, request.messages)
            .with_messages_already_persisted(request.messages_already_persisted)
            .with_legacy_origin(request.origin)
            .with_run_mode(request.run_mode)
            .with_adapter(request.adapter);
        activation.intent.agent_id = request.agent_id;
        activation.options.overrides = request.overrides;
        activation.options.frontend_tools = request.frontend_tools;
        activation.control.seeded_decisions = request.decisions;
        activation.trace.parent_run_id = request.parent_run_id;
        activation.trace.parent_thread_id = request.parent_thread_id;
        activation.trace.dispatch_id = request.dispatch_id.or(request.dispatch_id_hint.clone());
        activation.trace.session_id = request.session_id;
        activation.trace.transport_request_id = request.transport_request_id;
        activation.persistence.run_id_hint = request.run_id_hint;
        activation.persistence.dispatch_id_hint = request.dispatch_id_hint;
        if let Some(continue_run_id) = request.continue_run_id {
            activation.intent.kind = RunKind::HitlResume {
                run_id: continue_run_id,
            };
            activation.trace.run_mode = RunMode::Resume;
        }
        activation.control.inbox = request.run_inbox;
        activation
    }
}

/// Owned request to execute or resume a run.
pub struct RunActivation {
    pub intent: RunIntent,
    pub input: RunInput,
    pub options: RunOptions,
    pub trace: RunTraceContext,
    pub resolution_scope: RunResolutionScope,
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
            resolution_scope: RunResolutionScope::Live,
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
    pub fn with_pinned_resolution(mut self, manifest: PinnedRegistryManifest) -> Self {
        self.resolution_scope = RunResolutionScope::Pinned(manifest);
        self
    }

    #[must_use]
    pub fn with_registry_manifest(self, manifest: PinnedRegistryManifest) -> Self {
        self.with_pinned_resolution(manifest)
    }

    #[must_use]
    pub fn with_pinned_registry_set(mut self, registry_set: RegistrySet) -> Self {
        self.inherited.pinned_registry_set = Some(registry_set);
        self
    }

    #[must_use]
    pub fn with_event_buffer(mut self, buffer: Arc<EventBuffer>) -> Self {
        self.capture.event_buffer = Some(buffer);
        self
    }

    #[must_use]
    pub fn with_thread_context_cache(mut self, cache: Arc<ThreadContextSnapshot>) -> Self {
        self.capture.thread_context_cache = Some(cache);
        self
    }

    /// Build the replay-safe durable snapshot after message persistence and
    /// registry pinning have already completed.
    #[must_use]
    pub fn snapshot(
        &self,
        persisted_input: RunInputSnapshot,
        pinned_manifest: PinnedRegistryManifest,
    ) -> RunActivationSnapshot {
        RunActivationSnapshot {
            intent: self.intent.clone(),
            input: persisted_input,
            options: self.options.clone(),
            trace: self.trace.clone(),
            seeded_decisions: self.control.seeded_decisions.clone(),
            resolution_scope: pinned_manifest,
        }
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
    use awaken_contract::contract::storage::PinnedRegistryEntry;

    fn manifest(id: &str) -> PinnedRegistryManifest {
        PinnedRegistryManifest {
            publication_id: Some("pub-1".into()),
            registry_snapshot_version: Some(1),
            entries: vec![PinnedRegistryEntry {
                kind: "agent".into(),
                id: id.into(),
                version: 1,
                content_hash:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            }],
        }
    }

    fn assert_send_static<T: Send + 'static>(_: T) {}

    #[test]
    fn activation_is_owned_send_static() {
        let activation = RunActivation::new("thread", vec![Message::user("hi")]);
        assert_send_static(activation);
    }

    #[test]
    fn snapshot_uses_caller_supplied_persisted_input_and_manifest() {
        let activation = RunActivation::new("thread", vec![Message::user("hi")])
            .with_agent_id("agent-a")
            .with_dispatch_id("dispatch-1");
        let input = RunInputSnapshot {
            thread_id: "thread".into(),
            trigger_message_ids: vec!["msg-1".into()],
            ..Default::default()
        };
        let snapshot = activation.snapshot(input.clone(), manifest("agent-a"));
        assert_eq!(snapshot.intent.agent_id.as_deref(), Some("agent-a"));
        assert_eq!(snapshot.input, input);
        assert_eq!(snapshot.trace.dispatch_id.as_deref(), Some("dispatch-1"));
        assert_eq!(snapshot.resolution_scope.entries[0].id, "agent-a");
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

        let buffer = Arc::new(EventBuffer::new());
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
            .with_event_buffer(Arc::clone(&buffer))
            .with_thread_context_cache(Arc::clone(&cache))
            .with_run_id_hint("hinted-run-id")
            .with_dispatch_id_hint("hinted-dispatch-id")
            .with_continuation_run_id("parent-run")
            .with_messages_already_persisted(true)
            .with_pinned_registry_set(registry_set);

        // Capture sub-struct: runtime-side event capture + context fast path.
        assert!(
            activation.capture.event_buffer.is_some(),
            "with_event_buffer routes to CaptureWiring"
        );
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

        // Reverse spot-check: capture must not be mutated by submit-side or
        // inheritance setters, and vice versa.
        let neutral = RunActivation::new("t", Vec::new()).with_run_id_hint("x");
        assert!(neutral.capture.event_buffer.is_none());
        assert!(neutral.inherited.pinned_registry_set.is_none());
        assert_eq!(neutral.persistence.run_id_hint.as_deref(), Some("x"));
    }
}
