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

use crate::ext::AwakenModelSelection;
use crate::preview::PreviewSink;
use crate::project::{self, project_messages, project_step};
use crate::routes::vaults::VaultState;
use crate::types::{
    ConfirmResult, Event, EventReceipt, InboundEvent, ListEventsResponse, ModelConfig,
    ModelOverride, OutboundKind, SendEventsRequest, SendEventsResponse, Session, SessionAgent,
    SessionCreateParams, SessionError, SessionStats, StopReason, StreamFrame, Usage,
};
use awaken_session_contract::{ManagedSessionRepository, PersistedSession, SessionLifecycleFact};
use awaken_session_store::SqliteManagedSessionRepository;

/// The seeded owner scope a bare/self-hosted session is created under when the
/// edge resolved no workspace (ADR-0051 / ADR-0048 D2 "seeded, not absent"). It
/// matches the request scope the ownership guard derives for an unscoped request,
/// so a single-tenant deployment never 404s itself.
pub(crate) const DEFAULT_SCOPE: &str = "default";

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The Managed Agents contract error for a `memory_store` add/remove on a running
/// session — memory stores bind at session creation only.
const MEMORY_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; \
     adding or removing one from a running session is not supported";

mod application;
mod environment;
mod events;
mod helpers;
mod realization;
mod resource;
mod resources;
mod session_update;
pub(crate) use session_update::SessionUpdateCommand;
mod sessions;
mod threads;
mod types;

pub(crate) use helpers::{content_text, lifecycle_fact, rubric_text, session_usage_value};
pub(crate) use resource::{
    ParsedInputTarget, ParsedSessionInput, input_binding, parse_session_input,
    resolved_resource_dto, resource_binding_id,
};
pub use types::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, OutcomeIteration, OutcomeReport, Pending, RunError, RunErrorKind,
    SessionInit, SessionRuntime, SessionUsage, StepOutcome, ToolPermissionDecision,
};

struct SessionRecord {
    agent_id: String,
    session: Session,
    /// Durable source of truth for the runtime's currently applied input projection.
    resource_state: awaken_session_contract::SessionResourceState,
    events: Vec<Event>,
    /// Subagent (multiagent delegate) child threads spawned in the session, each a
    /// projected `session_thread` object (parent = the primary thread). Enumerated
    /// by `list_threads`/`get_thread`; each is announced by a `session.thread_created`
    /// event (ADR-0047 D4, first slice).
    child_threads: Vec<serde_json::Value>,
}

impl SessionRecord {
    /// Project the HTTP Session DTO from the typed aggregate state. The stored
    /// `Session` intentionally keeps `resources` empty so JSON can never become
    /// a second mutable resource index.
    fn session_projection(&self) -> Session {
        let mut session = self.session.clone();
        session.resources = self
            .resource_state
            .active
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(&session.id, input))
            .collect();
        session
    }
}

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
    vaults: Option<Arc<VaultState>>,
    /// The environments surface, when the server mounts one: a session's
    /// `environment_id` is resolved to its networking policy (egress on/off) at
    /// creation. `None` → every session gets host network (unrestricted).
    environments: Option<Arc<crate::routes::environments::EnvironmentState>>,
    /// The config-plane agent projection source (ADR-0043): when wired, a session
    /// referencing an agent published on the config plane inherits that agent's
    /// authoritative `model` (the config plane owns model/system/tools), so it runs
    /// the agent's model instead of the host default. Reuses the same
    /// [`crate::routes::agents_registry::AgentConfigSource`] port `/v1/agents` reads —
    /// no second source of agent truth. `None` → fall back to the host default model.
    config_source: Option<Arc<dyn crate::routes::agents_registry::AgentConfigSource>>,
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

/// The webhook lifecycle-fact catalog projected through [`SessionLifecycleSink`],
/// named to match Anthropic's official Managed Agents webhook event set. These are
/// a distinct vocabulary from the in-session SSE `OutboundKind` stream names: a
/// webhook consumer's `data.type` carries these past-tense *fact* names (the SSE
/// stream carries the present-tense transition names). Keeping them here — one
/// place, owned by the projecting crate — is why the wire crate needs no dependency
/// on the webhook delivery machinery: it emits fact names, the sink maps them.
///
/// Wired to the sink today: [`SESSION_IDLED`] (on create), [`SESSION_TERMINATED`]
/// (on archive), and [`SESSION_DELETED`] (on delete) — the session-level transitions
/// a webhook consumer acts on.
///
/// The rest of Anthropic's catalog is projected onto the SSE stream but not yet
/// fanned to webhooks, and maps to existing `OutboundKind` events: `session.status_
/// run_started` (`SessionStatusRunning`), `session.thread_created` (`SessionThread
/// Created`, whose webhook payload would carry `session_thread_id`), and `session.
/// outcome_evaluation_ended` (`SpanOutcomeEvaluationEnd`). Wiring one is additive —
/// add its const here and one `sink.emit` at the projection point — not a rename.
pub mod lifecycle_event {
    /// Session created, or a turn settled — now idle. Anthropic `session.status_idled`.
    pub const SESSION_IDLED: &str = "session.status_idled";
    /// Session terminated (archived). Anthropic `session.status_terminated`.
    pub const SESSION_TERMINATED: &str = "session.status_terminated";
    /// Session deleted (record dropped, not tombstoned). Matches the SSE
    /// terminal transition name `session.deleted` — the delete edge carries no
    /// status, so the fact is the past-tense event, not a `status_*` name.
    pub const SESSION_DELETED: &str = "session.deleted";
}

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
        Self {
            runtime,
            mcp_realizer,
            vaults: None,
            environments: None,
            config_source: None,
            resource_catalog: None,
            resource_purge_scheduler: None,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            sessions_repo: Arc::new(
                SqliteManagedSessionRepository::open_in_memory()
                    .expect("open ephemeral managed Session repository"),
            ),
            lifecycle_sink: None,
            runtime_incarnation: format!("managed:{}:{started_at}", std::process::id()),
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
    /// `EnvironmentState` with [`crate::environments_router`], or the sessions and the
    /// environment routes see different environments.
    #[must_use]
    pub fn with_environments(
        mut self,
        environments: Arc<crate::routes::environments::EnvironmentState>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }

    /// Wire a durable session repository (e.g. SQLite alongside the transcript
    /// store) so a session's config survives a restart and is reported faithfully
    /// by another process. The default is in-memory (single-process behavior).
    #[must_use]
    pub fn with_session_repo(mut self, repo: Arc<dyn ManagedSessionRepository>) -> Self {
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
        self.vaults = Some(vaults);
        self
    }

    /// Wire the config-plane agent projection source so a session inherits a
    /// published agent's authoritative `model`. Share the same
    /// [`crate::routes::agents_registry::AgentConfigSource`] that `/v1/agents` uses,
    /// or the session and the agent view disagree on the model.
    #[must_use]
    pub fn with_config_source(
        mut self,
        source: Arc<dyn crate::routes::agents_registry::AgentConfigSource>,
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

/// Private fail-closed null adapter for applications that do not enable MCP
/// attachment commands. It is composition policy, not part of the public port.
struct UnsupportedMcpAttachmentRealizer;

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for UnsupportedMcpAttachmentRealizer {}

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

    fn ephemeral_resource_catalog() -> awaken_admin_config_api::SqliteAdminStore {
        awaken_admin_config_api::SqliteAdminStore::open_in_memory()
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
        order: Arc<std::sync::Mutex<Vec<&'static str>>>,
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
            _content: &str,
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
            vec![Message::text(
                awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )]
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
            _content: &str,
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
        fn model(&self) -> String {
            "host-default-model".to_string()
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
            _content: &str,
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
            revision: awaken_session_contract::env_registry::EnvironmentRevision(1),
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-1".into()),
            sandbox: serde_json::json!({"isolation": "namespace"}),
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
                        mounts: Vec::new(),
                        env: Vec::new(),
                        prompts: Vec::new(),
                    },
                ),
            ),
            title: Some("My session".to_string()),
            metadata,
            agent_tools: None,
            environment_binding: None,
            mcp,
            resources: awaken_session_contract::SessionResourceState::from_legacy(sample_inputs()),
            realization: None,
            status: "idle".into(),
            archived_at: None,
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
        // Cause graph: durable mutable tools present -> use exact replacement;
        // absent legacy field -> derive Runtime defaults. This test is T1; the
        // following fallback test is T2.
        //
        // | Rule | Persisted tools | Projection |
        // |---|---|---|
        // | T1 | Some(including empty) | exact durable value |
        // | T2 | None | Runtime default |
        let state = ManagedState::new_with_mcp(RehydrateFake::default());
        let mut persisted = sample_persisted("sesn_1");
        persisted.agent_tools = Some(vec![serde_json::json!({"name": "durable-tool"})]);
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
    fn rehydrated_session_rejects_corrupt_tools_without_capability_downgrade() {
        // Causal graph:
        // durable tool projection is present but invalid
        //   -> typed rehydration fails
        //   -> Runtime defaults are not substituted
        //   -> caller cannot cache or expose a weaker Session.
        //
        // Decision table:
        // | Durable field | Shape | Expected behavior |
        // | absent | n/a | use Runtime defaults (legacy compatibility) |
        // | present | valid typed/legacy tool | restore exact tool |
        // | present | invalid | stable projection error; no fallback |
        let state = ManagedState::new_with_mcp(RehydrateFake::default());
        let mut persisted = sample_persisted("sesn_corrupt");
        persisted.agent_tools = Some(vec![serde_json::json!({"unexpected": true})]);

        let error = state
            .rehydrated_session("sesn_corrupt", Some(persisted))
            .expect_err("corrupt durable capabilities must fail closed");

        assert!(
            error
                .to_string()
                .contains("persisted_session_projection_invalid: agent.tools[0]"),
            "the recovery boundary returns a stable, indexed reason: {error}"
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
        assert_eq!(session.resources[0]["file_id"], "file-hash");
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
            &["resources", "runtime", "environment", "history"],
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
    async fn session_id_mint_skips_repository_truth_after_restart() {
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, sample_persisted("sesn_0")).await;
        let restarted = ManagedState::new(EndSessionRecorder::default()).with_session_repo(repo);
        let created = restarted
            .create_session(bare_create_params(), None)
            .await
            .expect("mint after restart");
        assert_eq!(created.id, "sesn_1");
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
    async fn live_input_mutations_survive_restart_without_changing_resource_identity() {
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
                serde_json::json!({
                    "type": "file",
                    "file_id": "immutable-file-hash",
                    "mount_path": "/input.txt"
                }),
            )
            .await
            .expect("attach");
        let resource_id = resource["id"].as_str().unwrap().to_string();
        state
            .update_resource(
                &id,
                &resource_id,
                serde_json::json!({"mount_path": "/renamed/input.txt"}),
            )
            .await
            .expect("update");
        let after_update = repo.get(&id).await.unwrap();
        assert_eq!(after_update.resources.revision, 3);
        assert_eq!(
            after_update
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
        assert_eq!(restored[0]["id"], resource_id);
        assert_eq!(restored[0]["mount_path"], "/renamed/input.txt");

        restarted
            .delete_resource(&id, &resource_id)
            .await
            .expect("detach");
        let after_delete = repo.get(&id).await.unwrap();
        assert_eq!(after_delete.resources.revision, 4);
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
