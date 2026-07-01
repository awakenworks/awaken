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

/// `agent` in a create-session request: either a bare id string or an object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AgentRef {
    Id(String),
    Obj {
        id: String,
        #[serde(default)]
        version: Option<u32>,
    },
}

impl AgentRef {
    pub fn id(&self) -> &str {
        match self {
            AgentRef::Id(id) => id,
            AgentRef::Obj { id, .. } => id,
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
}

/// The agent object echoed inside a session response.
#[derive(Debug, Clone, Serialize)]
pub struct SessionAgent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: u32,
    pub model: String,
    pub name: String,
    pub tools: Vec<Value>,
    pub mcp_servers: Vec<Value>,
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
    #[serde(rename = "user.pause")]
    UserPause {},
    #[serde(rename = "user.resume")]
    UserResume {},
}

impl InboundEvent {
    /// The public `type` string, echoed on the receipt.
    pub fn type_str(&self) -> &'static str {
        match self {
            InboundEvent::UserMessage { .. } => "user.message",
            InboundEvent::SystemMessage { .. } => "system.message",
            InboundEvent::UserToolConfirmation { .. } => "user.tool_confirmation",
            InboundEvent::UserCustomToolResult { .. } => "user.custom_tool_result",
            InboundEvent::UserDefineOutcome { .. } => "user.define_outcome",
            InboundEvent::UserInterrupt { .. } => "user.interrupt",
            InboundEvent::UserPause {} => "user.pause",
            InboundEvent::UserResume {} => "user.resume",
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
    #[serde(rename = "session.status_running")]
    SessionStatusRunning {},
    #[serde(rename = "session.status_idle")]
    SessionStatusIdle { stop_reason: StopReason },
    #[serde(rename = "span.outcome_evaluation_start")]
    SpanOutcomeEvaluationStart { outcome_id: String, iteration: u32 },
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
            OutboundKind::SessionStatusRunning {} => "session.status_running",
            OutboundKind::SessionStatusIdle { .. } => "session.status_idle",
            OutboundKind::SpanOutcomeEvaluationStart { .. } => "span.outcome_evaluation_start",
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
