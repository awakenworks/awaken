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

mod content;
pub(crate) use content::project_advisor_thread_message_content;
mod agent_ref;
pub use agent_ref::{AgentRef, AgentRefObject, ModelOverride};
mod model_config;
pub use model_config::{
    ModelConfig, ModelConfigParams, ModelEffort, ModelEffortInput, ModelEffortLevel,
    ModelInferenceGeo, ModelSpeed,
};

use awaken_agent_contract::agent::content::ContentBlock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonetaryAmount {
    pub amount: String,
    pub currency: Currency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Currency {
    USD,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BudgetLimit {
    Limit { max_list_cost: MonetaryAmount },
}

impl BudgetLimit {
    pub fn max_list_cost_minor(&self) -> Result<u64, String> {
        let Self::Limit { max_list_cost } = self;
        if max_list_cost.amount.is_empty()
            || (max_list_cost.amount.len() > 1 && max_list_cost.amount.starts_with('0'))
            || !max_list_cost
                .amount
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            return Err(
                "max_list_cost.amount must be a canonical non-negative integer string".into(),
            );
        }
        let amount = max_list_cost
            .amount
            .parse::<u64>()
            .map_err(|_| "max_list_cost.amount exceeds the supported range".to_owned())?;
        if amount == 0 {
            return Err("max_list_cost.amount must be greater than zero".into());
        }
        Ok(amount)
    }

    pub fn from_minor(amount: u64) -> Self {
        Self::Limit {
            max_list_cost: MonetaryAmount {
                amount: amount.to_string(),
                currency: Currency::USD,
            },
        }
    }
}
use serde_json::Value;

use awaken_session_contract::{AgentTool, validate_agent_tools};

use super::agent::{AgentMcpServer, AgentSkill};
use super::resource::{ResourceInput, SessionResource};

/// The Anthropic error envelope: `{ "type": "error", "error": { "type", "message" } }`.
/// The SDK parses this shape to populate `err.error.type` / `err.error.message`;
/// `error.type` is the status-keyed discriminator (`not_found_error`,
/// `invalid_request_error`, `api_error`, …).
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub error: ApiError,
    pub request_id: Option<String>,
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
            request_id: None,
        }
    }
}

/// `POST /v1/sessions` request body. The boundary is deliberately closed: an
/// unsupported extension must fail instead of being silently ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateParams {
    pub agent: AgentRef,
    #[serde(default)]
    pub budget: Option<BudgetLimit>,
    /// Events admitted atomically with Session creation. The state layer validates
    /// the entire collection before minting an id, then drives them through the
    /// same event command used by `POST .../events`.
    #[serde(default)]
    pub initial_events: Vec<InboundEvent>,
    pub environment_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
    /// Vaults whose credentials the session may use (matched to `mcp_servers`
    /// by exact `mcp_server_url`).
    #[serde(default)]
    pub vault_ids: Vec<String>,
    /// Mounted resources (ADR-0038), decoded as the SDK's tagged union before the
    /// state layer lowers them into neutral input bindings.
    #[serde(default)]
    pub resources: Vec<ResourceInput>,
}

impl SessionCreateParams {
    /// Build the smallest valid official SDK request without assembling a JSON
    /// object. Optional Managed fields remain absent/empty until explicitly set.
    #[must_use]
    pub fn new(agent: impl Into<String>, environment_id: impl Into<String>) -> Self {
        Self {
            agent: AgentRef::Id(agent.into()),
            budget: None,
            initial_events: Vec::new(),
            environment_id: environment_id.into(),
            title: None,
            metadata: std::collections::BTreeMap::new(),
            vault_ids: Vec::new(),
            resources: Vec::new(),
        }
    }

    pub fn validate_common(&self) -> Result<(), String> {
        self.agent.validate_sdk_limits()?;
        if self.metadata.len() > 16
            || self
                .metadata
                .iter()
                .any(|(key, value)| key.chars().count() > 64 || value.chars().count() > 512)
        {
            return Err(
                "metadata supports at most 16 pairs with 64-character keys and 512-character values"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn validate_public(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.initial_events.iter().any(|event| {
            !matches!(
                event,
                InboundEvent::UserMessage { .. } | InboundEvent::UserDefineOutcome { .. }
            )
        }) {
            return Err("initial_events accepts only user.message and user.define_outcome".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAgentUpdate {
    #[serde(default)]
    pub tools: Option<Vec<AgentTool>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<AgentMcpServer>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateParams {
    #[serde(default)]
    pub agent: Option<SessionAgentUpdate>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub budget: Option<Option<BudgetLimit>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub title: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub metadata: Option<Option<std::collections::BTreeMap<String, Option<String>>>>,
    /// Reserved by the SDK but not supported by the product yet. The route rejects
    /// a present value before any state mutation.
    #[serde(default)]
    pub vault_ids: Option<Vec<String>>,
}

impl SessionUpdateParams {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(tools) = self.agent.as_ref().and_then(|agent| agent.tools.as_ref()) {
            validate_agent_tools(tools)?;
        }
        Ok(())
    }
}

/// Session requests reuse the exact Agent URL-MCP DTO; there is one Managed MCP
/// vocabulary and one serializer/deserializer authority.
pub type McpServer = super::agent::AgentMcpServer;

/// The agent object echoed inside a session response
/// (`BetaManagedAgentsSessionAgent`).
#[derive(Debug, Clone, Serialize)]
pub struct SessionAgent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: u64,
    pub model: ModelConfig,
    pub name: String,
    /// SDK-required (nullable) fields; emitted as `null` when the host has none.
    pub description: Option<String>,
    pub system: Option<String>,
    pub tools: Vec<AgentTool>,
    pub mcp_servers: Vec<AgentMcpServer>,
    pub skills: Vec<AgentSkill>,
    /// SDK-required nullable coordinator roster; `null` means no delegation.
    pub multiagent: Option<SessionMultiagentCoordinator>,
}

/// The Session response freezes full child Agent definitions, unlike the Agent
/// authoring response whose coordinator roster contains versioned references.
#[derive(Debug, Clone, Serialize)]
pub struct SessionMultiagentCoordinator {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub agents: Vec<SessionMultiagentRosterEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SessionMultiagentRosterEntry {
    Agent(SessionThreadAgent),
    Advisor(super::agent::AdvisorRosterEntry),
}

impl SessionMultiagentRosterEntry {
    /// Returns an executable ordinary child Agent. Advisor execution is a real
    /// Thread too, but retains its distinct two-field wire identity.
    pub(crate) const fn as_agent(&self) -> Option<&SessionThreadAgent> {
        match self {
            Self::Agent(agent) => Some(agent),
            Self::Advisor(_) => None,
        }
    }

    pub(crate) const fn as_advisor(&self) -> Option<&super::agent::AdvisorRosterEntry> {
        match self {
            Self::Agent(_) => None,
            Self::Advisor(advisor) => Some(advisor),
        }
    }
}

/// `BetaManagedAgentsSessionThreadAgent` — the agent snapshot frozen for one
/// execution thread. The coordinator roster belongs only to [`SessionAgent`] and
/// is deliberately absent from this projection, matching the official contract.
#[derive(Debug, Clone, Serialize)]
pub struct SessionThreadAgent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub version: u64,
    pub model: ModelConfig,
    pub name: String,
    pub description: Option<String>,
    pub system: Option<String>,
    pub tools: Vec<AgentTool>,
    pub mcp_servers: Vec<AgentMcpServer>,
    pub skills: Vec<AgentSkill>,
}

impl From<&SessionAgent> for SessionThreadAgent {
    fn from(agent: &SessionAgent) -> Self {
        Self {
            id: agent.id.clone(),
            kind: "agent",
            version: agent.version,
            model: agent.model.clone(),
            name: agent.name.clone(),
            description: agent.description.clone(),
            system: agent.system.clone(),
            tools: agent.tools.clone(),
            mcp_servers: agent.mcp_servers.clone(),
            skills: agent.skills.clone(),
        }
    }
}

/// Official `session_thread.agent` union. Ordinary Agent Threads retain the
/// complete frozen execution snapshot; advisor consultations use only the
/// two-field advisor identity and must never be padded with fabricated Agent
/// fields.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SessionThreadAgentValue {
    Agent(SessionThreadAgent),
    Advisor(super::agent::AdvisorRosterEntry),
}

impl SessionThreadAgentValue {
    #[must_use]
    pub const fn as_agent(&self) -> Option<&SessionThreadAgent> {
        match self {
            Self::Agent(agent) => Some(agent),
            Self::Advisor(_) => None,
        }
    }

    #[must_use]
    pub const fn is_advisor(&self) -> bool {
        matches!(self, Self::Advisor(_))
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            Self::Agent(agent) => &agent.name,
            // Managed reserves this lifecycle/cross-post name independently of
            // the advisor model carried by the two-field Thread agent object.
            Self::Advisor(_) => "anthropic.advisor",
        }
    }
}

impl From<SessionThreadAgent> for SessionThreadAgentValue {
    fn from(agent: SessionThreadAgent) -> Self {
        Self::Agent(agent)
    }
}

impl From<super::agent::AdvisorRosterEntry> for SessionThreadAgentValue {
    fn from(advisor: super::agent::AdvisorRosterEntry) -> Self {
        Self::Advisor(advisor)
    }
}

/// `BetaManagedAgentsSessionThreadStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionThreadStatus {
    Running,
    Idle,
    Rescheduling,
    Terminated,
}

/// `BetaManagedAgentsSessionThreadStats`. Values are optional because a worker
/// may not expose timing telemetry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionThreadStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_seconds: Option<f64>,
}

/// `BetaManagedAgentsSessionThreadUsage`. The thread view stays `null` until
/// committed per-thread accounting exists; its optional list cost is derived
/// from the Session's one frozen pricing snapshot when present.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionThreadUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<SessionThreadCacheCreationUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_cost: Option<MonetaryAmount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
}

/// `BetaManagedAgentsCacheCreationUsage`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionThreadCacheCreationUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u64>,
}

/// `BetaManagedAgentsSessionThread` — one primary or delegated execution thread.
#[derive(Debug, Clone, Serialize)]
pub struct SessionThread {
    pub id: String,
    pub agent: SessionThreadAgentValue,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub parent_thread_id: Option<String>,
    pub session_id: String,
    pub stats: Option<SessionThreadStats>,
    pub status: SessionThreadStatus,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub updated_at: String,
    pub usage: Option<SessionThreadUsage>,
}

/// `BetaManagedAgentsSessionStats` — coarse per-session timing/counters. Empty on
/// this surface (serializes as `{}`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionStats {}

/// `BetaManagedAgentsServerToolUsage`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServerToolUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_fetch_requests: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_requests: Option<u64>,
}

/// `BetaManagedAgentsSessionUsage` — cumulative priced quantities.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<SessionThreadCacheCreationUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_cost: Option<MonetaryAmount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUsage>,
}

/// BetaManagedAgentsSpanModelUsage: required token counters for one request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SpanModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// Inference speed mode (`standard`/`fast`); omitted when the model reports none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

/// `BetaManagedAgentsSession` response (minimal but SDK-parseable).
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Rescheduling,
    #[default]
    Idle,
    Terminated,
}

impl SessionStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Rescheduling => "rescheduling",
            Self::Idle => "idle",
            Self::Terminated => "terminated",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub agent: SessionAgent,
    pub budget: Option<BudgetLimit>,
    pub environment_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    pub title: Option<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub resources: Vec<SessionResource>,
    pub outcome_evaluations: Vec<OutcomeEvaluation>,
    pub status: SessionStatus,
    pub stats: SessionStats,
    pub usage: Usage,
    /// The vaults the session is bound to (`vault_ids`).
    pub vault_ids: Vec<String>,
    /// Set when the session was launched by a deployment run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
}

/// `BetaManagedAgentsDeletedSession`.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedSession {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutcomeEvaluation {
    pub completed_at: Option<String>,
    pub description: String,
    pub explanation: Option<String>,
    pub iteration: u32,
    pub outcome_id: String,
    pub result: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// A client's `user.tool_confirmation` decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmResult {
    Allow,
    Deny,
}

/// The SDK's closed permission decision attached to Agent tool-use events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatedPermission {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutcomeRubric {
    Text { content: String },
    File { file_id: String },
}

/// Inbound events a client posts to `POST /v1/sessions/{id}/events`. M1 acts on
/// `user.message`; the rest deserialize (so the batch is accepted) and are
/// wired in later milestones.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum InboundEvent {
    #[serde(rename = "user.message")]
    UserMessage { content: Vec<ContentBlock> },
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
    /// `user.tool_result`. It is deliberately distinct from
    /// `user.custom_tool_result`: the latter may answer only a projected
    /// `agent.custom_tool_use`, while this family may answer only a projected
    /// self-hosted `agent.tool_use`.
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
        rubric: OutcomeRubric,
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

impl super::initial_event::InitialEventSpec for InboundEvent {
    fn initial_event_class(&self) -> super::initial_event::InitialEventClass {
        use super::initial_event::InitialEventClass;
        match self {
            InboundEvent::UserMessage { .. } => InitialEventClass::UserMessage,
            InboundEvent::SystemMessage { .. } => InitialEventClass::SystemMessage,
            InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => InitialEventClass::UserDefineOutcome {
                max_iterations: *max_iterations,
                description_nonempty: !description.trim().is_empty(),
                rubric_nonempty: match rubric {
                    OutcomeRubric::Text { content } => !content.trim().is_empty(),
                    OutcomeRubric::File { file_id } => !file_id.trim().is_empty(),
                },
            },
            other => InitialEventClass::Other(other.type_str()),
        }
    }
}

impl SessionCreateParams {
    pub(crate) fn validate_initial_events(&self) -> Result<(), String> {
        super::initial_event::validate_initial_events(
            &self.initial_events,
            &super::initial_event::InitialEventPolicy {
                min_count: 0,
                max_count: 50,
                allow_system_message: false,
                require_final_system_after_user: false,
                max_outcomes: Some(1),
                outcome_iterations: Some(1..=20),
            },
        )?;
        let file_documents = self
            .initial_events
            .iter()
            .map(InboundEvent::validate_content)
            .try_fold(0usize, |total, count| {
                count.and_then(|count| {
                    total
                        .checked_add(count)
                        .ok_or_else(|| "too many file-sourced documents".to_string())
                })
            })?;
        if file_documents > 100 {
            return Err("initial_events supports at most 100 file-sourced document blocks".into());
        }
        Ok(())
    }
}

/// `POST .../events` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendEventsRequest {
    pub events: Vec<InboundEvent>,
}

/// `POST .../events` response.
#[derive(Debug, Clone, Serialize)]
pub struct SendEventsResponse {
    /// The exact accepted inbound Events. This intentionally reuses the same
    /// public Event DTO returned by list/stream so receipt and history fields
    /// cannot drift into two wire implementations.
    pub data: Vec<Event>,
}

/// Why a session went idle — a tagged object, never a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    BudgetReached,
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
/// `code` selects the variant; the Session aggregate's terminal
/// `activation_failed` classification remains an `unknown_error` but is not
/// retryable. An unrecognized code honestly falls back to `unknown_error`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionError {
    /// The SDK error type, e.g. `"unknown_error"` or `"model_rate_limited_error"`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: String,
    pub retry_status: RetryStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_name: Option<String>,
}

impl SessionError {
    /// Classify a neutral runtime fault `(code, message)` into an SDK error object.
    /// The `code` is the stable snake_case class from `Failure::Inference`
    /// (`rate_limited` / `context_overflow` / `unauthorized` / …), or the Session
    /// aggregate's `activation_failed` state; it picks the error `type` and the
    /// `retry_status`. An unknown code (or a plain internal fault) is the catch-all
    /// `unknown_error` / `exhausted` — the session stays usable, so we don't
    /// force-terminate on one failed Run.
    pub fn classify(code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let mcp_server_name = code_message_server_name(code, &message);
        let (kind, retry_status) = match code {
            "mcp_connection_failed" => ("mcp_connection_failed_error", RetryStatus::Retrying),
            "mcp_authentication_failed" => {
                ("mcp_authentication_failed_error", RetryStatus::Terminal)
            }
            "rate_limited" => ("model_rate_limited_error", RetryStatus::Exhausted),
            "context_overflow" | "unauthorized" => {
                ("model_request_failed_error", RetryStatus::Terminal)
            }
            "activation_failed" => ("unknown_error", RetryStatus::Terminal),
            _ => ("unknown_error", RetryStatus::Exhausted),
        };
        Self {
            kind,
            message,
            retry_status,
            mcp_server_name,
        }
    }
}

fn code_message_server_name(code: &str, message: &str) -> Option<String> {
    if !code.starts_with("mcp_") {
        return None;
    }
    let marker = "mcp server `";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    Some(rest.split('`').next()?.to_string())
}

/// The payload of an outbound event (its `type` plus kind-specific fields).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
#[allow(
    clippy::large_enum_variant,
    reason = "the enum mirrors the fixed Managed Agents event wire; boxing one variant would add an internal ownership shape without changing the serialized protocol"
)]
pub enum OutboundKind {
    #[serde(rename = "user.message")]
    UserMessage { content: Vec<ContentBlock> },
    #[serde(rename = "system.message")]
    SystemMessage { content: Vec<ContentBlock> },
    #[serde(rename = "user.tool_confirmation")]
    UserToolConfirmation {
        tool_use_id: String,
        result: ConfirmResult,
        #[serde(skip_serializing_if = "Option::is_none")]
        deny_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
    },
    #[serde(rename = "user.custom_tool_result")]
    UserCustomToolResult {
        custom_tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
    },
    #[serde(rename = "user.tool_result")]
    UserToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
    },
    #[serde(rename = "user.define_outcome")]
    UserDefineOutcome {
        description: String,
        rubric: OutcomeRubric,
        max_iterations: Option<u32>,
        outcome_id: String,
    },
    #[serde(rename = "user.interrupt")]
    UserInterrupt {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
    },
    #[serde(rename = "agent.message")]
    AgentMessage { content: Vec<ContentBlock> },
    /// The agent's extended-thinking block (`agent.thinking`) — `{id, type,
    /// processed_at}`, no payload, mirroring the SDK's
    /// `BetaManagedAgentsAgentThinkingEvent`. Provider reasoning is projected
    /// as this contentless durable marker; reasoning text never crosses the
    /// public Managed wire.
    #[serde(rename = "agent.thinking")]
    AgentThinking {},
    #[serde(rename = "agent.tool_use")]
    AgentToolUse {
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        evaluated_permission: Option<EvaluatedPermission>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
    },
    #[serde(rename = "agent.tool_result")]
    AgentToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(rename = "agent.custom_tool_use")]
    AgentCustomToolUse {
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
    },
    /// An MCP tool call (`agent.mcp_tool_use`): a host-executed tool from an MCP
    /// server, distinguished from a built-in `agent.tool_use` by the `mcp__` name.
    #[serde(rename = "agent.mcp_tool_use")]
    AgentMcpToolUse {
        name: String,
        mcp_server_name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        evaluated_permission: Option<EvaluatedPermission>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_thread_id: Option<String>,
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
    /// Periodic point-in-time snapshot of cumulative Session usage and its
    /// optional hard budget, matching `BetaManagedAgentsSessionUsageEvent`.
    #[serde(rename = "session.usage")]
    SessionUsage {
        usage: Usage,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget: Option<BudgetLimit>,
    },
    /// The session is recovering from a transient error and is rescheduled for
    /// execution (`session.status_rescheduled`) — `{id, type, processed_at}`, no
    /// payload. Retryable inference errors emit this durable transition before
    /// the attempt is rescheduled, so list and reconnect clients observe the
    /// same recovery state.
    #[serde(rename = "session.status_rescheduled")]
    SessionStatusRescheduled {},
    /// The session reached its irreversible terminal state (emitted when the
    /// session is archived — this server models archive as termination, stamping
    /// `archived_at` and `status: "terminated"`). A client streaming or listing
    /// the session sees this as the last event; no further Runs are accepted.
    #[serde(rename = "session.status_terminated")]
    SessionStatusTerminated {},
    /// The session was deleted (`session.deleted`) — a terminal stream frame
    /// pushed to any open SSE connection just before the record is dropped.
    /// Unlike archive (which tombstones a still-listable `terminated` record),
    /// delete removes the session, so this frame is live-broadcast only: a
    /// subsequent `events.list`/`retrieve` is a 404, not a replay.
    #[serde(rename = "session.deleted")]
    SessionDeleted {},
    /// A Session Thread was created within the Session — the SDK's
    /// `session.thread_created`. `agent_name` identifies the Agent that the
    /// Thread runs.
    #[serde(rename = "session.thread_created")]
    SessionThreadCreated {
        session_thread_id: String,
        agent_name: String,
    },
    /// A Session Thread started running (`session.thread_status_running`).
    #[serde(rename = "session.thread_status_running")]
    SessionThreadStatusRunning {
        session_thread_id: String,
        agent_name: String,
    },
    /// A Session Thread went idle (`session.thread_status_idle`), carrying
    /// the same `stop_reason` shape as the session's own idle.
    #[serde(rename = "session.thread_status_idle")]
    SessionThreadStatusIdle {
        session_thread_id: String,
        agent_name: String,
        stop_reason: StopReason,
    },
    /// A Session Thread hit a transient error and is retrying
    /// (`session.thread_status_rescheduled`) — same identity shape as the other
    /// thread-status events.
    #[serde(rename = "session.thread_status_rescheduled")]
    SessionThreadStatusRescheduled {
        session_thread_id: String,
        agent_name: String,
    },
    /// The session's `metadata`/`title` changed (`session.updated`), carrying the
    /// title (including `null`) only when it changed and the full metadata bag
    /// only when it changed to a non-empty value.
    #[serde(rename = "session.updated")]
    SessionUpdated {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<Option<String>>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        metadata: std::collections::BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<SessionAgent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget: Option<Option<BudgetLimit>>,
    },
    /// A Session Thread terminated (`session.thread_status_terminated`),
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
        #[serde(skip_serializing_if = "Option::is_none")]
        to_agent_name: Option<String>,
        content: Vec<ContentBlock>,
    },
    /// The coordinator received the delegate's reply (`agent.thread_message_received`).
    #[serde(rename = "agent.thread_message_received")]
    AgentThreadMessageReceived {
        from_session_thread_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        from_agent_name: Option<String>,
        content: Vec<ContentBlock>,
    },
    /// The conversation history was summarized to fit context (the compact plugin
    /// folded older Steps). A pure marker: its shape matches the installed SDK's
    /// `BetaManagedAgentsAgentThreadContextCompactedEvent` — `{id, type,
    /// processed_at}`, no payload (aligned to `@anthropic-ai/sdk`, not guessed).
    #[serde(rename = "agent.thread_context_compacted")]
    ThreadContextCompacted {},
    /// A model request was initiated (`span.model_request_start`) — `{id, type,
    /// processed_at}`, no payload; its `id` is referenced by the paired
    /// `span.model_request_end`.
    #[serde(rename = "span.model_request_start")]
    SpanModelRequestStart {},
    /// A model request completed (`span.model_request_end`), carrying the paired
    /// start id, a nullable error flag, and this single request's token usage.
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
            OutboundKind::UserMessage { .. } => "user.message",
            OutboundKind::SystemMessage { .. } => "system.message",
            OutboundKind::UserToolConfirmation { .. } => "user.tool_confirmation",
            OutboundKind::UserCustomToolResult { .. } => "user.custom_tool_result",
            OutboundKind::UserToolResult { .. } => "user.tool_result",
            OutboundKind::UserDefineOutcome { .. } => "user.define_outcome",
            OutboundKind::UserInterrupt { .. } => "user.interrupt",
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
            OutboundKind::SessionUsage { .. } => "session.usage",
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

#[cfg(test)]
impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        // The wire payload is the public equality contract exercised by warm,
        // cold, and paginated projection tests. Keep test comparison tied to
        // that canonical serialization instead of duplicating every variant.
        serde_json::to_value(self).expect("event serializes")
            == serde_json::to_value(other).expect("event serializes")
    }
}

impl Event {
    /// The public event `type`, used as the SSE `event:` field.
    pub fn type_str(&self) -> &'static str {
        self.kind.type_str()
    }
}

/// A stream-only *live preview* frame (`event_start` / `event_delta`), emitted on
/// the SSE stream **only** while a Run is in flight and **never** persisted in
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
/// carries text deltas; tool use, tool results, and thinking are never previewed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum PreviewContent {
    #[serde(rename = "text")]
    Text { text: String },
}

/// An SSE serialization envelope: either a committed [`Event`] from the Session
/// broadcast or a connection-local [`PreviewFrame`] from the Runtime-owned Thread
/// subscription. Preview frames never re-enter the committed broadcast and are
/// forwarded only when the connection opted in via `event_deltas[]`.
#[derive(Debug, Clone)]
#[allow(
    clippy::large_enum_variant,
    reason = "committed and preview frames intentionally share one SSE envelope; the size difference is bounded and boxing would only add allocation"
)]
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
}

#[cfg(test)]
impl PartialEq for ListEventsResponse {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data && self.next_page == other.next_page
    }
}

#[cfg(test)]
mod tests {
    use awaken_runtime_contract::agent_bindings::{InferenceGeography, InferenceOptions};

    use super::*;

    #[test]
    fn model_geo_projection_exposes_only_the_official_boundary() {
        // Cause/effect graph: C1=neutral US, C2=neutral Provider-only region,
        // C3=wire global, C4=wire US. Effects: E1=typed official US; E2=Provider
        // detail stays absent; E3=no neutral restriction; E4=neutral US.
        // Decision rules: R1(C1)->E1; R2(C2)->E2; R3(C3)->E3; R4(C4)->E4.
        let us = ModelConfig::from_inference(
            "model",
            InferenceOptions {
                inference_geo: Some(InferenceGeography::Us),
                ..Default::default()
            },
        );
        assert_eq!(us.inference_geo, Some(ModelInferenceGeo::Us), "R1");

        let eu = ModelConfig::from_inference(
            "model",
            InferenceOptions {
                inference_geo: Some(InferenceGeography::Eu),
                ..Default::default()
            },
        );
        assert!(eu.inference_geo.is_none(), "R2");

        let mut wire = ModelConfig::new("model");
        wire.inference_geo = Some(ModelInferenceGeo::Global);
        assert!(wire.inference_options().inference_geo.is_none(), "R3");
        wire.inference_geo = Some(ModelInferenceGeo::Us);
        assert_eq!(
            wire.inference_options().inference_geo,
            Some(InferenceGeography::Us),
            "R4"
        );
    }

    /// Causal graph: rubric object tag + variant payload -> one closed domain
    /// variant; a scalar, unknown tag, or wrong payload -> boundary rejection.
    ///
    /// Decision table:
    /// | shape  | tag     | required field | effect              |
    /// |--------|---------|----------------|---------------------|
    /// | object | text    | content        | Text accepted       |
    /// | object | file    | file_id        | File accepted       |
    /// | scalar | -       | -              | rejected            |
    /// | object | unknown | -              | rejected            |
    #[test]
    fn outcome_rubric_wire_follows_the_closed_union_decision_table() {
        let text: InboundEvent = serde_json::from_value(serde_json::json!({
            "type": "user.define_outcome",
            "description": "finish",
            "rubric": { "type": "text", "content": "FINAL" }
        }))
        .unwrap();
        assert!(matches!(
            text,
            InboundEvent::UserDefineOutcome {
                rubric: OutcomeRubric::Text { ref content },
                ..
            } if content == "FINAL"
        ));

        let file: InboundEvent = serde_json::from_value(serde_json::json!({
            "type": "user.define_outcome",
            "description": "finish",
            "rubric": { "type": "file", "file_id": "file_1" }
        }))
        .unwrap();
        assert!(matches!(
            file,
            InboundEvent::UserDefineOutcome {
                rubric: OutcomeRubric::File { ref file_id },
                ..
            } if file_id == "file_1"
        ));

        for invalid in [
            serde_json::json!({
                "type": "user.define_outcome",
                "description": "finish",
                "rubric": "FINAL"
            }),
            serde_json::json!({
                "type": "user.define_outcome",
                "description": "finish",
                "rubric": { "type": "url", "url": "https://example.test/rubric" }
            }),
        ] {
            assert!(serde_json::from_value::<InboundEvent>(invalid).is_err());
        }
    }

    #[test]
    fn agent_ref_parses_a_bare_id_and_a_tagged_object() {
        // Cause/effect decision table:
        // | wire form                          | result                    |
        // | string                             | latest Agent reference    |
        // | object + `type: agent`             | versioned Agent reference |
        // | object missing/unknown discriminator | reject                 |
        // | plain Agent object with override   | reject                    |
        let bare: AgentRef = serde_json::from_str(r#""assistant""#).unwrap();
        assert_eq!(bare.id(), "assistant");
        let obj: AgentRef =
            serde_json::from_str(r#"{"id":"assistant","type":"agent","version":3}"#).unwrap();
        assert_eq!(obj.id(), "assistant");
        assert_eq!(obj.version(), Some(3));
        // A plain reference never overrides the model.
        assert!(matches!(bare.model_override(), ModelOverride::Absent));
        assert!(matches!(obj.model_override(), ModelOverride::Absent));
        for invalid in [
            r#"{"id":"a"}"#,
            r#"{"id":"a","type":"future_kind"}"#,
            r#"{"id":"a","type":"agent","model":"x"}"#,
        ] {
            assert!(serde_json::from_str::<AgentRef>(invalid).is_err());
        }
    }

    #[test]
    fn agent_with_overrides_replaces_the_model_for_the_session() {
        // Cause/effect decision table: string model -> typed default controls;
        // object + official geography -> typed geography; object + any unknown
        // geography literal -> serde rejection before state construction.
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
        // `{id, speed, inference_geo}` object model override, with a pinned version.
        let o: AgentRef = serde_json::from_str(
            r#"{"id":"assistant","type":"agent_with_overrides","version":2,"model":{"id":"claude-opus-4-8","speed":"fast","inference_geo":"us"}}"#,
        )
        .unwrap();
        assert_eq!(o.version(), Some(2));
        match o.model_override() {
            ModelOverride::Set(cfg) => {
                assert_eq!(cfg.id, "claude-opus-4-8");
                assert_eq!(cfg.speed, Some(ModelSpeed::Fast));
                assert_eq!(cfg.inference_geo, Some(ModelInferenceGeo::Us));
            }
            other => panic!("expected Set, got {other:?}"),
        }
        assert!(
            serde_json::from_str::<AgentRef>(
                r#"{"id":"assistant","type":"agent_with_overrides","model":{"id":"claude-opus-4-8","inference_geo":"eu"}}"#,
            )
            .is_err(),
            "an unsupported closed-enum literal never reaches model resolution"
        );
    }

    #[test]
    fn public_session_status_is_the_exact_sdk_union() {
        // Cause/effect decision table: each public variant serializes to one SDK
        // literal; internal preparation/failure phases have no representable
        // variant and therefore cannot leak through this type.
        let values = [
            (SessionStatus::Rescheduling, "rescheduling"),
            (SessionStatus::Running, "running"),
            (SessionStatus::Idle, "idle"),
            (SessionStatus::Terminated, "terminated"),
        ];
        assert_eq!(values.len(), 4);
        for (status, expected) in values {
            assert_eq!(serde_json::to_value(status).unwrap(), expected);
        }
    }

    #[test]
    fn model_override_is_omitted_or_a_non_null_sdk_model() {
        // Cause/effect decision table:
        // | model field | DTO effect |
        // | omitted     | inherit    |
        // | valid value | replace    |
        // | null        | reject     |
        let absent: AgentRef =
            serde_json::from_str(r#"{"id":"a","type":"agent_with_overrides"}"#).unwrap();
        assert!(matches!(absent.model_override(), ModelOverride::Absent));
        assert!(
            serde_json::from_str::<AgentRef>(
                r#"{"id":"a","type":"agent_with_overrides","model":null}"#
            )
            .is_err()
        );
    }

    #[test]
    fn agent_overrides_reuse_the_typed_agent_composite_contract() {
        // Causal graph:
        // known SDK discriminator + exact fields -> typed override -> normalization
        // unknown discriminator / misspelled field -> admission error -> no Session
        //
        // Decision table:
        // | tool/skill/MCP shape                    | admission |
        // | all known tags and exact fields         | accept    |
        // | unknown tag                             | reject    |
        // | known tag with misspelled required key  | reject    |
        let valid = serde_json::json!({
            "id": "assistant",
            "type": "agent_with_overrides",
            "mcp_servers": [{"type":"url", "name":"docs", "url":"https://mcp.test"}],
            "tools": [{"type":"mcp_toolset", "mcp_server_name":"docs"}],
            "skills": [{"type":"custom", "skill_id":"skill_1", "version":"2"}]
        });
        let parsed: AgentRef = serde_json::from_value(valid).expect("typed composites parse");
        assert!(matches!(
            parsed.tools_override(),
            Some(tools) if matches!(tools[0], AgentTool::McpToolset { .. })
        ));

        for invalid in [
            serde_json::json!({
                "id":"a", "type":"agent_with_overrides",
                "skills":[{"type":"unknown", "skill_id":"s"}]
            }),
            serde_json::json!({
                "id":"a", "type":"agent_with_overrides",
                "mcp_servers":[{"type":"url", "name":"s", "uri":"https://mcp.test"}]
            }),
            serde_json::json!({
                "id":"a", "type":"agent_with_overrides",
                "tools":[{"type":"mcp_toolset", "mcp_server":"s"}]
            }),
        ] {
            assert!(serde_json::from_value::<AgentRef>(invalid).is_err());
        }
    }

    #[test]
    fn session_tool_overrides_reject_unknown_members_before_projection() {
        // Cause/effect graph: C1 create/update omits tool overrides; C2 it uses a
        // closed Agent-tool member; C3 it names an unknown member. Effects:
        // E1 inherit/unchanged, E2 canonical normalization, E3 admission error
        // before Session mutation. Decision rows: absent=>E1; known=>E2;
        // unknown on create or update=>E3.
        // Constraint: both Session ingress paths reuse `validate_agent_tools`;
        // neither may silently drop or retain a parallel Agent-tool member.
        let create: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent": {
                "id": "assistant",
                "type": "agent_with_overrides",
                "tools": [{
                    "type": "agent_toolset_20260401",
                    "configs": [{"name": "parallel_web_search"}]
                }]
            },
            "environment_id": "env"
        }))
        .expect("typed request retains the value until semantic validation");
        assert_eq!(
            create.validate_common().unwrap_err(),
            "unknown agent tool `parallel_web_search`",
            "C3/create=>E3"
        );

        let update: SessionUpdateParams = serde_json::from_value(serde_json::json!({
            "agent": {
                "tools": [{
                    "type": "agent_toolset_20260401",
                    "configs": [{"name": "parallel_web_search"}]
                }]
            }
        }))
        .expect("typed update retains the value until semantic validation");
        assert_eq!(
            update.validate().unwrap_err(),
            "unknown agent tool `parallel_web_search`",
            "C3/update=>E3"
        );
        assert!(
            serde_json::from_value::<SessionUpdateParams>(serde_json::json!({}))
                .expect("absent override")
                .validate()
                .is_ok(),
            "C1=>E1"
        );
    }

    #[test]
    fn session_create_requires_environment_and_rejects_non_sdk_mcp_field() {
        // Cause/effect decision table:
        // | environment_id | top-level mcp_servers | result |
        // | present        | absent                | typed construction/accept |
        // | absent         | absent                | reject |
        // | present        | present               | reject |
        let parsed = SessionCreateParams::new("assistant", "environment_1");
        assert_eq!(parsed.environment_id, "environment_1");
        assert_eq!(parsed.agent.id(), "assistant");
        for invalid in [
            serde_json::json!({"agent":"assistant"}),
            serde_json::json!({
                "agent":"assistant",
                "environment_id":"environment_1",
                "mcp_servers":[]
            }),
        ] {
            assert!(serde_json::from_value::<SessionCreateParams>(invalid).is_err());
        }
    }

    #[test]
    fn session_create_validation_enforces_sdk_boundaries_before_state_mutation() {
        // Cause/effect graph: C1 version is zero; C2 an override exceeds an SDK
        // collection/string limit; C3 metadata exceeds its pair/key/value limit;
        // C4 a create-only event uses a non-create event variant. Each cause must
        // produce E1 invalid-request admission and E2 no state-layer mutation.
        //
        // Decision table: R1 !C1..!C4 -> accept; R2 C1 -> E1+E2; R3 C2 -> E1+E2;
        // R4 C3 -> E1+E2; R5 C4 -> E1+E2. The shared initial-event validator owns
        // the independent 50-event/outcome-cardinality rules.
        let valid: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent": {"id":"assistant", "type":"agent", "version":1},
            "environment_id":"environment_1",
            "metadata":{"key":"value"},
            "initial_events":[{
                "type":"user.message", "content":[{"type":"text", "text":"hello"}]
            }]
        }))
        .unwrap();
        assert!(valid.validate_public().is_ok(), "R1");

        let zero_version: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent":{"id":"assistant", "type":"agent", "version":0},
            "environment_id":"environment_1"
        }))
        .unwrap();
        assert!(zero_version.validate_public().is_err(), "R2");

        let too_many_tools: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent":{
                "id":"assistant", "type":"agent_with_overrides",
                "tools": (0..129).map(|index| serde_json::json!({
                    "type":"custom", "name":format!("tool_{index}"),
                    "description":"tool", "input_schema":{"type":"object"}
                })).collect::<Vec<_>>()
            },
            "environment_id":"environment_1"
        }))
        .unwrap();
        assert!(too_many_tools.validate_public().is_err(), "R3");

        let too_many_metadata: SessionCreateParams = serde_json::from_value(serde_json::json!({
            "agent":"assistant", "environment_id":"environment_1",
            "metadata": (0..17).map(|index| (format!("key_{index}"), "value"))
                .collect::<std::collections::BTreeMap<_, _>>()
        }))
        .unwrap();
        assert!(too_many_metadata.validate_public().is_err(), "R4");

        let unsupported_initial_event: SessionCreateParams =
            serde_json::from_value(serde_json::json!({
                "agent":"assistant", "environment_id":"environment_1",
                "initial_events":[{"type":"user.interrupt"}]
            }))
            .unwrap();
        assert!(unsupported_initial_event.validate_public().is_err(), "R5");
    }

    #[test]
    fn user_message_rejects_non_sdk_routing_fields() {
        // Cause/effect decision table: the SDK content-only message is admitted;
        // historical per-event `model` and `session_thread_id` fields are rejected
        // before any event is appended.
        let valid: InboundEvent = serde_json::from_str(
            r#"{"type":"user.message","content":[{"type":"text","text":"hi"}]}"#,
        )
        .unwrap();
        assert!(matches!(valid, InboundEvent::UserMessage { .. }));
        for invalid in [
            r#"{"type":"user.message","content":[{"type":"text","text":"hi"}],"model":"fast"}"#,
            r#"{"type":"user.message","content":[{"type":"text","text":"hi"}],"session_thread_id":"thread_1"}"#,
        ] {
            assert!(serde_json::from_str::<InboundEvent>(invalid).is_err());
        }
    }

    #[test]
    fn tool_replies_reject_non_sdk_thread_selectors() {
        // Causes: the fixtures below establish `tool replies reject non sdk thread selectors` with
        // the concrete inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 an inbound Event is one of the three tool-reply
        // variants; C2 it has the SDK field set only or also carries the obsolete
        // `session_thread_id` selector. E1 C1+SDK-only decodes so the qualified
        // public Event id remains the sole routing authority; E2 C1+selector is
        // rejected by the closed schema before admission. Decision table:
        // W1=C1+SDK-only=>E1; W2=C1+selector=>E2.
        for wire in [
            serde_json::json!({
                "type":"user.tool_confirmation", "tool_use_id":"call-1",
                "result":"allow"
            }),
            serde_json::json!({
                "type":"user.custom_tool_result", "custom_tool_use_id":"call-2",
                "content":[{"type":"text","text":"done"}], "is_error":false
            }),
            serde_json::json!({
                "type":"user.tool_result", "tool_use_id":"call-3",
                "content":[{"type":"text","text":"done"}], "is_error":false
            }),
        ] {
            serde_json::from_value::<InboundEvent>(wire).expect("W1/E1");
        }
        for wire in [
            serde_json::json!({
                "type":"user.tool_confirmation", "tool_use_id":"call-1",
                "result":"allow", "session_thread_id":"sthr_1"
            }),
            serde_json::json!({
                "type":"user.custom_tool_result", "custom_tool_use_id":"call-2",
                "content":[{"type":"text","text":"done"}], "is_error":false,
                "session_thread_id":"sthr_1"
            }),
            serde_json::json!({
                "type":"user.tool_result", "tool_use_id":"call-3",
                "content":[{"type":"text","text":"done"}], "is_error":false,
                "session_thread_id":"sthr_1"
            }),
        ] {
            assert!(
                serde_json::from_value::<InboundEvent>(wire).is_err(),
                "W2/E2"
            );
        }
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
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
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
        assert_eq!(v["model_usage"]["cache_read_input_tokens"], 0);
        assert_eq!(v["model_usage"]["cache_creation_input_tokens"], 0);
        assert!(
            v["model_usage"].get("speed").is_none(),
            "speed is omitted when None"
        );

        // session.usage carries the cumulative snapshot under `usage`; a missing
        // budget is omitted (the SDK accepts optional/null) and zero counters stay
        // representable rather than making the event malformed.
        let v = serde_json::to_value(ev(OutboundKind::SessionUsage {
            usage: Usage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                ..Default::default()
            },
            budget: None,
        }))
        .unwrap();
        assert_eq!(v["type"], "session.usage");
        assert_eq!(v["usage"]["input_tokens"], 0);
        assert_eq!(v["usage"]["output_tokens"], 0);
        assert!(v.get("budget").is_none());
    }

    #[test]
    fn mcp_failures_project_structured_retry_and_server_fields() {
        let auth =
            SessionError::classify("mcp_authentication_failed", "mcp server `calc`: HTTP 401");
        assert_eq!(auth.kind, "mcp_authentication_failed_error");
        assert_eq!(auth.mcp_server_name.as_deref(), Some("calc"));
        assert_eq!(auth.retry_status, RetryStatus::Terminal);
        let connection = SessionError::classify(
            "mcp_connection_failed",
            "mcp server `offline`: connection refused",
        );
        assert_eq!(connection.kind, "mcp_connection_failed_error");
        assert_eq!(connection.mcp_server_name.as_deref(), Some("offline"));
        assert_eq!(connection.retry_status, RetryStatus::Retrying);
    }

    /// Cause graph: resolved Session agent snapshot -> thread-specific projection
    /// -> official wire fields. The coordinator roster must remain owned by the
    /// Session and must not leak into a thread snapshot.
    ///
    /// | Agent roster | Thread projection | Result |
    /// |---|---|---|
    /// | absent | typed fields | exact agent snapshot |
    /// | present | typed fields | exact snapshot without `multiagent` |
    #[test]
    fn thread_agent_projection_never_repeats_the_coordinator_roster() {
        let session_agent = SessionAgent {
            id: "coordinator".into(),
            kind: "agent",
            version: 7,
            model: ModelConfig::new("model-1"),
            name: "Coordinator".into(),
            description: Some("coordinates".into()),
            system: Some("delegate carefully".into()),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            multiagent: Some(SessionMultiagentCoordinator {
                kind: "coordinator",
                agents: vec![SessionMultiagentRosterEntry::Agent(SessionThreadAgent {
                    id: "researcher".into(),
                    kind: "agent",
                    version: 3,
                    model: ModelConfig::new("model-2"),
                    name: "Researcher".into(),
                    description: None,
                    system: None,
                    tools: Vec::new(),
                    mcp_servers: Vec::new(),
                    skills: Vec::new(),
                })],
            }),
        };
        let projected = serde_json::to_value(SessionThreadAgent::from(&session_agent)).unwrap();
        assert_eq!(projected["id"], "coordinator");
        assert_eq!(projected["version"], 7);
        assert_eq!(projected["model"]["id"], "model-1");
        assert!(
            projected.get("multiagent").is_none(),
            "the thread contract has no duplicate roster field"
        );
    }

    #[test]
    fn session_thread_agent_union_uses_the_closed_official_shapes() {
        // Causes: the fixtures below establish `session thread agent union` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `uses the closed official shapes` and every asserted state
        // transition or side effect must hold.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 the coordinated target is an ordinary Agent;
        // C2 it is an advisor invocation. E1 C1 serializes the complete frozen
        // Agent snapshot; E2 C2 serializes exactly `{type,model}` and never pads
        // an advisor with fabricated id/version/tools. Constraint: C1 xor C2.
        // Decision table:
        // | Rule | C1 | C2 | Effect |
        // | U1 | T | F | E1 |
        // | U2 | F | T | E2 |
        let agent = SessionThreadAgentValue::Agent(SessionThreadAgent {
            id: "researcher".into(),
            kind: "agent",
            version: 3,
            model: ModelConfig::new("model-agent"),
            name: "Researcher".into(),
            description: None,
            system: None,
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
        });
        let agent_json = serde_json::to_value(agent).unwrap();
        assert_eq!(agent_json["type"], "agent", "U1/E1");
        assert_eq!(agent_json["id"], "researcher", "U1/E1");
        assert_eq!(agent_json["version"], 3, "U1/E1");
        assert!(agent_json.get("tools").is_some(), "U1/E1");

        let advisor = SessionThreadAgentValue::Advisor(crate::types::agent::AdvisorRosterEntry {
            model: "model-advisor".into(),
            kind: crate::types::agent::AdvisorRosterEntryKind::Advisor,
        });
        let advisor_json = serde_json::to_value(advisor).unwrap();
        assert_eq!(
            advisor_json
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["model".to_string(), "type".to_string()]
                .into_iter()
                .collect(),
            "U2/E2"
        );
        assert_eq!(advisor_json["type"], "advisor", "U2/E2");
        assert_eq!(advisor_json["model"], "model-advisor", "U2/E2");
    }
}
