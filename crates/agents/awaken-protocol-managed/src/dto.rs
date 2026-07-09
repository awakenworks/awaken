//! The Managed Agents public wire DTOs (anti-corruption boundary).
//!
//! These are the *only* place Anthropic protocol vocabulary lives (G16). Field
//! names, `type` strings, and the `stop_reason` tagged shape are byte-compatible
//! with the official SDK (`anthropic-beta: managed-agents-2026-04-01`), so the
//! TypeScript SDK can drive the server. Message content reuses the neutral
//! [`ContentBlock`], which already serializes as `{ "type": "text", "text": .. }`.

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

/// `agent` in a create-session request: either a bare id string or an object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AgentRef {
    Id(String),
    Obj {
        id: String,
        #[serde(default)]
        version: Option<u32>,
        /// Per-session model override (R2): binds this session's runs to `model`
        /// instead of the host default. Absent → the host default model.
        #[serde(default)]
        model: Option<String>,
        /// Runtime selection (R3): `"awaken"` (native) or `"acp:<cli>"` (an
        /// external ACP agent such as Claude Code). Absent → native.
        #[serde(default)]
        runtime: Option<String>,
    },
}

impl AgentRef {
    pub fn id(&self) -> &str {
        match self {
            AgentRef::Id(id) => id,
            AgentRef::Obj { id, .. } => id,
        }
    }

    /// The session's requested model, when the client bound one (R2).
    pub fn model(&self) -> Option<&str> {
        match self {
            AgentRef::Obj { model, .. } => model.as_deref(),
            AgentRef::Id(_) => None,
        }
    }

    /// The session's requested runtime adapter, when the client chose one (R3).
    pub fn runtime(&self) -> Option<&str> {
        match self {
            AgentRef::Obj { runtime, .. } => runtime.as_deref(),
            AgentRef::Id(_) => None,
        }
    }
}

/// `POST /v1/sessions` request body (only the fields the runtime slice reads;
/// unknown fields are ignored so the full SDK payload is accepted).
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSessionRequest {
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
    pub mcp_servers: Vec<McpServerWire>,
    /// Vaults whose credentials the session may use (matched to `mcp_servers`
    /// by exact `mcp_server_url`).
    #[serde(default)]
    pub vault_ids: Vec<String>,
    /// Mounted resources (ADR-0038): file / memory_store / github_repository entries
    /// the SDK sends on `sessions.create`. Kept as opaque `Value`s (the wire shapes
    /// differ per kind); the state layer parses each into a `SessionResource`.
    #[serde(default)]
    pub resources: Vec<Value>,
}

/// One MCP server on the wire (`BetaManagedAgentsMCPServerURLDefinition` /
/// `BetaManagedAgentsURLMCPServerParams`): `{ name, type: "url", url }`. The
/// SDK's `type: "url"` tag is tolerated (and ignored) on input — there is only
/// one variant — and always re-serialized on output.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerWire {
    pub name: String,
    pub url: String,
}

impl Serialize for McpServerWire {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("McpServerWire", 3)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("type", "url")?;
        s.serialize_field("url", &self.url)?;
        s.end()
    }
}

/// The `BetaManagedAgentsModelConfig` object: `{ id, speed? }`. A session/agent's
/// `model` is this object on the wire, never a bare string (the SDK reads
/// `agent.model.id`).
#[derive(Debug, Clone, Serialize)]
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
    pub tools: Vec<Value>,
    pub mcp_servers: Vec<Value>,
    pub skills: Vec<Value>,
    /// The multiagent coordinator roster, omitted when the agent delegates to no one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiagent: Option<Value>,
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
    /// `BetaManagedAgentsSessionStats` — coarse timing (empty on this surface).
    pub stats: Value,
    /// `BetaManagedAgentsSessionUsage` — token usage (empty on this surface).
    pub usage: Value,
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
    /// The generic client-provided result for a parked tool, keyed by the
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

/// The payload of an outbound event (its `type` plus kind-specific fields).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum OutboundKind {
    #[serde(rename = "agent.message")]
    AgentMessage { content: Vec<ContentBlock> },
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
    #[serde(rename = "session.status_running")]
    SessionStatusRunning {},
    #[serde(rename = "session.status_idle")]
    SessionStatusIdle { stop_reason: StopReason },
    /// The session reached its irreversible terminal state (emitted when the
    /// session is archived — this server models archive as termination, stamping
    /// `archived_at` and `status: "terminated"`). A client streaming or listing
    /// the session sees this as the last event; no further turns are accepted.
    #[serde(rename = "session.status_terminated")]
    SessionStatusTerminated {},
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
            OutboundKind::AgentToolUse { .. } => "agent.tool_use",
            OutboundKind::AgentToolResult { .. } => "agent.tool_result",
            OutboundKind::AgentCustomToolUse { .. } => "agent.custom_tool_use",
            OutboundKind::AgentMcpToolUse { .. } => "agent.mcp_tool_use",
            OutboundKind::AgentMcpToolResult { .. } => "agent.mcp_tool_result",
            OutboundKind::SessionStatusRunning {} => "session.status_running",
            OutboundKind::SessionStatusIdle { .. } => "session.status_idle",
            OutboundKind::SessionStatusTerminated {} => "session.status_terminated",
            OutboundKind::SessionThreadCreated { .. } => "session.thread_created",
            OutboundKind::SessionThreadStatusRunning { .. } => "session.thread_status_running",
            OutboundKind::SessionThreadStatusIdle { .. } => "session.thread_status_idle",
            OutboundKind::SessionUpdated { .. } => "session.updated",
            OutboundKind::SessionThreadStatusTerminated { .. } => {
                "session.thread_status_terminated"
            }
            OutboundKind::AgentThreadMessageSent { .. } => "agent.thread_message_sent",
            OutboundKind::AgentThreadMessageReceived { .. } => "agent.thread_message_received",
            OutboundKind::ThreadContextCompacted { .. } => "agent.thread_context_compacted",
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
    fn agent_ref_parses_a_per_session_model_and_bare_id() {
        // Bare string id → no model (R2 optional, backward compatible).
        let bare: AgentRef = serde_json::from_str(r#""assistant""#).unwrap();
        assert_eq!(bare.id(), "assistant");
        assert_eq!(bare.model(), None);

        // Object with a model → carried as the session's model override.
        let obj: AgentRef =
            serde_json::from_str(r#"{"id":"assistant","model":"fast-model"}"#).unwrap();
        assert_eq!(obj.id(), "assistant");
        assert_eq!(obj.model(), Some("fast-model"));

        // Object without a model → None (host default).
        let no_model: AgentRef = serde_json::from_str(r#"{"id":"assistant"}"#).unwrap();
        assert_eq!(no_model.model(), None);
    }

    #[test]
    fn create_session_request_accepts_an_agent_with_model() {
        let req: CreateSessionRequest =
            serde_json::from_str(r#"{"agent":{"id":"a","model":"m2"}}"#).unwrap();
        assert_eq!(req.agent.model(), Some("m2"));
    }

    #[test]
    fn agent_ref_parses_a_runtime_selection() {
        let acp: AgentRef = serde_json::from_str(r#"{"id":"a","runtime":"acp:claude"}"#).unwrap();
        assert_eq!(acp.runtime(), Some("acp:claude"));
        // Absent → None (native, backward compatible).
        let native: AgentRef = serde_json::from_str(r#""a""#).unwrap();
        assert_eq!(native.runtime(), None);
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
}
