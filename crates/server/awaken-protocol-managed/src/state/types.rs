//! Neutral value types and the [`SessionRuntime`] port the adapter drives:
//! pending tools, turn outcomes, capabilities, session init, and run errors.

use super::*;

/// The tool a run parked on: its id, model-visible name/input, and whether it is
/// client-executed (projected as `agent.custom_tool_use`) or a built-in awaiting
/// confirmation (`agent.tool_use{ask}`).
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The neutral terminus of a step — the runtime's outcome, free of any wire
/// vocabulary (the managed adapter maps this + `pending` to the wire `StopReason`
/// at projection time). `Parked` carries no ids here; the pending tool supplies
/// them. Keeping the port neutral lets it live in a protocol-agnostic contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminus {
    /// A natural end of turn (→ wire `end_turn`).
    End,
    /// The run parked awaiting a decision on `pending` (→ wire `requires_action`).
    Parked,
    /// The run gave up after exhausting its retries/steps (→ wire `retries_exhausted`).
    Exhausted,
}

/// The result of running one step (a new turn, or a resume). `pending` is set when
/// `stop` is `Parked`.
pub struct StepOutcome {
    pub messages: Vec<Message>,
    pub stop: Terminus,
    pub pending: Option<Pending>,
    /// `true` when this turn folded its context — projected as an
    /// `agent.thread_context_compacted` event ahead of the turn's messages.
    pub compacted: bool,
    /// `true` when the runtime transparently retried a transient inference failure
    /// during this turn (auto-recovery) — projected as a `session.status_rescheduled`
    /// event ahead of the turn's messages, so a client observes the recovery.
    pub rescheduled: bool,
    /// Set when the run ended in a terminal fault (the neutral `EndCause::Error`) —
    /// projected as a `session.error` event before the turn goes idle, so a client
    /// observes the failure. `None` on a normal completion.
    pub failure: Option<StepFailure>,
}

/// A terminal run fault carried from the neutral `EndCause::Error` so the adapter
/// can project `session.error`. Neutral (a stable `code` + human `message`), not
/// managed-wire vocabulary.
pub struct StepFailure {
    pub code: String,
    pub message: String,
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
    /// The session's mounted resources (ADR-0038), parsed from the wire `resources[]`:
    /// files, memory stores, repos. The host realizes each into the run's sandbox and
    /// appends a prompt fragment to the system prompt (A3a). Empty = no mounts.
    pub resources: Vec<SessionResource>,
    /// The session's requested model (R2), staged so the run binds it; `None` →
    /// the host default.
    pub model: Option<String>,
    /// The session's requested runtime adapter (R3): `"acp:*"` routes to an ACP
    /// CLI; `None`/`"awaken"` → native.
    pub runtime: Option<String>,
    /// Deny network egress for the session's sandbox, resolved from its environment's
    /// networking policy (a non-`unrestricted` policy → `true`). The host runs the
    /// `bash` tool under a `bwrap --unshare-net` namespace. `false` = host network.
    pub deny_egress: bool,
}

/// One session MCP server, bound at creation: the wire name/url plus the vault
/// credential the URL matched (`None` when no vault credential matches — the
/// host then connects unauthenticated and the server decides). Consumed by
/// `ManagedHost::prepare_session` in the server assembly.
pub struct McpServerBinding {
    pub name: String,
    pub url: String,
    /// The matched vault credential's neutral row id, as a plain string (the port
    /// speaks no control-plane vocabulary — the host re-types it into the vault's
    /// `CredentialSourceId` at the lookup). `None` = no vault credential matched.
    pub credential_source_id: Option<String>,
    /// The matched credential's stored refresh configuration
    /// ([`VaultState::mcp_refresh_for_source`]), so the host can register a
    /// transport-level refresher next to the bearer. `None` when the credential
    /// is not refreshable (entered without a refresh object).
    pub refresh: Option<McpRefreshBinding>,
}

/// A queued live-inbox message on the session's in-flight turn. `id` is the
/// runtime's queue identity — targetable until the engine consumes the entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveInboxEntry {
    pub id: u64,
    pub content: Vec<ContentBlock>,
}

/// The session's live-inbox resource: the editable queue of messages addressed
/// to the in-flight turn. `active: false` means no native turn is running (the
/// queue shows empty; sends go through the normal event path instead).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveInboxSnapshot {
    pub active: bool,
    pub version: u64,
    pub messages: Vec<LiveInboxEntry>,
}

impl LiveInboxSnapshot {
    pub fn inactive() -> Self {
        Self {
            active: false,
            version: 0,
            messages: Vec::new(),
        }
    }
}

/// Why a live-inbox operation failed. Mirrors the runtime contract's edit
/// errors, plus `Inactive` for "no native turn in flight on this session".
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveInboxError {
    #[error("no turn is in flight; send the message as a normal event")]
    Inactive,
    #[error("no queued message with that id")]
    UnknownMessage,
    #[error("proposed order does not match the current queue")]
    StaleOrder,
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Run one user turn on `thread` to its first pause or end. `content` is the
    /// user message's full block list (multimodal): text interleaved with any
    /// image blocks, never flattened to a bare string.
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError>;

    /// Run one user turn, installing `sink` as the run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        _sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.run(agent, thread, content).await
    }

    /// Answer a built-in tool the run parked on (allow/deny) and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending built-in tool.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: Decision,
    ) -> Result<StepOutcome, RunError>;

    /// Deliver a client-executed tool's result to the parked run and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending client-executed tool.
    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<StepOutcome, RunError>;

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

    /// Attach a resource to a LIVE session: stage its mount and make it take effect
    /// on the thread's next turn (the server impl merges it into the thread's staged
    /// resources and evicts the cached sandbox so the next turn rebuilds with it).
    /// The default is a no-op, so a host without resource staging is unaffected.
    async fn attach_resource(
        &self,
        _thread: &str,
        _resource: SessionResource,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// Detach a resource from a LIVE session: flush any write-back (memory) while the
    /// old sandbox is still live, drop this resource's mount, and evict the cached
    /// sandbox so the next turn rebuilds without it. Takes the resolved resource (not
    /// just an id) so the host has its mount path and kind. The default is a no-op.
    async fn detach_resource(
        &self,
        _thread: &str,
        _resource: SessionResource,
    ) -> Result<(), RunError> {
        Ok(())
    }

    /// Rotate a LIVE session resource's authorization token (Managed Agents
    /// `resources.update`): re-key the host-held credential so future operations use the
    /// new token — for a `github_repository`, both the clone token and the injected GitHub
    /// MCP server's bearer — then evict the cached sandbox so the next turn rebuilds with it.
    /// `resource.auth_token` carries the NEW token. The default is a no-op.
    async fn rotate_resource_token(
        &self,
        _thread: &str,
        _resource: SessionResource,
    ) -> Result<(), RunError> {
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

    /// The session's accumulated token usage across all turns, surfaced on the
    /// session's `usage` field. The default is empty — a runtime that reports no usage
    /// (the deterministic in-process models).
    async fn session_usage(&self, _thread: &str) -> SessionUsage {
        SessionUsage::default()
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

    /// The live-inbox queue on `thread`'s in-flight turn. The default reports
    /// an inactive queue, so a host without live-inbox wiring is unaffected.
    async fn live_inbox_snapshot(&self, _thread: &str) -> LiveInboxSnapshot {
        LiveInboxSnapshot::inactive()
    }

    /// Queue a message onto `thread`'s in-flight turn; it is folded into the
    /// running transcript at the next safe boundary. Fails `Inactive` when no
    /// native turn is running (the caller should send a normal event instead).
    async fn live_inbox_queue(
        &self,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Delete one queued (not yet consumed) message.
    async fn live_inbox_remove(&self, _thread: &str, _id: u64) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Replace one queued message's content, keeping its id and position.
    async fn live_inbox_replace(
        &self,
        _thread: &str,
        _id: u64,
        _content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    /// Reorder the queue to exactly `order` (a full permutation of current ids).
    async fn live_inbox_reorder(
        &self,
        _thread: &str,
        _order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

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

/// The session-level token usage the managed wire reports (the port's neutral shape;
/// the runtime's per-model `TokenUsage` totals are mapped onto this by the host, so
/// this crate needs no runtime-plane type). Cumulative across all turns and models.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}
