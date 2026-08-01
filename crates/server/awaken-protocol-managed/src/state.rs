//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; a tool-use event keeps
//! the tool call's own id so a `user.tool_confirmation` can reference it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::page::paginate_by_id;

use crate::preview::{PreviewAllocations, PreviewSink};
use crate::project::{self, project_messages, project_messages_with_mcp_ids, project_step};
use crate::routes::vaults::{SessionCredentialSource, VaultState};
use crate::types::{
    ConfirmResult, Event, EventReceipt, InboundEvent, ListEventsResponse, ModelConfig,
    ModelOverride, OutboundKind, SendEventsRequest, SendEventsResponse, Session, SessionAgent,
    SessionCreateParams, SessionError, SessionStats, SessionThread, SessionThreadAgent,
    SessionThreadStatus, StopReason, StreamFrame, Usage,
};
use awaken_session_contract::{ManagedLifecycleFact, ManagedSessionRepository, PersistedSession};
use awaken_session_store::SqliteManagedSessionRepository;

/// The seeded owner scope a bare/self-hosted session is created under when the
/// edge resolved no workspace (ADR-0051 / ADR-0048 D2 "seeded, not absent"). It
/// matches the request scope the ownership guard derives for an unscoped request,
/// so a single-tenant deployment never 404s itself.
pub(crate) const DEFAULT_SCOPE: &str = "default";

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
pub(crate) const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// Distinguishes multiple Managed adapters constructed inside one process tick
/// (tests and embedded multi-tenant composition). Production still normally has
/// one adapter per Coordinator process.
static MANAGED_STATE_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

/// The Managed Agents contract error for a `memory_store` add/remove on a running
/// session — memory stores bind at session creation only.
const MEMORY_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; \
     adding or removing one from a running session is not supported";

mod application;
mod deployment_sessions;
mod environment;
mod events;
mod helpers;
#[path = "state/lifecycle_event.rs"]
pub mod lifecycle_event;
mod mcp_attachment;
mod realization;
mod resource;
mod resources;
mod sandbox_provisioning;
mod session_create_idempotency;
mod session_record;
mod session_update;
pub(crate) use session_update::SessionUpdateCommand;
mod sessions;
mod threads;
mod types;

pub(crate) use helpers::{content_text, lifecycle_fact, rubric_text, session_usage_value};
use mcp_attachment::UnsupportedMcpAttachmentRealizer;
pub(crate) use resource::{
    ParsedInputTarget, ParsedSessionInput, input_binding, resolved_resource_dto,
    resource_binding_id,
};
use session_record::SessionRecord;
pub use types::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, OutcomeIteration, OutcomeReport, Pending, RunError, RunErrorKind,
    SessionInit, SessionRuntime, SessionUsage, StepOutcome, ToolPermissionDecision,
};

/// The adapter's in-memory session store plus the runtime port.
pub struct ManagedState {
    runtime: Arc<dyn SessionRuntime>,
    /// Sole external realization port for initial and hot MCP generations.
    /// Desired state remains in the Session aggregate; this adapter owns only
    /// stage/publish/drain effects and their secret-free receipts.
    mcp_realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    /// The vault surface, when the server mounts one (ADR-0043 Phase 3): a
    /// session's `mcp_servers` are bound to vault credentials through it at
    /// creation. `None` means every binding resolves to no credential.
    credential_source: Option<Arc<dyn SessionCredentialSource>>,
    /// The environments surface, when the server mounts one: a session's
    /// `environment_id` is resolved to its networking policy (egress on/off) at
    /// creation. `None` → every session gets host network (unrestricted).
    environments: Arc<crate::routes::environments::EnvironmentExecutionState>,
    /// The config-plane agent projection source (ADR-0043): when wired, a session
    /// referencing an agent published on the config plane inherits that agent's
    /// authoritative `model` (the config plane owns model/system/tools), so it runs
    /// the agent's model instead of the host default. Reuses the same
    /// [`awaken_executable_agent_contract::ExecutableAgentProfileSource`] port
    /// `/v1/agents` reads — no second source of agent truth. `None` → fall back
    /// to the host default model.
    config_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    /// Resource authoring/resolution port. The Managed ACL lowers compatibility
    /// Repository URL/token input into catalog/vault references; the catalog owns
    /// no principal or authorization policy.
    resource_catalog: Option<Arc<dyn awaken_resource_contract::ResourceCatalog>>,
    resource_purge_scheduler: Option<Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    /// The aspect-layer session→owner index (ADR-0051): the [`ScopeId`] that
    /// created each session, keyed by the tenancy-agnostic session id. It is NOT
    /// on the core session aggregate (which stays tenancy-agnostic) — it lives
    /// here so the edge ownership guard can 404 a cross-tenant request without the
    /// core ever reading a scope. Populated at `create_session` from the
    /// edge-resolved owner; read by [`ManagedState::owner_scope`].
    owners: Mutex<HashMap<String, String>>,
    /// Durable-config source of truth for the session aggregate: `create` writes
    /// it, rehydration reads it so a restored session reports its real
    /// agent/model/title/metadata/MCP instead of placeholder defaults. The
    /// in-memory `sessions` map is a per-process read-through cache over it.
    sessions_repo: Arc<dyn ManagedSessionRepository>,
    /// Optional projection sink for committed session lifecycle facts (ADR-0048):
    /// the assembly wires a webhook dispatcher here so a created/terminated session
    /// fans out to workspace-scoped subscriptions. `None` = no projection (the
    /// default, byte-identical to before). The wire crate stays webhook-agnostic —
    /// it only knows this narrow port.
    lifecycle_sink: Option<Arc<dyn SessionLifecycleSink>>,
    /// Unique process incarnation persisted in Session realization leases. A
    /// restarted process must acquire a higher epoch before recreating effects.
    runtime_incarnation: String,
    session_seq: AtomicU64,
    /// Shared with each turn's [`PreviewSink`] so a preview's minted `agent.message`
    /// id is drawn from the same `evt_N` sequence the committed event carries.
    event_seq: Arc<AtomicU64>,
    /// Per-session live SSE broadcast: `append_step`/`append_outcome` publish
    /// committed [`Event`]s here (Phase 1) and each turn's `PreviewSink` publishes
    /// `event_start`/`event_delta` previews (Phase 2). A `stream_events` connection
    /// subscribes; senders are created lazily on first publish/subscribe and never
    /// removed (a dropped session's channel is just an idle allocation).
    live: Mutex<HashMap<String, broadcast::Sender<StreamFrame>>>,
}

/// A sink for committed session lifecycle facts, projected to external consumers
/// (webhooks). The Managed adapter calls it after a lifecycle transition commits,
/// handing the session's persisted owner (S3) so the consumer can stamp tenancy.
/// The port now lives in `awaken-session-contract` (a contract/ leaf); re-exported
/// here so existing `awaken_protocol_managed::…` paths keep resolving. The fact
/// catalog below (the projected wire event names) stays in this adapter.
pub use awaken_session_contract::SessionLifecycleSink;

/// Why a session operation failed (mapped to an HTTP status by the router).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("session not found")]
    NotFound,
    /// A write was sent to an archived (terminated, read-only) session; the router
    /// maps it to 409 `invalid_request_error`.
    #[error("session is archived and is read-only")]
    Archived,
    /// The root Session revision changed while a command was being compiled.
    /// Callers re-read and retry the complete command; stale snapshots are never
    /// merged or written back.
    #[error("session changed concurrently; read the latest revision and retry")]
    Conflict,
    #[error("session idempotency key was reused with another request")]
    IdempotencyMismatch,
    /// A session create named a vault that does not exist (`vault_ids`); the
    /// router maps it to the standard 404 envelope naming the vault id.
    #[error("vault `{0}` not found")]
    VaultNotFound(String),
    #[error(transparent)]
    Run(#[from] RunError),
    /// A live-inbox operation was refused (inactive queue, unknown message,
    /// or a stale reorder); the router maps each case to its own status.
    #[error(transparent)]
    LiveInbox(#[from] LiveInboxError),
}

impl ManagedState {
    pub(crate) async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Option<Option<crate::env_registry::EnvItem>> {
        Some(self.environments.get(environment_id).await)
    }

    pub(crate) fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.config_source
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(workspace_id, agent_id))
    }

    pub(crate) fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.config_source
            .as_ref()
            .and_then(|source| source.unavailable_delegate_in(workspace_id, agent_id))
    }

    pub fn new(runtime: impl SessionRuntime + 'static) -> Self {
        Self::from_ports(
            Arc::new(runtime),
            Arc::new(UnsupportedMcpAttachmentRealizer),
        )
    }

    /// Compose one object that implements both independent application ports.
    /// The shared `Arc` preserves one adapter instance without merging the
    /// Session turn lifecycle with MCP attachment realization.
    pub fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: SessionRuntime + awaken_session_contract::McpAttachmentRealizer + 'static,
    {
        let runtime = Arc::new(runtime);
        Self::from_ports(runtime.clone(), runtime)
    }

    fn from_ports(
        runtime: Arc<dyn SessionRuntime>,
        mcp_realizer: Arc<dyn awaken_session_contract::McpAttachmentRealizer>,
    ) -> Self {
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let sessions_repo: Arc<dyn ManagedSessionRepository> = Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .expect("open ephemeral managed Session repository"),
        );
        runtime.install_environment_binding_sink(Arc::new(
            crate::state::environment::RepositoryEnvironmentBindingSink::new(sessions_repo.clone()),
        ));
        Self {
            runtime,
            mcp_realizer,
            credential_source: None,
            environments: Arc::new(
                crate::routes::environments::EnvironmentExecutionState::default(),
            ),
            config_source: None,
            resource_catalog: None,
            resource_purge_scheduler: None,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            sessions_repo,
            lifecycle_sink: None,
            runtime_incarnation: format!(
                "managed:{}:{started_at}:{}",
                std::process::id(),
                MANAGED_STATE_INCARNATION_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            session_seq: AtomicU64::new(0),
            event_seq: Arc::new(AtomicU64::new(0)),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Wire a projection sink (a webhook dispatcher) so committed session lifecycle
    /// facts fan out to workspace-scoped subscribers (ADR-0048). Default: none.
    #[must_use]
    pub fn with_lifecycle_sink(mut self, sink: Arc<dyn SessionLifecycleSink>) -> Self {
        self.lifecycle_sink = Some(sink);
        self
    }

    /// Wire the environments surface, so `POST /v1/sessions` resolves the session's
    /// `environment_id` to its networking policy (egress on/off). Share the same
    /// Coordinator's one executable Environment projection and WorkQueue.
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<crate::routes::environments::EnvironmentExecutionState>,
    ) -> Self {
        self.environments = environments;
        self
    }

    /// Wire a durable session repository (e.g. SQLite alongside the transcript
    /// store) so a session's config survives a restart and is reported faithfully
    /// by another process. The default is in-memory (single-process behavior).
    #[must_use]
    pub fn with_session_repo(mut self, repo: Arc<dyn ManagedSessionRepository>) -> Self {
        self.runtime.install_environment_binding_sink(Arc::new(
            crate::state::environment::RepositoryEnvironmentBindingSink::new(repo.clone()),
        ));
        self.sessions_repo = repo;
        self
    }

    /// Wire recoverable physical cleanup scheduling. Authorization has already
    /// completed at the edge; this port receives resource identity only.
    #[must_use]
    pub fn with_resource_purge_scheduler(
        mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) -> Self {
        self.resource_purge_scheduler = Some(scheduler);
        self
    }

    /// Wire the vault surface, so `POST /v1/sessions` binds each requested MCP
    /// server to a vault credential by URL (ADR-0043 Phase 3). Share the same
    /// `VaultState` with [`crate::vault_router`], or the sessions and the vault
    /// routes see different credentials.
    #[must_use]
    pub fn with_vaults(mut self, vaults: Arc<VaultState>) -> Self {
        self.credential_source = Some(vaults);
        self
    }

    /// Wire the same secret-free credential-selection port through either the
    /// local VaultState adapter or the authenticated split-service adapter.
    #[must_use]
    pub fn with_credential_source(mut self, source: Arc<dyn SessionCredentialSource>) -> Self {
        self.credential_source = Some(source);
        self
    }

    /// Wire the config-plane agent projection source so a session inherits a
    /// published agent's authoritative `model`. Share the same
    /// [`awaken_executable_agent_contract::ExecutableAgentProfileSource`] that
    /// `/v1/agents` uses, or the Session and executable profile disagree.
    #[must_use]
    pub fn with_config_source(
        mut self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) -> Self {
        self.config_source = Some(source);
        self
    }

    /// Wire the platform Resource Catalog used to resolve Memory/Repository
    /// configuration once at Session creation.
    #[must_use]
    pub fn with_resource_catalog(
        mut self,
        catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    ) -> Self {
        self.resource_catalog = Some(catalog);
        self
    }

    fn next_event_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    /// The session's live SSE broadcast sender, created on first use. Capacity is
    /// generous so a fast turn's preview burst doesn't lag a slow subscriber into
    /// `Lagged` (which the stream tolerates by skipping). Never removed.
    fn live_sender(&self, session_id: &str) -> broadcast::Sender<StreamFrame> {
        let mut live = self.live.lock().unwrap();
        live.entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(1024).0)
            .clone()
    }

    /// Open a live SSE subscription for `session_id`: the current committed-event
    /// snapshot (backfill) plus a receiver for frames published after this call.
    /// Subscribing *before* cloning the snapshot means no committed event can slip
    /// through the gap — an event that lands mid-call is on the receiver, and the
    /// caller dedupes it against the snapshot by id.
    pub fn stream_subscribe(
        &self,
        session_id: &str,
    ) -> Result<(Vec<Event>, broadcast::Receiver<StreamFrame>), StateError> {
        let rx = self.live_sender(session_id).subscribe();
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok((record.events.clone(), rx))
    }

    /// Publish each committed `Event` appended to `session_id` since `from` on the
    /// live broadcast, so an open SSE connection receives it without a reconnect.
    /// Best-effort: no subscriber (or a lagging one) is not an error.
    fn broadcast_committed_from(&self, session_id: &str, record: &SessionRecord, from: usize) {
        if from >= record.events.len() {
            return;
        }
        if let Some(tx) = self.live.lock().unwrap().get(session_id) {
            for event in &record.events[from..] {
                let _ = tx.send(StreamFrame::Committed(event.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::Message;
    use std::collections::BTreeMap;

    fn ephemeral_session_repo() -> SqliteManagedSessionRepository {
        SqliteManagedSessionRepository::open_in_memory()
            .expect("open ephemeral managed Session repository")
    }

    async fn create_session_fixture(
        repo: &dyn ManagedSessionRepository,
        owner: &str,
        mut session: PersistedSession,
    ) {
        session.revision = awaken_session_contract::SessionRevision(0);
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        repo.create(
            owner,
            session.clone(),
            awaken_session_contract::IdempotencyRecord {
                key: format!("test:create:{}:{payload_hash}", session.session_id),
                payload_hash,
            },
            Vec::new(),
        )
        .await
        .expect("create Session fixture");
    }

    fn ephemeral_resource_catalog() -> awaken_resource_store::SqliteResourceStore {
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Catalog")
    }

    type RestoredRuntime = (
        String,
        Option<String>,
        usize,
        awaken_session_contract::SessionNetworkPolicy,
        serde_json::Value,
    );

    /// A runtime that reports a non-empty committed transcript, so a session can
    /// rehydrate. Every operational method is unused by these tests.
    #[derive(Clone, Default)]
    struct RehydrateFake {
        restored: Arc<
            std::sync::Mutex<
                Vec<(
                    String,
                    String,
                    awaken_session_contract::ResolvedSessionResources,
                )>,
            >,
        >,
        restored_environments: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
        restored_runtimes: Arc<std::sync::Mutex<Vec<RestoredRuntime>>>,
        delegated: Arc<std::sync::Mutex<Vec<DelegatedRun>>>,
        order: Arc<std::sync::Mutex<Vec<&'static str>>>,
        committed: Arc<std::sync::Mutex<Option<Vec<Message>>>>,
    }

    #[async_trait]
    impl SessionRuntime for RehydrateFake {
        async fn prepare_session(&self, thread: &str, init: SessionInit) -> Result<(), RunError> {
            self.order.lock().unwrap().push("runtime");
            self.restored_runtimes.lock().unwrap().push((
                thread.to_string(),
                init.runtime,
                0,
                init.environment.network,
                init.environment.sandbox,
            ));
            Ok(())
        }

        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }
        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }
        async fn committed_messages(&self, thread: &str) -> Vec<Message> {
            self.order.lock().unwrap().push("history");
            if let Some(messages) = self.committed.lock().unwrap().clone() {
                return messages;
            }
            vec![Message::text(
                awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )]
        }
        async fn delegated_runs(&self, _thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
            self.order.lock().unwrap().push("delegations");
            Ok(self.delegated.lock().unwrap().clone())
        }
        async fn restore_session_environment(
            &self,
            agent: &str,
            thread: &str,
            binding: &str,
        ) -> Result<(), RunError> {
            self.order.lock().unwrap().push("environment");
            self.restored_environments.lock().unwrap().push((
                agent.to_string(),
                thread.to_string(),
                binding.to_string(),
            ));
            Ok(())
        }
        fn model(&self) -> String {
            "host-default-model".to_string()
        }
        async fn apply_session_inputs(
            &self,
            thread: &str,
            workspace_id: &str,
            inputs: &awaken_session_contract::ResolvedSessionResources,
        ) -> Result<(), RunError> {
            self.order.lock().unwrap().push("resources");
            self.restored.lock().unwrap().push((
                thread.to_string(),
                workspace_id.to_string(),
                inputs.clone(),
            ));
            Ok(())
        }
    }

    #[async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for RehydrateFake {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_contract::StageMcpAttachment,
        ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
            if let Some(restored) = self
                .restored_runtimes
                .lock()
                .unwrap()
                .iter_mut()
                .find(|restored| restored.0 == request.generation.session_id)
            {
                restored.2 += 1;
            }
            let receipt_fingerprint = request.fingerprint();
            Ok(awaken_session_contract::McpRealizationReceipt {
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
                receipt_fingerprint,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), RunError> {
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), RunError> {
            Ok(())
        }
    }

    /// A runtime that records every `end_session` thread it is asked to tear down,
    /// so a test can prove the terminal edges (delete/archive) reach the host's
    /// sandbox disposal rather than leaking it. Every driving method is unused.
    #[derive(Clone, Default)]
    struct EndSessionRecorder {
        ended: Arc<std::sync::Mutex<Vec<String>>>,
        interrupted: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl SessionRuntime for EndSessionRecorder {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }
        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }
        async fn end_session(&self, thread: &str) -> Result<(), RunError> {
            self.ended.lock().unwrap().push(thread.to_string());
            Ok(())
        }
        async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
            self.interrupted.lock().unwrap().push(thread.to_string());
            Ok(())
        }
        fn model(&self) -> String {
            "host-default-model".to_string()
        }
    }

    /// Cause graph: exact child -> runtime termination -> one terminal projection.
    /// Runtime failure stops before status/event mutation; a repeated successful
    /// archive observes the terminal projection and performs no second effect.
    ///
    /// | Child | Runtime | Prior status | Result | Runtime calls | terminal events |
    /// |---|---|---|---|---|---|
    /// | absent | n/a | n/a | 404 | 0 | 0 |
    /// | present | fail | running | error, still running | 1 | 0 |
    /// | present | succeed | running | terminated | 1 | 1 |
    /// | present | n/a | terminated | same receipt | unchanged | unchanged |
    #[tokio::test]
    async fn child_thread_archive_is_runtime_backed_fail_closed_and_idempotent() {
        let runtime = EndSessionRecorder::default();
        let ended = runtime.ended.clone();
        let state = ManagedState::new(runtime);
        let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let session = state.create_session(request, None).await.unwrap();
        {
            let mut sessions = state.sessions.lock().unwrap();
            let record = sessions.get_mut(&session.id).unwrap();
            record.child_threads.push(ManagedState::child_thread(
                &record.session,
                "child-1",
                "researcher",
            ));
        }

        let archived = state.archive_thread(&session.id, "child-1").await.unwrap();
        assert_eq!(archived.status, SessionThreadStatus::Terminated);
        assert_eq!(ended.lock().unwrap().as_slice(), &["child-1"]);
        let terminal_count = state
            .list_events(&session.id, None, None)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "session.thread_status_terminated")
            .count();
        assert_eq!(terminal_count, 1);

        state.archive_thread(&session.id, "child-1").await.unwrap();
        assert_eq!(ended.lock().unwrap().as_slice(), &["child-1"]);
        assert_eq!(
            state
                .list_events(&session.id, None, None)
                .unwrap()
                .data
                .iter()
                .filter(|event| event.type_str() == "session.thread_status_terminated")
                .count(),
            1
        );
    }

    /// Cause/effect graph: optional `session_thread_id` -> canonical runtime
    /// Thread selection -> interrupt side effects. A named live Thread selects
    /// exactly itself; an absent selector fans out to the primary and every
    /// non-terminal child; an unknown or terminal selector fails admission before
    /// the receipt/event log or runtime changes.
    ///
    /// Decision table:
    /// | rule | selector | target state | runtime keys | persisted receipt |
    /// |---|---|---|---|---|
    /// | I1 | child id | idle/requires-action | child only | yes |
    /// | I2 | primary id | live | Session id only | yes |
    /// | I3 | absent | mixed | primary + non-terminal children | yes |
    /// | I4 | child id | terminated/unknown | none | no |
    #[tokio::test]
    async fn interrupt_selector_targets_one_thread_or_all_non_terminal_threads() {
        let runtime = EndSessionRecorder::default();
        let interrupted = runtime.interrupted.clone();
        let state = ManagedState::new(runtime);
        let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let session = state.create_session(request, None).await.unwrap();
        {
            let mut sessions = state.sessions.lock().unwrap();
            let record = sessions.get_mut(&session.id).unwrap();
            let mut idle = ManagedState::child_thread(&record.session, "child-idle", "researcher");
            idle.status = SessionThreadStatus::Idle;
            record.child_threads.push(idle);
            let mut terminated =
                ManagedState::child_thread(&record.session, "child-ended", "reviewer");
            terminated.status = SessionThreadStatus::Terminated;
            terminated.archived_at = Some(PROCESSED_AT.to_string());
            record.child_threads.push(terminated);
        }

        let send = |event| SendEventsRequest {
            events: vec![event],
            user_profile_id: None,
        };
        state
            .send_events(
                &session.id,
                send(InboundEvent::UserInterrupt {
                    session_thread_id: Some("child-idle".into()),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            interrupted.lock().unwrap().as_slice(),
            &["child-idle"],
            "I1"
        );
        assert_eq!(
            state
                .list_thread_events(&session.id, "child-idle", None, None)
                .unwrap()
                .data
                .iter()
                .filter(|event| event.type_str() == "user.interrupt")
                .count(),
            1,
            "I1 is visible on the selected child Thread stream"
        );

        interrupted.lock().unwrap().clear();
        state
            .send_events(
                &session.id,
                send(InboundEvent::UserInterrupt {
                    session_thread_id: Some(format!("{}:primary", session.id)),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            interrupted.lock().unwrap().as_slice(),
            &[session.id.as_str()],
            "I2"
        );

        interrupted.lock().unwrap().clear();
        state
            .send_events(
                &session.id,
                send(InboundEvent::UserInterrupt {
                    session_thread_id: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            interrupted.lock().unwrap().as_slice(),
            &[session.id.as_str(), "child-idle"],
            "I3 excludes the terminal child"
        );
        assert_eq!(
            state
                .list_thread_events(&session.id, "child-idle", None, None)
                .unwrap()
                .data
                .iter()
                .filter(|event| event.type_str() == "user.interrupt")
                .count(),
            2,
            "I3's selector-free interrupt is visible on every live child stream"
        );

        for rejected in ["child-ended", "child-unknown"] {
            interrupted.lock().unwrap().clear();
            let event_count = state
                .list_events(&session.id, None, None)
                .unwrap()
                .data
                .len();
            let error = state
                .send_events(
                    &session.id,
                    send(InboundEvent::UserInterrupt {
                        session_thread_id: Some(rejected.into()),
                    }),
                )
                .await
                .expect_err("I4 rejects a non-live selector");
            assert!(matches!(error, StateError::Run(_)), "I4: {error}");
            assert!(interrupted.lock().unwrap().is_empty(), "I4");
            assert_eq!(
                state
                    .list_events(&session.id, None, None)
                    .unwrap()
                    .data
                    .len(),
                event_count,
                "I4 admission is atomic"
            );
        }
    }

    // Delete finalization tests are generated from this causal graph:
    //
    // C1 terminal CAS committed ──> E1 public reads are NotFound
    //                         └───> C2 external cleanup attempted
    // C2 cleanup succeeds ────────> E2 durable row becomes a tombstone
    // C2 cleanup fails ───────────> E3 hidden cleanup row remains pending
    // E3 + C3 later retry succeeds -> E2
    //
    // Decision table ("cleanup" includes sandbox and Repository cleanup):
    //
    // | Rule | C1 | C2 | C3 | E1 | E2 | E3 |
    // |------|----|----|----|----|----|----|
    // | D1   | T  | T  | -  | T  | T  | F  |
    // | D2   | T  | F  | F  | T  | F  | T  |
    // | D3   | T  | F  | T  | T  | T  | F  |
    //
    // D1, D2 and D3 respectively generate the success, failure, and restart
    // recovery tests below. No test invents a second cleanup implementation.

    /// D1: `DELETE /v1/sessions/{id}` reaches the host's terminal sandbox
    /// disposal and converges the hidden durable row to a tombstone.
    #[tokio::test]
    async fn delete_session_disposes_the_host_sandbox() {
        let rt = EndSessionRecorder::default();
        let ended = rt.ended.clone();
        let repo = Arc::new(ephemeral_session_repo());
        let state = ManagedState::new(rt).with_session_repo(repo.clone());
        let id = state
            .create_session(bare_create_params(), None)
            .await
            .expect("create")
            .id;
        state.delete_session(&id).await.expect("delete");
        assert_eq!(
            *ended.lock().unwrap(),
            vec![id.clone()],
            "delete tears down the session's sandbox via end_session"
        );
        assert!(
            repo.get(&id).await.is_none(),
            "successful cleanup converges to a tombstone"
        );
    }

    /// `POST /v1/sessions/{id}/archive` reaps the sandbox on the terminal
    /// transition only — a re-archive (idempotent) does not re-dispose.
    #[tokio::test]
    async fn archive_session_disposes_on_the_terminal_transition_only() {
        let rt = EndSessionRecorder::default();
        let ended = rt.ended.clone();
        let state = ManagedState::new(rt);
        let id = state
            .create_session(bare_create_params(), None)
            .await
            .expect("create")
            .id;
        state.archive_session(&id).await.expect("archive");
        assert_eq!(
            *ended.lock().unwrap(),
            vec![id.clone()],
            "archive reaps the sandbox on the terminal transition"
        );
        state.archive_session(&id).await.expect("re-archive");
        assert_eq!(
            *ended.lock().unwrap(),
            vec![id],
            "a re-archive (idempotent) does not re-dispose"
        );
    }

    #[tokio::test]
    async fn archive_persists_release_before_and_after_sandbox_teardown() {
        let repo = Arc::new(ephemeral_session_repo());
        let state =
            ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
        let request = serde_json::from_value(serde_json::json!({
            "agent": "assistant",
            "resources": [{
                "type": "file",
                "file_id": "immutable-file",
                "mount_path": "/input.txt"
            }]
        }))
        .unwrap();
        let id = state.create_session(request, None).await.unwrap().id;
        assert_eq!(
            repo.get(&id).await.unwrap().resources.activations[0].state,
            awaken_session_contract::ActivationState::Active
        );

        state.archive_session(&id).await.unwrap();
        let durable = repo.get(&id).await.unwrap();
        assert_eq!(durable.status, "terminated");
        assert_eq!(
            durable.resources.activations[0].state,
            awaken_session_contract::ActivationState::Released
        );
        assert!(repo.reconcilable_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn session_create_enforces_the_500_file_boundary() {
        // Cause/effect rules for the Managed file-count limit:
        // R1: C1=file_count=500 → E1=create succeeds.
        // R2: C2=file_count=501 → E2=bad request before any Session persists.
        let request = |count: usize| {
            let resources = (0..count)
                .map(|index| {
                    serde_json::json!({
                        "type": "file",
                        "file_id": format!("file_{index}"),
                        "mount_path": format!("/input-{index}.txt")
                    })
                })
                .collect::<Vec<_>>();
            serde_json::from_value(serde_json::json!({
                "agent": "assistant",
                "resources": resources
            }))
            .unwrap()
        };
        let state = ManagedState::new(EndSessionRecorder::default());
        assert!(state.create_session(request(500), None).await.is_ok());
        let error = state.create_session(request(501), None).await.unwrap_err();
        assert!(error.to_string().contains("at most 500 files"), "{error}");
    }

    /// A runtime whose sandbox teardown always fails — to prove the terminal edges
    /// are BEST-EFFORT: a dispose failure is logged, never propagated, so it cannot
    /// resurrect a deleted session.
    struct EndSessionFailer;

    #[async_trait]
    impl SessionRuntime for EndSessionFailer {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }
        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }
        async fn end_session(&self, _thread: &str) -> Result<(), RunError> {
            Err(RunError::internal("sandbox dispose blew up"))
        }
        fn model(&self) -> String {
            "host-default-model".to_string()
        }
    }

    #[tokio::test]
    async fn child_thread_archive_failure_commits_no_terminal_projection() {
        let state = ManagedState::new(EndSessionFailer);
        let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let session = state.create_session(request, None).await.unwrap();
        {
            let mut sessions = state.sessions.lock().unwrap();
            let record = sessions.get_mut(&session.id).unwrap();
            record.child_threads.push(ManagedState::child_thread(
                &record.session,
                "child-fails",
                "researcher",
            ));
        }

        let error = state
            .archive_thread(&session.id, "child-fails")
            .await
            .expect_err("runtime failure must fail closed");
        assert!(error.to_string().contains("sandbox dispose blew up"));
        assert_eq!(
            state.get_thread(&session.id, "child-fails").unwrap().status,
            SessionThreadStatus::Running
        );
        assert!(
            !state
                .list_events(&session.id, None, None)
                .unwrap()
                .data
                .iter()
                .any(|event| event.type_str() == "session.thread_status_terminated")
        );
    }

    /// D2: a sandbox teardown failure at delete is swallowed (best-effort): the delete is
    /// terminal, so the session is still removed and reads 404 afterwards — a dispose
    /// error must never leave a "deleted" session alive.
    #[tokio::test]
    async fn delete_is_best_effort_when_sandbox_teardown_fails() {
        let repo = Arc::new(ephemeral_session_repo());
        let state = ManagedState::new(EndSessionFailer).with_session_repo(repo.clone());
        let request = serde_json::from_value(serde_json::json!({
            "agent": "assistant",
            "resources": [{
                "type": "file",
                "file_id": "immutable-file",
                "mount_path": "/input.txt"
            }]
        }))
        .unwrap();
        let id = state
            .create_session(request, None)
            .await
            .expect("create")
            .id;
        state
            .delete_session(&id)
            .await
            .expect("delete stays terminal despite a sandbox teardown failure");
        assert!(
            matches!(state.get_session(&id), Err(StateError::NotFound)),
            "the session is gone even though its sandbox dispose errored"
        );
        let durable = repo.get(&id).await.unwrap();
        assert_eq!(durable.status, "deleted");
        assert_eq!(
            durable.resources.activations[0].state,
            awaken_session_contract::ActivationState::Releasing,
            "cleanup failure stays durable for ResourceReclaimer"
        );
        assert_eq!(
            repo.reconcilable_sessions().await,
            vec![awaken_session_contract::ScopedPersistedSession {
                workspace_id: DEFAULT_SCOPE.to_string(),
                session: durable,
            }]
        );
    }

    fn sample_persisted(id: &str) -> PersistedSession {
        let mut metadata = BTreeMap::new();
        metadata.insert("team".to_string(), "research".to_string());
        let holder = awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Workload,
            "awaken.workload.acp",
        );
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env_local".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-1".into()),
            sandbox: serde_json::json!({"isolation": "namespace"}),
            sandbox_provisioning: Default::default(),
            packages: Default::default(),
            network: awaken_session_contract::SessionNetworkPolicy::None,
            credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                inference_holder: holder.clone(),
                mcp_holder: holder.clone(),
                resource_holder: holder,
            },
        };
        let mut mcp = awaken_session_contract::SessionMcpAttachmentSet::from_initial(
            vec![awaken_session_contract::McpAttachmentDraft {
                name: "calc".into(),
                target: awaken_session_contract::McpTarget::parse_http("https://x").unwrap(),
                credential: None,
                prompts_as_skills: false,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
        mcp.attachments[0].state = awaken_session_contract::McpAttachmentState::Active;
        PersistedSession {
            session_id: id.to_string(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Frozen(
                awaken_session_contract::SessionBaseline::compile(
                    awaken_session_contract::SessionBaselineInputs {
                        environment,
                        mcp_authoring: Default::default(),
                        agent_id: "coder".into(),
                        model: "kimi-k2".into(),
                        runtime: Some("acp:custom".into()),
                        application: None,
                        delegate_ids: Vec::new(),
                        toolsets: Vec::new(),
                        mounts: Vec::new(),
                        env: Vec::new(),
                        prompts: Vec::new(),
                    },
                ),
            ),
            title: Some("My session".to_string()),
            metadata,
            tools: Default::default(),
            environment_binding: None,
            mcp,
            resources: awaken_session_contract::SessionResourceState::from_legacy(sample_inputs()),
            realization: None,
            status: "idle".into(),
            archived_at: None,
        }
    }

    #[tokio::test]
    async fn immediate_environment_binding_sink_is_durable_and_idempotent() {
        use awaken_session_contract::SessionEnvironmentBindingSink;

        let repo = Arc::new(ephemeral_session_repo());
        create_session_fixture(
            repo.as_ref(),
            DEFAULT_SCOPE,
            sample_persisted("binding-now"),
        )
        .await;
        let sink = crate::state::environment::RepositoryEnvironmentBindingSink::new(repo.clone());

        sink.persist("binding-now", "opaque-handle").await.unwrap();
        let first = repo.get("binding-now").await.unwrap();
        assert_eq!(first.environment_binding.as_deref(), Some("opaque-handle"));
        sink.persist("binding-now", "opaque-handle").await.unwrap();
        let replay = repo.get("binding-now").await.unwrap();
        assert_eq!(replay.revision, first.revision);
        assert_eq!(replay.environment_binding, first.environment_binding);
    }

    struct ConflictInjectingRepo {
        inner: Arc<SqliteManagedSessionRepository>,
        conflicts: AtomicU64,
    }

    #[async_trait]
    impl ManagedSessionRepository for ConflictInjectingRepo {
        async fn create(
            &self,
            owner_scope: &str,
            session: PersistedSession,
            idempotency: awaken_session_contract::IdempotencyRecord,
            facts: Vec<awaken_session_contract::ManagedLifecycleFact>,
        ) -> Result<
            awaken_session_contract::SessionRevision,
            awaken_session_contract::SessionRepositoryError,
        > {
            self.inner
                .create(owner_scope, session, idempotency, facts)
                .await
        }

        async fn commit_mutation(
            &self,
            owner_scope: &str,
            mutation: awaken_session_contract::SessionMutation,
        ) -> Result<
            awaken_session_contract::SessionMutationResult,
            awaken_session_contract::SessionRepositoryError,
        > {
            if self
                .conflicts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Ok(awaken_session_contract::SessionMutationResult::Conflict {
                    current_revision: mutation.expected_revision,
                });
            }
            self.inner.commit_mutation(owner_scope, mutation).await
        }

        async fn append_lifecycle(&self, fact: awaken_session_contract::ManagedLifecycleFact) {
            self.inner.append_lifecycle(fact).await;
        }

        async fn pending_lifecycle(&self) -> Vec<awaken_session_contract::ManagedLifecycleFact> {
            self.inner.pending_lifecycle().await
        }

        async fn complete_lifecycle(&self, fact_id: &str) {
            self.inner.complete_lifecycle(fact_id).await;
        }

        async fn get(&self, session_id: &str) -> Option<PersistedSession> {
            self.inner.get(session_id).await
        }

        async fn owner(&self, session_id: &str) -> Option<String> {
            self.inner.owner(session_id).await
        }
    }

    #[tokio::test]
    async fn immediate_binding_cas_retries_once_then_fails_closed_at_the_bound() {
        use awaken_session_contract::SessionEnvironmentBindingSink;

        for (case, conflicts, accepted) in [
            ("one conflict then success", 1, true),
            ("three conflicts exhaust bound", 3, false),
        ] {
            let inner = Arc::new(ephemeral_session_repo());
            create_session_fixture(
                inner.as_ref(),
                DEFAULT_SCOPE,
                sample_persisted(&format!("binding-{conflicts}")),
            )
            .await;
            let repo = Arc::new(ConflictInjectingRepo {
                inner: inner.clone(),
                conflicts: AtomicU64::new(conflicts),
            });
            let sink = crate::state::environment::RepositoryEnvironmentBindingSink::new(repo);
            let result = sink
                .persist(&format!("binding-{conflicts}"), "opaque")
                .await;
            assert_eq!(result.is_ok(), accepted, "{case}");
            assert_eq!(
                inner
                    .get(&format!("binding-{conflicts}"))
                    .await
                    .unwrap()
                    .environment_binding
                    .as_deref(),
                accepted.then_some("opaque"),
                "{case}"
            );
        }
    }

    fn sample_inputs() -> awaken_session_contract::ResolvedSessionResources {
        awaken_session_contract::ResolvedSessionResources {
            inputs: vec![awaken_session_contract::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::from("input-file"),
                source: awaken_session_contract::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file-hash"),
                },
                mount_path: "/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            skills: None,
        }
    }

    #[derive(Clone, Copy)]
    enum ApplicationCasRule {
        Apply,
        Replay,
        Stale,
    }

    #[tokio::test]
    async fn application_root_cas_decision_table() {
        // Cause-effect graph:
        // C1 payload/key already committed -> E1 replay the committed revision.
        // !C1 + C2 expected root revision is current -> E2 apply once.
        // !C1 + !C2 -> E3 conflict; the stale snapshot is never merged.
        //
        // | Rule | C1 same receipt | C2 current revision | Effect |
        // |------|-----------------|---------------------|--------|
        // | A1   | F               | T                   | apply  |
        // | A2   | T               | -                   | replay |
        // | A3   | F               | F                   | 409    |
        //
        // The rows generate the cases below against the real SQLite adapter and
        // the one application command compiler, not a duplicate fake algorithm.
        for (index, rule) in [
            ApplicationCasRule::Apply,
            ApplicationCasRule::Replay,
            ApplicationCasRule::Stale,
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("sesn_application_cas_{index}");
            let repo = Arc::new(ephemeral_session_repo());
            create_session_fixture(repo.as_ref(), "workspace-a", sample_persisted(&id)).await;
            let stale = repo.get(&id).await.unwrap();
            let state = ManagedState::new_with_mcp(RehydrateFake::default())
                .with_session_repo(repo.clone());
            let applied = state
                .commit_session_snapshot(
                    "workspace-a",
                    stale.clone(),
                    "decision-table-first",
                    Vec::new(),
                )
                .await
                .unwrap();
            assert_eq!(
                applied.revision,
                awaken_session_contract::SessionRevision(2)
            );

            match rule {
                ApplicationCasRule::Apply => {}
                ApplicationCasRule::Replay => {
                    let replayed = state
                        .commit_session_snapshot(
                            "workspace-a",
                            stale,
                            "decision-table-first",
                            Vec::new(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(replayed.revision, applied.revision);
                }
                ApplicationCasRule::Stale => {
                    assert!(matches!(
                        state
                            .commit_session_snapshot(
                                "workspace-a",
                                stale,
                                "decision-table-stale",
                                Vec::new(),
                            )
                            .await,
                        Err(StateError::Conflict)
                    ));
                    assert_eq!(repo.get(&id).await.unwrap().revision, applied.revision);
                }
            }
        }
    }

    #[test]
    fn rehydrated_session_restores_persisted_config() {
        // Cause graph: a durable mutable tool set is the exact replacement;
        // only a genuinely non-durable in-memory Session derives Runtime defaults.
        //
        // | Rule | Persisted tools | Projection |
        // |---|---|---|
        // | T1 | durable row, including empty | exact durable value |
        // | T2 | no durable row | Runtime default for transient projection |
        let state = ManagedState::new_with_mcp(RehydrateFake::default());
        let mut persisted = sample_persisted("sesn_1");
        persisted.tools =
            crate::project::session_tool_configuration(&[crate::types::agent::AgentTool::Custom {
                name: "durable-tool".into(),
                description: "Client-executed tool".into(),
                input_schema: crate::types::agent::CustomToolInputSchema::from_value(
                    serde_json::json!({"type": "object"}),
                )
                .unwrap(),
            }]);
        let session = state
            .rehydrated_session("sesn_1", Some(persisted))
            .expect("valid durable projection");
        assert_eq!(session.agent.id, "coder");
        assert_eq!(session.agent.model.id, "kimi-k2");
        assert_eq!(session.title.as_deref(), Some("My session"));
        assert_eq!(
            session.metadata.get("team").map(String::as_str),
            Some("research")
        );
        assert_eq!(
            session.agent.mcp_servers.len(),
            1,
            "the accepted MCP server is restored"
        );
        assert!(matches!(
            &session.agent.tools[..],
            [crate::types::agent::AgentTool::Custom { name, .. }] if name == "durable-tool"
        ));
        assert!(
            session.resources.is_empty(),
            "the stored Session DTO must not duplicate typed resource state"
        );
    }

    #[test]
    fn rehydrated_session_falls_back_without_persisted_config() {
        let state = ManagedState::new_with_mcp(RehydrateFake::default());
        let session = state
            .rehydrated_session("sesn_1", None)
            .expect("legacy fallback projection");
        assert_eq!(session.agent.id, "assistant");
        assert_eq!(session.agent.model.id, "host-default-model");
        assert!(session.title.is_none());
        assert!(session.agent.mcp_servers.is_empty());
    }

    #[test]
    fn persisted_session_rejects_corrupt_tools_before_rehydration() {
        // Causal graph:
        // durable tool projection is present but invalid
        //   -> typed store decoding fails
        //   -> Runtime defaults are not substituted
        //   -> caller cannot cache or expose a weaker Session.
        //
        // Decision table:
        // | Durable field | Shape | Expected behavior |
        // | absent | n/a | decode the explicit empty default for legacy rows |
        // | present | valid typed tool | restore exact tool |
        // | present | invalid | decoding error; no fallback |
        let persisted = sample_persisted("sesn_corrupt");
        let mut value = serde_json::to_value(persisted).unwrap();
        value["tools"] = serde_json::json!({"unexpected": true});

        assert!(
            serde_json::from_value::<PersistedSession>(value).is_err(),
            "corrupt durable capabilities fail at the store decoding boundary and cannot reach rehydration"
        );
    }

    #[tokio::test]
    async fn ensure_session_rehydrates_from_repo_after_cache_loss() {
        // Causal graph:
        // durable Session -> one canonical projection preparation -> environment
        // adoption -> committed history -> readable in-memory Session.
        //
        // Decision table:
        // | MCP state            | preparation owner          | calls |
        // | needs reconciliation | realization synchronizer  | one   |
        // | already settled      | ensure_session             | one   |
        // Both branches must converge before environment/history; duplicate
        // preparation can repeat mounts, runtime registration, and secret staging.
        // A session created in one process is gone from a fresh process's cache,
        // but the shared repo + committed transcript restore it faithfully.
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let mut persisted = sample_persisted("sesn_1");
        persisted.environment_binding = Some("opaque-runtime-binding".to_string());
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, persisted).await;

        // Fresh state (empty cache) sharing the durable repo — simulates a restart.
        let runtime = RehydrateFake::default();
        runtime.delegated.lock().unwrap().push(DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-durable".into()),
            parent_call_id: "call-durable".into(),
            agent_id: "researcher".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Completed,
        });
        let restored = runtime.restored.clone();
        let restored_environments = runtime.restored_environments.clone();
        let restored_runtimes = runtime.restored_runtimes.clone();
        let order = runtime.order.clone();
        let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());
        restarted.ensure_session("sesn_1").await.expect("rehydrate");
        let session = restarted
            .get_session("sesn_1")
            .expect("session present after rehydrate");
        assert_eq!(
            session.agent.id, "coder",
            "real agent id, not the placeholder"
        );
        assert_eq!(session.title.as_deref(), Some("My session"));
        assert_eq!(session.agent.mcp_servers.len(), 1);
        assert_eq!(session.resources.len(), 1);
        assert!(matches!(
            &session.resources[0],
            crate::types::resource::SessionResource::File { file_id, .. }
                if file_id == "file-hash"
        ));
        let threads = restarted.list_threads("sesn_1").expect("threads restored");
        assert_eq!(threads.len(), 2, "primary plus the durable runtime child");
        assert!(threads.iter().any(|thread| {
            thread.id == "child-durable"
                && thread.agent.id == "researcher"
                && thread.status == SessionThreadStatus::Idle
        }));
        assert_eq!(
            restored.lock().unwrap().as_slice(),
            &[(
                "sesn_1".to_string(),
                DEFAULT_SCOPE.to_string(),
                sample_inputs(),
            )],
            "restart replays the persisted manifest once without re-resolving it"
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &[
                "resources",
                "runtime",
                "environment",
                "history",
                "delegations"
            ],
            "resources must be staged before environment adoption and history opening"
        );
        assert_eq!(
            restored_runtimes.lock().unwrap().as_slice(),
            &[(
                "sesn_1".to_string(),
                Some("acp:custom".to_string()),
                1,
                awaken_session_contract::SessionNetworkPolicy::None,
                serde_json::json!({"isolation": "namespace"}),
            )],
            "the complete secret-free runtime pin is restored"
        );
        assert_eq!(
            restored_environments.lock().unwrap().as_slice(),
            &[(
                "coder".to_string(),
                "sesn_1".to_string(),
                "opaque-runtime-binding".to_string(),
            )],
            "the runtime alone receives and interprets the opaque binding"
        );
        let durable = repo.get("sesn_1").await.unwrap();
        assert_eq!(durable.resources.activations.len(), 1);
        assert_eq!(
            durable.resources.activations[0].state,
            awaken_session_contract::ActivationState::Active,
            "the first recovery adopts a durable activation record for a legacy manifest"
        );
    }

    #[tokio::test]
    async fn committed_event_refresh_merges_peer_messages_exactly_once() {
        // Cause/effect graph:
        // C1 a durable Session is already cached on Coordinator A;
        // C2 Coordinator B commits a new Runtime message to the shared transcript;
        // C3 A refreshes once or repeatedly through the public-read seam.
        // E1 A exposes both committed messages; E2 each message is projected once;
        // E3 the in-memory cache never becomes an alternative source of truth.
        //
        // Decision table:
        // | cache | transcript delta | refresh count | result                 |
        // | warm  | none             | one          | unchanged              |
        // | warm  | one peer message | one          | append peer projection |
        // | warm  | same peer message| repeated     | no duplicate           |
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, sample_persisted("sesn_peer")).await;
        let runtime = RehydrateFake::default();
        let first = Message::text(
            awaken_agent_contract::agent::message::Id("peer-first".into()),
            awaken_agent_contract::agent::message::Role::Assistant,
            "first",
        );
        *runtime.committed.lock().unwrap() = Some(vec![first.clone()]);
        let state = ManagedState::new_with_mcp(runtime.clone()).with_session_repo(repo);
        state.ensure_session("sesn_peer").await.expect("warm cache");

        let second = Message::text(
            awaken_agent_contract::agent::message::Id("peer-second".into()),
            awaken_agent_contract::agent::message::Role::Assistant,
            "second",
        );
        *runtime.committed.lock().unwrap() = Some(vec![first, second]);
        state
            .refresh_committed_events("sesn_peer")
            .await
            .expect("merge peer commit");
        state
            .refresh_committed_events("sesn_peer")
            .await
            .expect("idempotent refresh");

        let events = state
            .list_events("sesn_peer", None, None)
            .expect("read refreshed projection")
            .data;
        let rendered = serde_json::to_string(&events).unwrap();
        assert_eq!(rendered.matches("first").count(), 1, "E1/E2");
        assert_eq!(rendered.matches("second").count(), 1, "E1/E2");
    }

    #[tokio::test]
    async fn protocol_defaults_preparer_rehydrates_the_exact_durable_baseline() {
        // Phase-4 rule M5: an existing durable Session after process restart is
        // not merely "present".  The shared preparer must traverse the same
        // recovery path that reinstalls its frozen Resource and Environment
        // snapshot before a wire adapter may execute.
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let mut persisted = sample_persisted("external-thread");
        persisted.environment_binding = Some("opaque-runtime-binding".to_string());
        create_session_fixture(repo.as_ref(), "workspace-a", persisted).await;

        let runtime = RehydrateFake::default();
        let restored = runtime.restored.clone();
        let restored_environments = runtime.restored_environments.clone();
        let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo);

        restarted
            .prepare_protocol_session("workspace-a", "external-thread", "ignored-on-recovery")
            .await
            .expect("prepare existing protocol thread");

        assert_eq!(restored.lock().unwrap().len(), 1);
        assert_eq!(restored.lock().unwrap()[0].2, sample_inputs());
        assert_eq!(
            restored_environments.lock().unwrap().as_slice(),
            &[(
                "coder".to_string(),
                "external-thread".to_string(),
                "opaque-runtime-binding".to_string(),
            )],
            "the durable agent and opaque Environment binding win over request-time defaults"
        );
    }

    #[tokio::test]
    async fn session_id_mint_namespaces_restart_away_from_repository_truth() {
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, sample_persisted("sesn_0")).await;
        let restarted = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo);
        let created = restarted
            .create_session(bare_create_params(), None)
            .await
            .expect("mint after restart");
        assert_ne!(created.id, "sesn_0");
        assert!(created.id.starts_with("sesn_fnv1a64:"));
    }

    #[tokio::test]
    async fn active_active_session_id_mint_is_collision_free() {
        // Cause/effect graph: each Coordinator owns a distinct process
        // incarnation but both start their local sequence at zero and share one
        // Session repository.
        //
        // | Rule | incarnations | local sequence | shared repo | Effect |
        // |---|---|---|---|---|
        // | S1 | different | both zero | yes | two distinct committed Sessions |
        // | S2 | same process object | increasing | yes | distinct Sessions |
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let left = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
        let right =
            ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo.clone());
        let (left_result, right_result) = tokio::join!(
            left.create_session(bare_create_params(), None),
            right.create_session(bare_create_params(), None),
        );
        let left_session = left_result.expect("S1 left Session");
        let right_session = right_result.expect("S1 right Session");
        assert_ne!(left_session.id, right_session.id, "S1");
        assert!(repo.get(&left_session.id).await.is_some(), "S1");
        assert!(repo.get(&right_session.id).await.is_some(), "S1");

        let next = left
            .create_session(bare_create_params(), None)
            .await
            .expect("S2 next Session");
        assert_ne!(next.id, left_session.id, "S2");
        assert_ne!(next.id, right_session.id, "S2");
    }

    #[tokio::test]
    async fn ensure_session_retries_and_commits_a_crash_interrupted_activation() {
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let mut pending = sample_persisted("sesn_pending");
        let desired = pending.resources.active.clone();
        pending.resources = Default::default();
        pending
            .resources
            .prepare("sesn_pending", desired.clone())
            .unwrap();
        pending.resources.start_attempt().unwrap();
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, pending).await;

        let runtime = RehydrateFake::default();
        let restored = runtime.restored.clone();
        let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo.clone());
        restarted
            .ensure_session("sesn_pending")
            .await
            .expect("recover pending activation");

        assert_eq!(restored.lock().unwrap().len(), 1);
        assert_eq!(restored.lock().unwrap()[0].2, desired);
        let durable = repo.get("sesn_pending").await.unwrap();
        assert!(durable.resources.pending.is_none());
        assert_eq!(durable.resources.active, desired);
        assert_eq!(durable.resources.activations[0].attempts, 2);
        assert_eq!(
            durable.resources.activations[0].state,
            awaken_session_contract::ActivationState::Active
        );
    }

    #[tokio::test]
    async fn resource_reclaimer_finishes_terminal_release_after_restart() {
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let mut deleted = sample_persisted("sesn_deleted");
        deleted.status = "deleted".into();
        deleted.resources.adopt_legacy_active("sesn_deleted");
        deleted.resources.begin_release().unwrap();
        create_session_fixture(repo.as_ref(), "workspace-a", deleted).await;

        let restarted =
            ManagedState::new_with_mcp(RehydrateFake::default()).with_session_repo(repo.clone());
        assert_eq!(restarted.reconcile_resource_activations().await, 1);
        assert!(
            repo.get("sesn_deleted").await.is_none(),
            "D3: a successful retry converges the hidden cleanup row to a tombstone"
        );
        assert!(repo.reconcilable_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn terminal_root_fences_mcp_recovery_before_runtime_effects() {
        // Cause graph: repository index hit + MCP nonterminal + root terminal
        // -> skip realization. The lifecycle classification table lives on
        // PersistedSession; this integration rule proves the scanner consumes it.
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let mut failed = sample_persisted("sesn_failed_mcp");
        failed.status = "activation_failed".into();
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, failed).await;
        assert_eq!(repo.reconcilable_sessions().await.len(), 1, "T1 indexed");

        let runtime = RehydrateFake::default();
        let runtime_effects = runtime.restored_runtimes.clone();
        let restarted = ManagedState::new_with_mcp(runtime).with_session_repo(repo);
        assert_eq!(restarted.reconcile_mcp_attachments().await, 0, "T1 skip");
        assert!(
            runtime_effects.lock().unwrap().is_empty(),
            "T1 terminal root creates no MCP Runtime effect"
        );
    }

    #[tokio::test]
    async fn live_file_attach_and_delete_survive_restart_without_projection_truth() {
        // Cause graph:
        // typed add/update/delete command -> prepare exact next generation
        // -> Runtime applies it -> aggregate commits it -> restart replays only
        // the committed generation. A projection is never persisted as truth.
        //
        // Decision table:
        // | Rule | Command | Runtime | Durable state | Restart behavior |
        // | R1 | add | success | revision +1, one Active | exact id/path restored |
        // | R2 | delete | success | revision +1, no Active | resource absent |
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let catalog = Arc::new(ephemeral_resource_catalog());
        let state = ManagedState::new_with_mcp(RehydrateFake::default())
            .with_session_repo(repo.clone())
            .with_resource_catalog(catalog.clone());
        let id = state
            .create_session(bare_create_params(), None)
            .await
            .expect("create")
            .id;

        let resource = state
            .create_resource(
                &id,
                serde_json::from_value(serde_json::json!({
                    "type": "file",
                    "file_id": "immutable-file-hash",
                    "mount_path": "/input.txt"
                }))
                .unwrap(),
            )
            .await
            .expect("attach");
        let resource_id = resource.id().unwrap().to_string();
        let after_attach = repo.get(&id).await.unwrap();
        assert_eq!(after_attach.resources.revision, 2);
        assert_eq!(
            after_attach
                .resources
                .activations
                .iter()
                .filter(|activation| {
                    activation.state == awaken_session_contract::ActivationState::Active
                })
                .count(),
            1
        );

        let restarted = ManagedState::new_with_mcp(RehydrateFake::default())
            .with_session_repo(repo.clone())
            .with_resource_catalog(catalog.clone());
        restarted.ensure_session(&id).await.expect("rehydrate");
        let restored = restarted.list_resources(&id).expect("list restored");
        assert_eq!(restored.len(), 1);
        assert!(matches!(
            &restored[0],
            crate::types::resource::SessionResource::File { id, mount_path, .. }
                if id == &resource_id && mount_path == "/input.txt"
        ));

        restarted
            .delete_resource(&id, &resource_id)
            .await
            .expect("detach");
        let after_delete = repo.get(&id).await.unwrap();
        assert_eq!(after_delete.resources.revision, 3);
        assert!(
            after_delete
                .resources
                .activations
                .iter()
                .all(|activation| activation.state
                    != awaken_session_contract::ActivationState::Active)
        );
        let second_restart = ManagedState::new_with_mcp(RehydrateFake::default())
            .with_session_repo(repo)
            .with_resource_catalog(catalog);
        second_restart
            .ensure_session(&id)
            .await
            .expect("rehydrate after delete");
        assert!(second_restart.list_resources(&id).unwrap().is_empty());
    }

    /// The only required create field is the agent; every other field defaults.
    fn bare_create_params() -> SessionCreateParams {
        serde_json::from_value(serde_json::json!({ "agent": "assistant" }))
            .expect("minimal create params deserialize")
    }

    #[tokio::test]
    async fn externally_identified_application_session_follows_the_causal_decision_table() {
        // Cause graph: exact external identity + required application contribution
        // admits a preparing Control aggregate. Either absent cause fails before a
        // durable row is authored.
        //
        // | Rule | exact id | application required | Effect |
        // | E1 | non-empty | yes | create under exact id |
        // | E2 | empty | yes | reject, no row |
        // | E3 | non-empty | no | reject, no row |
        // | E4 | same id | same request | replay exact Session |
        // | E5 | same id | different request | reject conflict |
        let state = ManagedState::new_with_mcp(RehydrateFake::default());
        let required: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent": "assistant",
            "application_contribution_required": true
        }))
        .unwrap();
        let created = state
            .create_application_session("flow/run-1", required.clone(), Some("workspace".into()))
            .await
            .expect("E1");
        assert_eq!(created.id, "flow/run-1", "E1 exact identity");
        assert_eq!(created.status, "preparing", "E1 waits for contribution");
        let replayed = state
            .create_application_session("flow/run-1", required.clone(), Some("workspace".into()))
            .await
            .expect("E4");
        assert_eq!(replayed.id, created.id, "E4 exact replay");

        let different: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent": "other-agent",
            "application_contribution_required": true
        }))
        .unwrap();
        assert!(
            state
                .create_application_session("flow/run-1", different, Some("workspace".into()),)
                .await
                .is_err(),
            "E5"
        );

        assert!(
            state
                .create_application_session(" ", required, Some("workspace".into()))
                .await
                .is_err(),
            "E2"
        );
        assert!(
            state
                .create_application_session("flow/run-2", bare_create_params(), None)
                .await
                .is_err(),
            "E3"
        );
        assert!(
            state.sessions_repo.get("flow/run-2").await.is_none(),
            "E3 no row"
        );
    }

    #[tokio::test]
    async fn delete_broadcasts_session_deleted_then_removes_the_record() {
        let state = ManagedState::new_with_mcp(RehydrateFake::default());
        let id = state
            .create_session(bare_create_params(), None)
            .await
            .expect("create")
            .id;

        // Subscribe as an SSE client would, *before* the delete. A fresh session
        // has no committed events, so the stream tails live rather than ending on
        // a terminal backfill — the window in which `session.deleted` is observed.
        let (snapshot, mut rx) = state.stream_subscribe(&id).expect("subscribe");
        assert!(
            snapshot.is_empty(),
            "a fresh session has no committed events"
        );

        state.delete_session(&id).await.expect("delete");

        // The terminal frame reached the open stream before the record was dropped.
        match rx
            .try_recv()
            .expect("a frame was broadcast to the open stream")
        {
            StreamFrame::Committed(e) => assert_eq!(
                e.type_str(),
                "session.deleted",
                "the broadcast terminal frame is session.deleted"
            ),
            other => panic!("expected a committed session.deleted frame, got {other:?}"),
        }

        // And the record is gone: retrieve and events.list are now 404, by design
        // (delete removes the session; it does not tombstone it as archive does).
        assert!(
            matches!(state.get_session(&id), Err(StateError::NotFound)),
            "the deleted session is no longer retrievable"
        );
        assert!(
            matches!(
                state.list_events(&id, None, None),
                Err(StateError::NotFound)
            ),
            "events.list on a deleted session is a 404, not a replay"
        );
    }
}
