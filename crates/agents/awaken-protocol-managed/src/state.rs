//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; a tool-use event keeps
//! the tool call's own id so a `user.tool_confirmation` can reference it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_credential_vault::CredentialSourceId;

use crate::dto::{
    ConfirmResult, CreateSessionRequest, Event, EventReceipt, InboundEvent, ListEventsResponse,
    OutboundKind, SendEventsRequest, SendEventsResponse, Session, SessionAgent, StopReason,
};
use crate::project::{self, project_messages, project_turn};
use crate::vaults::{McpRefreshBinding, VaultState};

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The tool a run parked on: its id, model-visible name/input, and whether it is
/// client-executed (projected as `agent.custom_tool_use`) or a built-in awaiting
/// confirmation (`agent.tool_use{ask}`).
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The result of running one step (a new turn, or a resume). `pending` is set when
/// `stop` is `RequiresAction`.
pub struct TurnOutcome {
    pub messages: Vec<Message>,
    pub stop: StopReason,
    pub pending: Option<Pending>,
}

/// A human-in-the-loop tool decision, delivered by `user.tool_confirmation`.
pub struct Decision {
    pub allow: bool,
    pub note: Option<String>,
}

/// The advertised capability surface echoed in a session's agent object. The adapter
/// reads this once at session creation so the public agent object reports what the run
/// can actually do. This is neutral data; the public Managed Agents wire shaping (the
/// built-in `agent_toolset` fold, `custom` tools, `skills`, `multiagent`) lives in
/// [`crate::project`]. Deliberately absent: MCP servers (the host wires none) and
/// session resources (the host has no Files-API-backed resource to reference yet), so
/// those wire fields stay empty until a real producer exists.
#[derive(Default)]
pub struct AgentCapabilities {
    /// The registered built-in tools (the hand toolset). Each names a tool of the
    /// versioned agent toolset and whether its calls require human confirmation.
    pub builtin_tools: Vec<BuiltinTool>,
    /// Client-executed tools: the caller runs them and returns the result.
    pub custom_tools: Vec<CustomTool>,
    /// Skills the agent offers (activated on demand, not model-visible as tools).
    pub skills: Vec<String>,
    /// Delegate agents the agent may coordinate (the multiagent roster).
    pub delegates: Vec<String>,
}

/// One registered built-in tool: its name and whether calls require confirmation
/// (`ask` = the permission gate parks the call for an approval).
pub struct BuiltinTool {
    pub name: String,
    pub ask: bool,
}

/// One client-executed custom tool: the model-visible name, description, and input
/// schema the runtime pins for it.
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One evaluation round of a goal: the agent's revision messages committed this
/// round (empty when grading the existing deliverable), and the verdict.
pub struct OutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The result of `user.define_outcome`: the ordered evaluation rounds. The loop
/// always ends idle (`end_turn`).
pub struct OutcomeReport {
    pub iterations: Vec<OutcomeIteration>,
}

/// What a new session provisions on its thread before the first turn (ADR-0043
/// Phase 3): the agent it runs and the MCP servers it connects to, each already
/// bound to a vault credential's neutral domain id (or none). Consumed by the
/// server's `ManagedHost` through [`SessionRuntime::prepare_session`].
pub struct SessionInit {
    pub agent_id: String,
    pub mcp_servers: Vec<McpServerBinding>,
    /// The consumption-side project the session arrived through
    /// (`/projects/{id}/v1/sessions`), stamped by the ingress middleware.
    /// `None` = the bare workspace-default surface — byte-identical behavior
    /// to before projects existed.
    pub project_id: Option<String>,
    /// The session's requested model (R2), staged so the run binds it; `None` →
    /// the host default.
    pub model: Option<String>,
    /// The session's requested runtime adapter (R3): `"acp:*"` routes to an ACP
    /// CLI; `None`/`"awaken"` → native.
    pub runtime: Option<String>,
}

/// One session MCP server, bound at creation: the wire name/url plus the vault
/// credential the URL matched (`None` when no vault credential matches — the
/// host then connects unauthenticated and the server decides). Consumed by
/// `ManagedHost::prepare_session` in the server assembly.
pub struct McpServerBinding {
    pub name: String,
    pub url: String,
    pub credential_source_id: Option<CredentialSourceId>,
    /// The matched credential's stored refresh configuration
    /// ([`VaultState::mcp_refresh_for_source`]), so the host can register a
    /// transport-level refresher next to the bearer. `None` when the credential
    /// is not refreshable (entered without a refresh object).
    pub refresh: Option<McpRefreshBinding>,
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Run one user turn on `thread` to its first pause or end. `content` is the
    /// user message's full block list (multimodal): text interleaved with any
    /// image blocks, never flattened to a bare string.
    async fn run_turn(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError>;

    /// Answer a built-in tool the run parked on (allow/deny) and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending built-in tool.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: Decision,
    ) -> Result<TurnOutcome, RunError>;

    /// Deliver a client-executed tool's result to the parked run and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending client-executed tool.
    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<TurnOutcome, RunError>;

    /// Provision `thread` for a new session BEFORE its record exists (ADR-0043
    /// Phase 3): the host materializes the init's MCP credential bindings and
    /// stages the servers for the thread's first turn. A failure fails the
    /// create (fail closed). The default is a no-op, so every host without MCP
    /// wiring is unaffected.
    async fn prepare_session(&self, _thread: &str, _init: SessionInit) -> Result<(), RunError> {
        Ok(())
    }

    /// Rebind `thread` to `model` for its subsequent turns (R5, per-turn override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next turn resolves the new executor.
    async fn rebind_model(&self, _thread: &str, _model: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, _thread: &str) -> bool {
        false
    }

    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    /// Buffer a system message; it is prepended to the next turn's input.
    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError>;

    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded by
    /// `max_iterations`; `rubric` is the normalized requirement text.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError>;

    /// The model id to echo in the session's agent object.
    fn model(&self) -> String;

    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }
}

/// A runtime failure. `kind` classifies who is at fault so the router can map it
/// to the right HTTP status: a `BadRequest` is the caller's (an unknown park, a
/// mismatched id, a wrong-binding resume); `Internal` is the runtime's.
#[derive(Debug, thiserror::Error)]
#[error("run failed: {message}")]
pub struct RunError {
    pub message: String,
    pub kind: RunErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunErrorKind {
    Internal,
    BadRequest,
}

impl RunError {
    /// A runtime-side failure (provider error, corrupt state) — maps to `500`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
        }
    }

    /// A caller-side failure (bad id, wrong binding, no park) — maps to `400`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
        }
    }
}

struct SessionRecord {
    agent_id: String,
    session: Session,
    events: Vec<Event>,
}

/// The adapter's in-memory session store plus the runtime port.
pub struct ManagedState {
    runtime: Box<dyn SessionRuntime>,
    /// The vault surface, when the server mounts one (ADR-0043 Phase 3): a
    /// session's `mcp_servers` are bound to vault credentials through it at
    /// creation. `None` means every binding resolves to no credential.
    vaults: Option<Arc<VaultState>>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    session_seq: AtomicU64,
    event_seq: AtomicU64,
}

/// Why a session operation failed (mapped to an HTTP status by the router).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("session not found")]
    NotFound,
    /// A session create named a vault that does not exist (`vault_ids`); the
    /// router maps it to the standard 404 envelope naming the vault id.
    #[error("vault `{0}` not found")]
    VaultNotFound(String),
    #[error(transparent)]
    Run(#[from] RunError),
}

impl ManagedState {
    pub fn new(runtime: impl SessionRuntime + 'static) -> Self {
        Self {
            runtime: Box::new(runtime),
            vaults: None,
            sessions: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(0),
            event_seq: AtomicU64::new(0),
        }
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

    fn next_event_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    /// `POST /v1/sessions`.
    ///
    /// MCP binding (ADR-0043 Phase 3): each requested server is bound to a vault
    /// credential by exact `mcp_server_url` match across the request's
    /// `vault_ids`, then the runtime provisions the thread via
    /// [`SessionRuntime::prepare_session`] BEFORE the record is inserted — a
    /// failed preparation fails the create (fail closed; the router maps the
    /// `RunError` to the error envelope). A `vault_ids` entry that names no
    /// existing vault fails the create closed too ([`VaultState::has_vault`]):
    /// a 404 naming the vault id, BEFORE anything is provisioned — never a
    /// silent no-binding whose 401 only surfaces at the first turn. (Without a
    /// wired vault surface there is nothing to validate against and every
    /// binding resolves to no credential, as before.)
    pub async fn create_session(
        &self,
        req: CreateSessionRequest,
        project_id: Option<String>,
    ) -> Result<Session, StateError> {
        if let Some(vaults) = &self.vaults
            && let Some(unknown) = req.vault_ids.iter().find(|v| !vaults.has_vault(v))
        {
            return Err(StateError::VaultNotFound(unknown.clone()));
        }
        // Mint an id no durable thread already owns: a fresh process restarts
        // the sequence at 0, but the store dir may hold committed truth from a
        // previous process (ADR-0039). Adopting such a thread would graft the
        // old transcript onto a NEW session, so skip forward instead — the
        // rehydration path (`ensure_session`) remains the only way to reattach
        // to an existing thread, and it is keyed by the caller's explicit id.
        let id = loop {
            let candidate = format!("sesn_{}", self.session_seq.fetch_add(1, Ordering::SeqCst));
            if !self.runtime.owns_thread(&candidate).await {
                break candidate;
            }
        };
        let agent_id = req.agent.id().to_string();
        let bindings = req
            .mcp_servers
            .iter()
            .map(|server| {
                let credential_source_id = self
                    .vaults
                    .as_ref()
                    .and_then(|v| v.mcp_credential_source_for_url(&req.vault_ids, &server.url));
                // The matched credential's stored refresh configuration rides
                // along, so the host can keep the connection alive past the
                // access token's expiry.
                let refresh = match (&self.vaults, &credential_source_id) {
                    (Some(v), Some(source_id)) => v.mcp_refresh_for_source(source_id),
                    _ => None,
                };
                McpServerBinding {
                    name: server.name.clone(),
                    url: server.url.clone(),
                    credential_source_id,
                    refresh,
                }
            })
            .collect();
        self.runtime
            .prepare_session(
                &id,
                SessionInit {
                    agent_id: agent_id.clone(),
                    mcp_servers: bindings,
                    project_id,
                    model: req.agent.model().map(str::to_string),
                    runtime: req.agent.runtime().map(str::to_string),
                },
            )
            .await
            .map_err(StateError::Run)?;
        // Enumerate the runtime's provisioned surface so the agent object reports what
        // the run can actually do (built-in toolset, custom tools, skills, delegates),
        // not an empty set. The wire shaping lives in `project`; the host supplies
        // neutral data.
        let caps = self.runtime.capabilities();
        let session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                // R6: echo the session's actual model — the requested override, else
                // the host default — so the client sees which model the session runs.
                model: req
                    .agent
                    .model()
                    .map(str::to_string)
                    .unwrap_or_else(|| self.runtime.model()),
                name: agent_id.clone(),
                tools: project::agent_tools(&caps),
                // Echo the accepted servers in the SDK's `{name, type:"url", url}` shape.
                mcp_servers: req
                    .mcp_servers
                    .iter()
                    .map(|s| serde_json::to_value(s).expect("mcp server wire serializes"))
                    .collect(),
                skills: project::agent_skills(&caps),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id: req
                .environment_id
                .unwrap_or_else(|| "env_local".to_string()),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            // The host has no Files-API-backed resource to reference on the wire yet.
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: "idle",
        };
        self.sessions.lock().unwrap().insert(
            id,
            SessionRecord {
                agent_id,
                session: session.clone(),
                events: Vec::new(),
            },
        );
        Ok(session)
    }

    /// A session object reconstructed for a rehydrated (post-restart) session. It
    /// reuses the runtime's advertised surface; environment/title/metadata default
    /// because the original create request is no longer available (its MCP servers
    /// among them, so `mcp_servers` reads empty after a restart).
    fn rehydrated_session(&self, id: &str) -> Session {
        let caps = self.runtime.capabilities();
        let agent_id = "assistant".to_string();
        Session {
            id: id.to_string(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                model: self.runtime.model(),
                name: agent_id,
                tools: project::agent_tools(&caps),
                mcp_servers: Vec::new(),
                skills: project::agent_skills(&caps),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id: "env_local".to_string(),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: None,
            metadata: Default::default(),
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: "idle",
        }
    }

    /// Recover a session whose in-memory record was lost from durable truth (a
    /// process restart, ADR-0039). If the store holds a committed transcript for
    /// `id`, rebuild the record — the projected history plus a reconstructed
    /// session object — so a resume can continue the parked run. A thread with no
    /// committed truth stays `NotFound` (fail closed): the store is authoritative.
    async fn ensure_session(&self, id: &str) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        let messages = self.runtime.committed_messages(id).await;
        if messages.is_empty() {
            return Err(StateError::NotFound);
        }
        let events: Vec<Event> = project_messages(&messages, None)
            .into_iter()
            .map(|event| Event {
                id: event.id.unwrap_or_else(|| self.next_event_id()),
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            })
            .collect();
        let record = SessionRecord {
            agent_id: "assistant".to_string(),
            session: self.rehydrated_session(id),
            events,
        };
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
        Ok(())
    }

    /// `GET /v1/sessions/{id}`.
    pub fn get_session(&self, id: &str) -> Result<Session, StateError> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(id)
            .map(|r| r.session.clone())
            .ok_or(StateError::NotFound)
    }

    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    fn append_turn(&self, session_id: &str, outcome: TurnOutcome) -> Result<(), StateError> {
        let pending = outcome
            .pending
            .as_ref()
            .map(|p| (p.tool_use_id.as_str(), p.client_executed));
        let projected = project_turn(&outcome.messages, outcome.stop, pending);
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        for event in projected {
            let id = event.id.unwrap_or_else(|| self.next_event_id());
            record.events.push(Event {
                id,
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        Ok(())
    }

    /// Append an outcome report: for each round, the agent's revision events then
    /// `span.outcome_evaluation_start` / `_end`, and a terminal `session.status_idle`.
    fn append_outcome(&self, session_id: &str, report: OutcomeReport) -> Result<(), StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        // Each round's durable evaluation record, collected as we project its events
        // and folded into the session object after the event-pushing borrow releases.
        let mut evaluations: Vec<serde_json::Value> = Vec::new();
        {
            let mut push = |id: Option<String>, kind: OutboundKind| {
                record.events.push(Event {
                    id: id.unwrap_or_else(|| {
                        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
                    }),
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            };
            for round in report.iterations {
                for event in project_messages(&round.messages, None) {
                    push(event.id, event.kind);
                }
                evaluations.push(project::outcome_evaluation(&round));
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationStart {
                        outcome_id: round.outcome_id.clone(),
                        iteration: round.iteration,
                    },
                );
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationEnd {
                        outcome_id: round.outcome_id,
                        iteration: round.iteration,
                        result: round.result,
                        explanation: round.explanation,
                    },
                );
            }
            push(
                None,
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::EndTurn,
                },
            );
        }
        // The session object carries the running list of evaluations that have graded
        // it, so a `GET /v1/sessions/{id}` reflects the outcomes that ran, not [].
        record.session.outcome_evaluations.extend(evaluations);
        Ok(())
    }

    /// `POST /v1/sessions/{id}/events`. Mints a receipt per inbound event and acts
    /// on `user.message` (run a turn) and `user.tool_confirmation` (resume a
    /// parked run), appending the projected events.
    pub async fn send_events(
        &self,
        session_id: &str,
        req: SendEventsRequest,
    ) -> Result<SendEventsResponse, StateError> {
        // Recover the session from durable truth if its in-memory record was lost
        // (a process restart) before resolving the agent — so a resume continues
        // the parked run instead of failing closed (ADR-0039).
        self.ensure_session(session_id).await?;
        let agent_id = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(session_id)
                .ok_or(StateError::NotFound)?
                .agent_id
                .clone()
        };

        let mut receipts = Vec::new();
        for inbound in &req.events {
            receipts.push(EventReceipt {
                id: self.next_event_id(),
                kind: inbound.type_str(),
                processed_at: None,
            });

            match inbound {
                InboundEvent::UserMessage { content, model, .. } => {
                    // R5: a per-turn model override rebinds the thread before the turn.
                    if let Some(model) = model {
                        self.runtime.rebind_model(session_id, model).await?;
                    }
                    let outcome = self
                        .runtime
                        .run_turn(&agent_id, session_id, content.clone())
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserToolConfirmation {
                    tool_use_id,
                    result,
                    deny_message,
                } => {
                    let decision = Decision {
                        allow: matches!(result, ConfirmResult::Allow),
                        note: deny_message.clone(),
                    };
                    let outcome = self
                        .runtime
                        .resume(session_id, tool_use_id, decision)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserCustomToolResult {
                    custom_tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = content.as_deref().map(content_text).unwrap_or_default();
                    let outcome = self
                        .runtime
                        .resume_custom(session_id, custom_tool_use_id, &text, *is_error)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserDefineOutcome {
                    description,
                    rubric,
                    max_iterations,
                } => {
                    let rubric = rubric_text(rubric);
                    let report = self
                        .runtime
                        .define_outcome(
                            session_id,
                            description,
                            &rubric,
                            max_iterations.unwrap_or(3),
                        )
                        .await?;
                    self.append_outcome(session_id, report)?;
                }
                InboundEvent::SystemMessage { content } => {
                    let text = content_text(content);
                    self.runtime.add_system(session_id, &text).await?;
                }
                // `user.interrupt`: cancel the run in flight on this thread (from a
                // concurrent request), so an in-progress outcome ends `interrupted`.
                InboundEvent::UserInterrupt { .. } => {
                    self.runtime.interrupt(session_id).await?;
                }
                // `user.pause`, `user.resume`: accept-only; the receipt is the
                // acknowledgement.
                _ => {}
            }
        }
        Ok(SendEventsResponse { data: receipts })
    }

    /// `GET /v1/sessions/{id}/events`.
    pub fn list_events(&self, session_id: &str) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(ListEventsResponse {
            data: record.events.clone(),
            next_page: None,
            has_more: false,
        })
    }

    /// `GET /v1/sessions/{id}/events/stream` — the events to replay as SSE.
    pub fn stream_events(&self, session_id: &str) -> Result<Vec<Event>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(record.events.clone())
    }
}

/// Normalize a Managed rubric (a bare string or `{type:"text",content}`) to text.
fn rubric_text(rubric: &serde_json::Value) -> String {
    match rubric {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Concatenate the text of a content-block list.
fn content_text(content: &[awaken_agent_contract::agent::content::ContentBlock]) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
