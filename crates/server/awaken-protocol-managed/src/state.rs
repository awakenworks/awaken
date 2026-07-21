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
use awaken_session_store::InMemorySessionRepository;

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

mod events;
mod helpers;
mod resource;
mod resources;
mod sessions;
mod threads;
mod types;

pub(crate) use helpers::{content_text, rubric_text, session_usage_value};
pub use resource::{ResourceAccess, SessionResource};
pub(crate) use resource::{
    input_binding, parse_session_resource, resolved_resource_dto, resource_dto,
};
pub use types::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, McpServerBinding, OutcomeIteration, OutcomeReport, Pending, RunError,
    RunErrorKind, SessionInit, SessionRuntime, SessionUsage, StepOutcome, ToolPermissionDecision,
};

struct SessionRecord {
    agent_id: String,
    session: Session,
    events: Vec<Event>,
    /// Subagent (multiagent delegate) child threads spawned in the session, each a
    /// projected `session_thread` object (parent = the primary thread). Enumerated
    /// by `list_threads`/`get_thread`; each is announced by a `session.thread_created`
    /// event (ADR-0047 D4, first slice).
    child_threads: Vec<serde_json::Value>,
}

/// The adapter's in-memory session store plus the runtime port.
pub struct ManagedState {
    runtime: Box<dyn SessionRuntime>,
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
        Self {
            runtime: Box::new(runtime),
            vaults: None,
            environments: None,
            config_source: None,
            resource_catalog: None,
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            sessions_repo: Arc::new(InMemorySessionRepository::default()),
            lifecycle_sink: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::Message;
    use std::collections::BTreeMap;

    /// A runtime that reports a non-empty committed transcript, so a session can
    /// rehydrate. Every operational method is unused by these tests.
    #[derive(Clone, Default)]
    struct RehydrateFake {
        restored: Arc<
            std::sync::Mutex<
                Vec<(
                    String,
                    String,
                    awaken_session_contract::EffectiveSessionInputs,
                )>,
            >,
        >,
    }

    #[async_trait]
    impl SessionRuntime for RehydrateFake {
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
            vec![Message::text(
                awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )]
        }
        fn model(&self) -> String {
            "host-default-model".to_string()
        }
        async fn restore_session_inputs(
            &self,
            thread: &str,
            workspace_id: &str,
            inputs: &awaken_session_contract::EffectiveSessionInputs,
        ) -> Result<(), RunError> {
            self.restored.lock().unwrap().push((
                thread.to_string(),
                workspace_id.to_string(),
                inputs.clone(),
            ));
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

    /// `DELETE /v1/sessions/{id}` reaches the host's terminal sandbox disposal
    /// (`end_session`) for the session's main thread — the wiring that stops a
    /// deleted session's sandbox from leaking.
    #[tokio::test]
    async fn delete_session_disposes_the_host_sandbox() {
        let rt = EndSessionRecorder::default();
        let ended = rt.ended.clone();
        let state = ManagedState::new(rt);
        let id = state
            .create_session(bare_create_params(), None)
            .await
            .expect("create")
            .id;
        state.delete_session(&id).await.expect("delete");
        assert_eq!(
            *ended.lock().unwrap(),
            vec![id],
            "delete tears down the session's sandbox via end_session"
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

    /// A sandbox teardown failure at delete is swallowed (best-effort): the delete is
    /// terminal, so the session is still removed and reads 404 afterwards — a dispose
    /// error must never leave a "deleted" session alive.
    #[tokio::test]
    async fn delete_is_best_effort_when_sandbox_teardown_fails() {
        let state = ManagedState::new(EndSessionFailer);
        let id = state
            .create_session(bare_create_params(), None)
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
    }

    fn sample_persisted(id: &str) -> PersistedSession {
        let mut metadata = BTreeMap::new();
        metadata.insert("team".to_string(), "research".to_string());
        PersistedSession {
            session_id: id.to_string(),
            agent_id: "coder".to_string(),
            model: "kimi-k2".to_string(),
            title: Some("My session".to_string()),
            metadata,
            environment_id: "env_local".to_string(),
            mcp_servers: vec![
                serde_json::json!({"name": "calc", "type": "url", "url": "https://x"}),
            ],
            effective_inputs: sample_inputs(),
            status: "idle".into(),
            archived_at: None,
        }
    }

    fn sample_inputs() -> awaken_session_contract::EffectiveSessionInputs {
        awaken_session_contract::EffectiveSessionInputs {
            inputs: vec![awaken_session_contract::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::from("input-file"),
                source: awaken_session_contract::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file-hash"),
                },
                mount_path: "/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
        }
    }

    #[test]
    fn rehydrated_session_restores_persisted_config() {
        let state = ManagedState::new(RehydrateFake::default());
        let session = state.rehydrated_session("sesn_1", Some(sample_persisted("sesn_1")));
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
        assert_eq!(session.resources.len(), 1);
        assert_eq!(session.resources[0]["file_id"], "file-hash");
    }

    #[test]
    fn rehydrated_session_falls_back_without_persisted_config() {
        let state = ManagedState::new(RehydrateFake::default());
        let session = state.rehydrated_session("sesn_1", None);
        assert_eq!(session.agent.id, "assistant");
        assert_eq!(session.agent.model.id, "host-default-model");
        assert!(session.title.is_none());
        assert!(session.agent.mcp_servers.is_empty());
    }

    #[tokio::test]
    async fn ensure_session_rehydrates_from_repo_after_cache_loss() {
        // A session created in one process is gone from a fresh process's cache,
        // but the shared repo + committed transcript restore it faithfully.
        let repo: Arc<dyn ManagedSessionRepository> =
            Arc::new(InMemorySessionRepository::default());
        repo.save(sample_persisted("sesn_1")).await;

        // Fresh state (empty cache) sharing the durable repo — simulates a restart.
        let runtime = RehydrateFake::default();
        let restored = runtime.restored.clone();
        let restarted = ManagedState::new(runtime).with_session_repo(repo);
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
        assert_eq!(
            restored.lock().unwrap().as_slice(),
            &[(
                "sesn_1".to_string(),
                DEFAULT_SCOPE.to_string(),
                sample_inputs(),
            )],
            "restart replays the persisted manifest once without re-resolving it"
        );
    }

    /// The only required create field is the agent; every other field defaults.
    fn bare_create_params() -> SessionCreateParams {
        serde_json::from_value(serde_json::json!({ "agent": "assistant" }))
            .expect("minimal create params deserialize")
    }

    #[tokio::test]
    async fn delete_broadcasts_session_deleted_then_removes_the_record() {
        let state = ManagedState::new(RehydrateFake::default());
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
