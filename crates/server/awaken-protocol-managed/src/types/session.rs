//! The Managed Agents wire transfer objects (anti-corruption boundary).
//!
//! Pure serde types, each mapping 1:1 onto the official SDK's beta
//! `managed-agents` types — the *only* place Anthropic protocol vocabulary lives
//! (G16). Field names, `type` strings, and the `stop_reason` tagged shape are
//! byte-compatible with `anthropic-beta: managed-agents-2026-04-01`, so the
//! TypeScript SDK can drive the server. Message content reuses the neutral
//! [`ContentBlock`], which already serializes as `{ "type": "text", "text": .. }`.
//!
//! This module holds *shapes only*. The logic that assembles them from neutral
//! domain state — projecting engine events into [`OutboundKind`], building a
//! [`Session`] record — lives in `state` and `project`, kept deliberately apart.

use awaken_agent_contract::agent::content::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Anthropic error envelope: `{ "type": "error", "error": { "type", "message" } }`.
/// The SDK parses this shape to populate `err.error.type` / `err.error.message`;
/// `error.type` is the status-keyed discriminator (`not_found_error`,
/// `invalid_request_error`, `api_error`, …).
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub error: ApiError,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
}

impl ErrorResponse {
    /// Build an envelope with `type: "error"` and the given inner error type.
    pub fn new(error_type: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: "error",
            error: ApiError {
                kind: error_type,
                message: message.into(),
            },
        }
    }
}

/// The resolved `model` axis of an `agent_with_overrides` session reference — the
/// SDK's override semantics for a single field made explicit: **omit** = inherit the
/// agent's model; **`null`** = clear, rejected for `model` since a session always
/// needs one (400 `agent_model_required`); **a value** = replace for this session.
/// This is the domain-facing tri-state [`AgentRef::model_override`] returns; the wire
/// form is the double-`Option` on [`AgentRefObject::model`].
#[derive(Debug, Clone)]
pub enum ModelOverride {
    /// `model` key absent — inherit the referenced agent version's model.
    Absent,
    /// `model: null` — an explicit clear, which the API forbids for the model axis.
    Cleared,
    /// `model` set to a bare id or `{id, speed?}` — replace for this session only.
    Set(ModelConfig),
}

/// The `type` discriminator on an [`AgentRefObject`]. Optional and tolerant: an object
/// without a `type` (or with an unrecognized one) is the plain `agent` reference,
/// preserving the pre-overrides behavior where the tag was ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRefKind {
    Agent,
    AgentWithOverrides,
    #[serde(other)]
    Other,
}

/// The object form of `agent`: `{id, type?, version?, model?, ...}`. A strongly-typed
/// struct rather than a hand-rolled deserializer — the one field needing more than a
/// plain `Option` is `model`, whose absent/`null`/value tri-state (the not-clearable
/// rule) rides the standard double-`Option` idiom. The other override fields
/// (`system`/`tools`/`mcp_servers`/`skills`) are accepted for wire-compatibility but
/// not yet applied, so they are not modeled here.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentRefObject {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: Option<AgentRefKind>,
    #[serde(default)]
    pub version: Option<u32>,
    /// Outer `None` = `model` omitted; `Some(None)` = `model: null`; `Some(Some(_))` =
    /// a value. Only meaningful when `kind` is `agent_with_overrides`.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub model: Option<Option<super::agent::ModelInput>>,
}

/// The standard serde double-`Option` reader: distinguishes an absent field (handled
/// by `#[serde(default)]` → outer `None`) from a present `null` (`Some(None)`) from a
/// present value (`Some(Some(_))`). Load-bearing for the model not-clearable rule.
fn deserialize_double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// `agent` in a create-session request — the SDK's
/// `string | {id, type:'agent', version?} | {id, type:'agent_with_overrides',
/// version?, model?, system?, tools?, ...}` (`BetaManagedAgentsAgentParams`).
/// Untagged: a JSON string is [`AgentRef::Id`]; a JSON object is [`AgentRef::Object`],
/// whose `type` then selects plain-reference vs. overrides. Per-session runtime
/// selection still travels in the session `metadata` bag (see [`SessionCreateParams`]);
/// the model now rides the official override object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AgentRef {
    Id(String),
    Object(AgentRefObject),
}

impl AgentRef {
    pub fn id(&self) -> &str {
        match self {
            AgentRef::Id(id) => id,
            AgentRef::Object(obj) => &obj.id,
        }
    }

    /// The agent version the client pinned, if any (`None` = latest).
    pub fn version(&self) -> Option<u32> {
        match self {
            AgentRef::Id(_) => None,
            AgentRef::Object(obj) => obj.version,
        }
    }

    /// The single-session model override. Only an `agent_with_overrides` object carries
    /// one; every other form reports [`ModelOverride::Absent`].
    pub fn model_override(&self) -> ModelOverride {
        match self {
            AgentRef::Object(AgentRefObject {
                kind: Some(AgentRefKind::AgentWithOverrides),
                model,
                ..
            }) => match model {
                None => ModelOverride::Absent,
                Some(None) => ModelOverride::Cleared,
                Some(Some(input)) => ModelOverride::Set(input.clone().into_config()),
            },
            _ => ModelOverride::Absent,
        }
    }
}

/// `POST /v1/sessions` request body (only the fields the runtime slice reads;
/// unknown fields are ignored so the full SDK payload is accepted).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionCreateParams {
    pub agent: AgentRef,
    #[serde(default)]
    pub environment_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
    /// MCP servers this session connects to (ADR-0043 Phase 3). Bound to vault
    /// credentials via `vault_ids` at session creation.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    /// Vaults whose credentials the session may use (matched to `mcp_servers`
    /// by exact `mcp_server_url`).
    #[serde(default)]
    pub vault_ids: Vec<String>,
    /// Mounted resources (ADR-0038): file / memory_store / github_repository entries
    /// the SDK sends on `sessions.create`. Kept as opaque `Value`s (the wire shapes
    /// differ per kind); the state layer lowers each into a typed input binding.
    #[serde(default)]
    pub resources: Vec<Value>,
}

/// One MCP server on the wire (`BetaManagedAgentsMCPServerURLDefinition` /
/// `BetaManagedAgentsURLMCPServerParams`): `{ name, type: "url", url }`. The
/// SDK's `type: "url"` tag is tolerated (and ignored) on input — there is only
/// one variant — and always re-serialized on output.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub url: String,
}

impl Serialize for McpServer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("McpServer", 3)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("type", "url")?;
        s.serialize_field("url", &self.url)?;
        s.end()
    }
}

/// The `BetaManagedAgentsModelConfig` object: `{ id, speed? }`. A session/agent's
/// `model` is this object on the wire, never a bare string (the SDK reads
/// `agent.model.id`). The single definition of the model-config shape — the agent
/// registry and session/thread projections all reuse it rather than rebuild it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

impl ModelConfig {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            speed: None,
        }
    }
}

/// The agent object echoed inside a session response
/// (`BetaManagedAgentsSessionAgent`).
#[derive(Debug, Clone, Serialize)]
pub struct SessionAgent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: u32,
    pub model: ModelConfig,
    pub name: String,
    /// SDK-required (nullable) fields; emitted as `null` when the host has none.
    pub description: Option<String>,
    pub system: Option<String>,
    // Opaque SDK unions passed through verbatim (the tool / MCP-server / skill
    // unions) — same treatment as [`Agent`](super::agent::Agent)'s.
    pub tools: Vec<Value>,
    pub mcp_servers: Vec<Value>,
    pub skills: Vec<Value>,
    /// The multiagent coordinator roster, omitted when the agent delegates to no one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiagent: Option<Value>,
}

/// `BetaManagedAgentsSessionStats` — coarse per-session timing/counters. Empty on
/// this surface (serializes as `{}`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionStats {}

/// `BetaManagedAgentsSessionUsage` — a session's accumulated token usage. Zero
/// until the first turn commits usage.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

/// `BetaManagedAgentsSpanModelUsage` — token usage for a *single* model request
/// (as opposed to the session's accumulated [`Usage`]). Carried by
/// `span.model_request_end`; reuses [`Usage`]'s four token fields (flattened) and
/// adds the optional inference-speed mode.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SpanModelUsage {
    #[serde(flatten)]
    pub usage: Usage,
    /// Inference speed mode (`standard`/`fast`); omitted when the model reports none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

/// `BetaManagedAgentsSession` response (minimal but SDK-parseable).
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub agent: SessionAgent,
    pub environment_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    pub title: Option<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub resources: Vec<Value>,
    pub outcome_evaluations: Vec<Value>,
    pub status: &'static str,
    pub stats: SessionStats,
    pub usage: Usage,
    /// The vaults the session is bound to (`vault_ids`).
    pub vault_ids: Vec<String>,
    /// Set when the session was launched by a deployment run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
}

/// A client's `user.tool_confirmation` decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmResult {
    Allow,
    Deny,
}

/// Inbound events a client posts to `POST /v1/sessions/{id}/events`. M1 acts on
/// `user.message`; the rest deserialize (so the batch is accepted) and are
/// wired in later milestones.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum InboundEvent {
    #[serde(rename = "user.message")]
    UserMessage {
        content: Vec<ContentBlock>,
        #[serde(default)]
        session_thread_id: Option<String>,
        /// Per-turn model override (R5): switches the thread to `model` for this
        /// turn onward. Absent → keep the session's current model.
        #[serde(default)]
        model: Option<String>,
    },
    #[serde(rename = "system.message")]
    SystemMessage { content: Vec<ContentBlock> },
    #[serde(rename = "user.tool_confirmation")]
    UserToolConfirmation {
        tool_use_id: String,
        result: ConfirmResult,
        #[serde(default)]
        deny_message: Option<String>,
    },
    #[serde(rename = "user.custom_tool_result")]
    UserCustomToolResult {
        custom_tool_use_id: String,
        #[serde(default)]
        content: Option<Vec<ContentBlock>>,
        #[serde(default)]
        is_error: bool,
    },
    /// The generic client-provided result for an awaiting tool, keyed by the
    /// `agent.tool_use` id from a `requires_action` `event_ids` — the SDK's
    /// `user.tool_result`. Handled like `user.custom_tool_result` (delivers a
    /// client tool's result), keyed by `tool_use_id` rather than `custom_tool_use_id`.
    #[serde(rename = "user.tool_result")]
    UserToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<Vec<ContentBlock>>,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(rename = "user.define_outcome")]
    UserDefineOutcome {
        description: String,
        rubric: Value,
        #[serde(default)]
        max_iterations: Option<u32>,
    },
    #[serde(rename = "user.interrupt")]
    UserInterrupt {
        #[serde(default)]
        session_thread_id: Option<String>,
    },
}

impl InboundEvent {
    /// The public `type` string, echoed on the receipt.
    pub fn type_str(&self) -> &'static str {
        match self {
            InboundEvent::UserMessage { .. } => "user.message",
            InboundEvent::SystemMessage { .. } => "system.message",
            InboundEvent::UserToolConfirmation { .. } => "user.tool_confirmation",
            InboundEvent::UserCustomToolResult { .. } => "user.custom_tool_result",
            InboundEvent::UserToolResult { .. } => "user.tool_result",
            InboundEvent::UserDefineOutcome { .. } => "user.define_outcome",
            InboundEvent::UserInterrupt { .. } => "user.interrupt",
        }
    }
}

/// `POST .../events` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct SendEventsRequest {
    pub events: Vec<InboundEvent>,
}

/// One receipt in the `POST .../events` response `data` array.
#[derive(Debug, Clone, Serialize)]
pub struct EventReceipt {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub processed_at: Option<String>,
}

/// `POST .../events` response.
#[derive(Debug, Clone, Serialize)]
pub struct SendEventsResponse {
    pub data: Vec<EventReceipt>,
}

/// Why a session went idle — a tagged object, never a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    RequiresAction { event_ids: Vec<String> },
    RetriesExhausted,
}

/// What a client should do next in response to a `session.error` — a tagged
/// object (the SDK's `BetaManagedAgentsRetryStatus` union), never a bare string:
/// `Retrying` (transient, the runtime will retry), `Exhausted` (retry budget spent,
/// but the session stays usable), or `Terminal` (retrying cannot help this fault).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RetryStatus {
    Retrying,
    Exhausted,
    Terminal,
}

/// The `error` object of a `session.error` event — one of the SDK's error variants
/// (`unknown_error` plus the classified `model_rate_limited_error` /
/// `model_request_failed_error`), each a human-readable `message` plus the
/// `retry_status` the client keys recovery on. The neutral runtime's stable fault
/// `code` selects the variant; an unrecognized code honestly falls back to
/// `unknown_error`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionError {
    /// The SDK error type, e.g. `"unknown_error"` or `"model_rate_limited_error"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
    pub retry_status: RetryStatus,
}

impl SessionError {
    /// Classify a neutral runtime fault `(code, message)` into an SDK error object.
    /// The `code` is the stable snake_case class from `Failure::Inference`
    /// (`rate_limited` / `context_overflow` / `unauthorized` / …); it picks the error
    /// `type` and the `retry_status`. An unknown code (or a plain internal fault) is
    /// the catch-all `unknown_error` / `exhausted` — the session stays usable, so we
    /// don't force-terminate on one failed turn.
    pub fn classify(code: &str, message: impl Into<String>) -> Self {
        let (kind, retry_status) = match code {
            "rate_limited" => ("model_rate_limited_error", RetryStatus::Exhausted),
            "context_overflow" | "unauthorized" => {
                ("model_request_failed_error", RetryStatus::Terminal)
            }
            _ => ("unknown_error", RetryStatus::Exhausted),
        };
        Self {
            kind,
            message: message.into(),
            retry_status,
        }
    }
}

/// The payload of an outbound event (its `type` plus kind-specific fields).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum OutboundKind {
    #[serde(rename = "agent.message")]
    AgentMessage { content: Vec<ContentBlock> },
    /// The agent's extended-thinking block (`agent.thinking`) — `{id, type,
    /// processed_at}`, no payload, mirroring the SDK's
    /// `BetaManagedAgentsAgentThinkingEvent`. Wire type defined for catalog
    /// completeness; not yet emitted — awaken's stream carries no thinking channel
    /// (see the conformance matrix's `agent.thinking` gap).
    #[serde(rename = "agent.thinking")]
    AgentThinking {},
    #[serde(rename = "agent.tool_use")]
    AgentToolUse {
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        evaluated_permission: Option<String>,
    },
    #[serde(rename = "agent.tool_result")]
    AgentToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(rename = "agent.custom_tool_use")]
    AgentCustomToolUse { name: String, input: Value },
    /// An MCP tool call (`agent.mcp_tool_use`): a host-executed tool from an MCP
    /// server, distinguished from a built-in `agent.tool_use` by the `mcp__` name.
    #[serde(rename = "agent.mcp_tool_use")]
    AgentMcpToolUse {
        name: String,
        mcp_server_name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        evaluated_permission: Option<String>,
    },
    /// The result of an MCP tool call (`agent.mcp_tool_result`), keyed by
    /// `mcp_tool_use_id`.
    #[serde(rename = "agent.mcp_tool_result")]
    AgentMcpToolResult {
        mcp_tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// A problem occurred during session execution (`session.error`). Carries the
    /// SDK error object; a `retry_status: terminal` error means the run failed
    /// unrecoverably. Committed to the event log so a streaming/listing client
    /// observes the failure, not just the POST caller who gets the HTTP envelope.
    #[serde(rename = "session.error")]
    SessionError { error: SessionError },
    #[serde(rename = "session.status_running")]
    SessionStatusRunning {},
    #[serde(rename = "session.status_idle")]
    SessionStatusIdle { stop_reason: StopReason },
    /// The session is recovering from a transient error and is rescheduled for
    /// execution (`session.status_rescheduled`) — `{id, type, processed_at}`, no
    /// payload. Wire type defined for catalog completeness; not yet emitted — the
    /// runtime collapses transient retries below the projection seam (see the
    /// conformance matrix's `session.status_rescheduled` gap).
    #[serde(rename = "session.status_rescheduled")]
    SessionStatusRescheduled {},
    /// The session reached its irreversible terminal state (emitted when the
    /// session is archived — this server models archive as termination, stamping
    /// `archived_at` and `status: "terminated"`). A client streaming or listing
    /// the session sees this as the last event; no further turns are accepted.
    #[serde(rename = "session.status_terminated")]
    SessionStatusTerminated {},
    /// The session was deleted (`session.deleted`) — a terminal stream frame
    /// pushed to any open SSE connection just before the record is dropped.
    /// Unlike archive (which tombstones a still-listable `terminated` record),
    /// delete removes the session, so this frame is live-broadcast only: a
    /// subsequent `events.list`/`retrieve` is a 404, not a replay.
    #[serde(rename = "session.deleted")]
    SessionDeleted {},
    /// A subagent (multiagent delegate) thread was spawned within the session —
    /// the SDK's `session.thread_created`. `agent_name` is the callable delegate
    /// the child thread runs.
    #[serde(rename = "session.thread_created")]
    SessionThreadCreated {
        session_thread_id: String,
        agent_name: String,
    },
    /// A subagent child thread started running (`session.thread_status_running`).
    #[serde(rename = "session.thread_status_running")]
    SessionThreadStatusRunning {
        session_thread_id: String,
        agent_name: String,
    },
    /// A subagent child thread went idle (`session.thread_status_idle`), carrying
    /// the same `stop_reason` shape as the session's own idle.
    #[serde(rename = "session.thread_status_idle")]
    SessionThreadStatusIdle {
        session_thread_id: String,
        agent_name: String,
        stop_reason: StopReason,
    },
    /// A subagent child thread hit a transient error and is retrying
    /// (`session.thread_status_rescheduled`) — same identity shape as the other
    /// thread-status events. Wire type defined for catalog completeness; not yet
    /// emitted — the thread rescheduled fact isn't surfaced (see the conformance
    /// matrix's `thread_status_rescheduled` gap).
    #[serde(rename = "session.thread_status_rescheduled")]
    SessionThreadStatusRescheduled {
        session_thread_id: String,
        agent_name: String,
    },
    /// The session's `metadata`/`title` changed (`session.updated`), carrying the
    /// title (when the update set it) and the full metadata bag (when non-empty).
    #[serde(rename = "session.updated")]
    SessionUpdated {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        metadata: std::collections::BTreeMap<String, String>,
    },
    /// A subagent child thread terminated (`session.thread_status_terminated`),
    /// e.g. when archived.
    #[serde(rename = "session.thread_status_terminated")]
    SessionThreadStatusTerminated {
        session_thread_id: String,
        agent_name: String,
    },
    /// The coordinator sent the delegate its input (`agent.thread_message_sent`).
    #[serde(rename = "agent.thread_message_sent")]
    AgentThreadMessageSent {
        to_session_thread_id: String,
        to_agent_name: String,
        content: Vec<ContentBlock>,
    },
    /// The coordinator received the delegate's reply (`agent.thread_message_received`).
    #[serde(rename = "agent.thread_message_received")]
    AgentThreadMessageReceived {
        from_session_thread_id: String,
        from_agent_name: String,
        content: Vec<ContentBlock>,
    },
    /// The conversation history was summarized to fit context (the compact plugin
    /// folded older turns). A pure marker: its shape matches the installed SDK's
    /// `BetaManagedAgentsAgentThreadContextCompactedEvent` — `{id, type,
    /// processed_at}`, no payload (aligned to `@anthropic-ai/sdk`, not guessed).
    #[serde(rename = "agent.thread_context_compacted")]
    ThreadContextCompacted {},
    /// A model request was initiated (`span.model_request_start`) — `{id, type,
    /// processed_at}`, no payload; its `id` is referenced by the paired
    /// `span.model_request_end`. Wire type defined for catalog completeness; not
    /// yet emitted — the model call sits below the runtime seam (see the
    /// conformance matrix's `span.model_request_*` gap).
    #[serde(rename = "span.model_request_start")]
    SpanModelRequestStart {},
    /// A model request completed (`span.model_request_end`), carrying the paired
    /// start id, a nullable error flag, and this single request's token usage.
    /// Wire type defined for catalog completeness; not yet emitted (same gap as
    /// `span.model_request_start`).
    #[serde(rename = "span.model_request_end")]
    SpanModelRequestEnd {
        model_request_start_id: String,
        // Nullable per the SDK (`boolean | null`): the key is always present.
        is_error: Option<bool>,
        model_usage: SpanModelUsage,
    },
    #[serde(rename = "span.outcome_evaluation_start")]
    SpanOutcomeEvaluationStart { outcome_id: String, iteration: u32 },
    /// A progress ping while a revision cycle is being graded
    /// (`span.outcome_evaluation_ongoing`), between the start and end spans.
    #[serde(rename = "span.outcome_evaluation_ongoing")]
    SpanOutcomeEvaluationOngoing { outcome_id: String, iteration: u32 },
    #[serde(rename = "span.outcome_evaluation_end")]
    SpanOutcomeEvaluationEnd {
        outcome_id: String,
        iteration: u32,
        result: String,
        explanation: String,
    },
}

impl OutboundKind {
    /// The public `type` string — also the SSE `event:` name the SDK dispatches on.
    pub fn type_str(&self) -> &'static str {
        match self {
            OutboundKind::AgentMessage { .. } => "agent.message",
            OutboundKind::AgentThinking {} => "agent.thinking",
            OutboundKind::AgentToolUse { .. } => "agent.tool_use",
            OutboundKind::AgentToolResult { .. } => "agent.tool_result",
            OutboundKind::AgentCustomToolUse { .. } => "agent.custom_tool_use",
            OutboundKind::AgentMcpToolUse { .. } => "agent.mcp_tool_use",
            OutboundKind::AgentMcpToolResult { .. } => "agent.mcp_tool_result",
            OutboundKind::SessionError { .. } => "session.error",
            OutboundKind::SessionStatusRunning {} => "session.status_running",
            OutboundKind::SessionStatusIdle { .. } => "session.status_idle",
            OutboundKind::SessionStatusRescheduled {} => "session.status_rescheduled",
            OutboundKind::SessionStatusTerminated {} => "session.status_terminated",
            OutboundKind::SessionDeleted {} => "session.deleted",
            OutboundKind::SessionThreadCreated { .. } => "session.thread_created",
            OutboundKind::SessionThreadStatusRunning { .. } => "session.thread_status_running",
            OutboundKind::SessionThreadStatusIdle { .. } => "session.thread_status_idle",
            OutboundKind::SessionThreadStatusRescheduled { .. } => {
                "session.thread_status_rescheduled"
            }
            OutboundKind::SessionUpdated { .. } => "session.updated",
            OutboundKind::SessionThreadStatusTerminated { .. } => {
                "session.thread_status_terminated"
            }
            OutboundKind::AgentThreadMessageSent { .. } => "agent.thread_message_sent",
            OutboundKind::AgentThreadMessageReceived { .. } => "agent.thread_message_received",
            OutboundKind::ThreadContextCompacted { .. } => "agent.thread_context_compacted",
            OutboundKind::SpanModelRequestStart {} => "span.model_request_start",
            OutboundKind::SpanModelRequestEnd { .. } => "span.model_request_end",
            OutboundKind::SpanOutcomeEvaluationStart { .. } => "span.outcome_evaluation_start",
            OutboundKind::SpanOutcomeEvaluationOngoing { .. } => "span.outcome_evaluation_ongoing",
            OutboundKind::SpanOutcomeEvaluationEnd { .. } => "span.outcome_evaluation_end",
        }
    }
}

/// A committed public event: `id` + `type` + kind fields + `processed_at`. This
/// is what a receipt references, a list returns, and the SSE stream delivers.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: String,
    #[serde(flatten)]
    pub kind: OutboundKind,
    pub processed_at: Option<String>,
}

impl Event {
    /// The public event `type`, used as the SSE `event:` field.
    pub fn type_str(&self) -> &'static str {
        self.kind.type_str()
    }
}

/// A stream-only *live preview* frame (`event_start` / `event_delta`), emitted on
/// the SSE stream **only** while a turn is in flight and **never** persisted in
/// the event log — the buffered `agent.message` stays the authoritative record.
/// Mirrors the official Managed Agents live-preview wire: an `event_start`
/// announces the upcoming buffered event's `type` + `id`, then `event_delta`
/// frames carry incremental `content_delta` text. The delta type is
/// `content_delta` (NOT the Messages-API `content_block_delta`), so the SDK's
/// live-preview accumulator — not its Messages-API accumulator — reconciles it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum PreviewFrame {
    #[serde(rename = "event_start")]
    EventStart { event: PreviewTarget },
    #[serde(rename = "event_delta")]
    EventDelta {
        event_id: String,
        delta: PreviewDelta,
    },
}

impl PreviewFrame {
    pub fn type_str(&self) -> &'static str {
        match self {
            PreviewFrame::EventStart { .. } => "event_start",
            PreviewFrame::EventDelta { .. } => "event_delta",
        }
    }
}

/// The `event` on an `event_start`: the `type` + `id` of the buffered event this
/// preview announces. Its `id` equals the buffered event's `id`, so a client
/// reconciles the accumulated preview against the committed event by id.
#[derive(Debug, Clone, Serialize)]
pub struct PreviewTarget {
    #[serde(rename = "type")]
    pub event_type: String,
    pub id: String,
}

/// The `delta` on an `event_delta` — always `content_delta` with an `index` and a
/// text `content` block: the incremental slice of the previewed message.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum PreviewDelta {
    #[serde(rename = "content_delta")]
    ContentDelta {
        index: usize,
        content: PreviewContent,
    },
}

/// The incremental content on a `content_delta`. Text only — awaken's live stream
/// carries text deltas; tool use and thinking are never previewed (matching the
/// official wire: "tool use, tool results … are never previewed").
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum PreviewContent {
    #[serde(rename = "text")]
    Text { text: String },
}

/// A frame on the live SSE stream: either a committed [`Event`] (also in the
/// persisted log) or a stream-only [`PreviewFrame`]. The per-session broadcast
/// carries both; each SSE connection forwards previews only if it opted in via
/// `event_deltas[]`.
#[derive(Debug, Clone)]
pub enum StreamFrame {
    Committed(Event),
    Preview(PreviewFrame),
}

impl StreamFrame {
    /// The SSE `event:` name for this frame.
    pub fn type_str(&self) -> &'static str {
        match self {
            StreamFrame::Committed(e) => e.type_str(),
            StreamFrame::Preview(p) => p.type_str(),
        }
    }

    /// The JSON body for the SSE `data:` line.
    pub fn data(&self) -> String {
        match self {
            StreamFrame::Committed(e) => serde_json::to_string(e).expect("event serializes"),
            StreamFrame::Preview(p) => serde_json::to_string(p).expect("preview serializes"),
        }
    }
}

/// `GET .../events` response.
#[derive(Debug, Clone, Serialize)]
pub struct ListEventsResponse {
    pub data: Vec<Event>,
    pub next_page: Option<String>,
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ref_parses_a_bare_id_and_a_tagged_object() {
        // Bare string id.
        let bare: AgentRef = serde_json::from_str(r#""assistant""#).unwrap();
        assert_eq!(bare.id(), "assistant");
        // The SDK's `{ id, type:"agent", version }` object — the `type` tag is
        // tolerated (ignored) on input.
        let obj: AgentRef =
            serde_json::from_str(r#"{"id":"assistant","type":"agent","version":3}"#).unwrap();
        assert_eq!(obj.id(), "assistant");
        assert_eq!(obj.version(), Some(3));
        // A plain reference never overrides the model.
        assert!(matches!(bare.model_override(), ModelOverride::Absent));
        assert!(matches!(obj.model_override(), ModelOverride::Absent));
        // An unrecognized `type` degrades to a plain reference (tolerant), and does
        // not surface a model override even if one rode along.
        let odd: AgentRef =
            serde_json::from_str(r#"{"id":"a","type":"future_kind","model":"x"}"#).unwrap();
        assert_eq!(odd.id(), "a");
        assert!(matches!(odd.model_override(), ModelOverride::Absent));
    }

    #[test]
    fn agent_with_overrides_replaces_the_model_for_the_session() {
        // Bare-string model override.
        let s: AgentRef = serde_json::from_str(
            r#"{"id":"assistant","type":"agent_with_overrides","model":"claude-sonnet-5"}"#,
        )
        .unwrap();
        match s.model_override() {
            ModelOverride::Set(cfg) => {
                assert_eq!(cfg.id, "claude-sonnet-5");
                assert!(cfg.speed.is_none());
            }
            other => panic!("expected Set, got {other:?}"),
        }
        // `{id, speed}` object model override, with a pinned version.
        let o: AgentRef = serde_json::from_str(
            r#"{"id":"assistant","type":"agent_with_overrides","version":2,"model":{"id":"claude-opus-4-8","speed":"fast"}}"#,
        )
        .unwrap();
        assert_eq!(o.version(), Some(2));
        match o.model_override() {
            ModelOverride::Set(cfg) => {
                assert_eq!(cfg.id, "claude-opus-4-8");
                assert_eq!(cfg.speed.as_deref(), Some("fast"));
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn overrides_distinguish_absent_from_null_model() {
        // `model` omitted → inherit (Absent), not a clear.
        let absent: AgentRef =
            serde_json::from_str(r#"{"id":"a","type":"agent_with_overrides"}"#).unwrap();
        assert!(matches!(absent.model_override(), ModelOverride::Absent));
        // `model: null` → an explicit clear (rejected downstream with 400).
        let cleared: AgentRef =
            serde_json::from_str(r#"{"id":"a","type":"agent_with_overrides","model":null}"#)
                .unwrap();
        assert!(matches!(cleared.model_override(), ModelOverride::Cleared));
    }

    #[test]
    fn user_message_carries_a_per_turn_model_override() {
        let with: InboundEvent = serde_json::from_str(
            r#"{"type":"user.message","content":[{"type":"text","text":"hi"}],"model":"fast"}"#,
        )
        .unwrap();
        match with {
            InboundEvent::UserMessage { model, .. } => assert_eq!(model.as_deref(), Some("fast")),
            _ => panic!("expected user.message"),
        }
        // Absent → None (backward compatible).
        let without: InboundEvent = serde_json::from_str(
            r#"{"type":"user.message","content":[{"type":"text","text":"hi"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            without,
            InboundEvent::UserMessage { model: None, .. }
        ));
    }

    /// The newly catalogued outbound wire types serialize to exactly the shape the
    /// installed SDK declares (`events.d.ts`) — the `type` tag plus the right field
    /// set. Guards the type-catalog completion against silently diverging from the
    /// SDK even before emission is wired.
    #[test]
    fn newly_catalogued_outbound_events_serialize_to_the_sdk_wire_shape() {
        use std::collections::BTreeSet;
        let ev = |kind| Event {
            id: "evt_0".to_string(),
            kind,
            processed_at: Some("2026-01-01T00:00:00Z".to_string()),
        };
        let keys =
            |v: &Value| -> BTreeSet<String> { v.as_object().unwrap().keys().cloned().collect() };
        let bare: BTreeSet<String> = ["id", "type", "processed_at"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // No-payload events: exactly {id, type, processed_at} + the right tag.
        for (kind, ty) in [
            (OutboundKind::AgentThinking {}, "agent.thinking"),
            (
                OutboundKind::SessionStatusRescheduled {},
                "session.status_rescheduled",
            ),
            (
                OutboundKind::SpanModelRequestStart {},
                "span.model_request_start",
            ),
        ] {
            let v = serde_json::to_value(ev(kind)).unwrap();
            assert_eq!(v["type"], ty);
            assert_eq!(
                keys(&v),
                bare,
                "{ty} is a bare {{id,type,processed_at}} event"
            );
        }

        // thread_status_rescheduled carries the thread's identity.
        let v = serde_json::to_value(ev(OutboundKind::SessionThreadStatusRescheduled {
            session_thread_id: "sthr_1".to_string(),
            agent_name: "researcher".to_string(),
        }))
        .unwrap();
        assert_eq!(v["type"], "session.thread_status_rescheduled");
        assert_eq!(v["session_thread_id"], "sthr_1");
        assert_eq!(v["agent_name"], "researcher");

        // model_request_end carries the paired start id, a nullable error flag, and
        // this single request's token usage (four SDK fields; speed omitted here).
        let v = serde_json::to_value(ev(OutboundKind::SpanModelRequestEnd {
            model_request_start_id: "evt_start".to_string(),
            is_error: None,
            model_usage: SpanModelUsage {
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                speed: None,
            },
        }))
        .unwrap();
        assert_eq!(v["type"], "span.model_request_end");
        assert_eq!(v["model_request_start_id"], "evt_start");
        assert!(
            v["is_error"].is_null(),
            "is_error is present and null (SDK: boolean | null)"
        );
        assert_eq!(v["model_usage"]["input_tokens"], 10);
        assert_eq!(v["model_usage"]["output_tokens"], 20);
        assert!(
            v["model_usage"].get("speed").is_none(),
            "speed is omitted when None"
        );
    }
}
