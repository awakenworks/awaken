//! The A2A v0.3 protocol wire types (anti-corruption boundary).
//!
//! The only place A2A (Agent2Agent) vocabulary lives. This is the subset the
//! `message:send` method and agent-card discovery need: a `Message` of text
//! `Part`s, a `Task` with a lifecycle `TaskStatus`, the request/response
//! envelopes, streaming events, push-notification configuration, and a JSON
//! error envelope.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A2A v0.3 message role. ProtoJSON spellings are normalized only at the v1 ACL.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "agent")]
    Agent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageKind {
    #[serde(rename = "message")]
    Message,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskKind {
    #[serde(rename = "task")]
    Task,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatusUpdateKind {
    #[serde(rename = "status-update")]
    StatusUpdate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskArtifactUpdateKind {
    #[serde(rename = "artifact-update")]
    ArtifactUpdate,
}

/// One message part. A2A parts carry a `kind` discriminator (`text`/`file`/`data`)
/// alongside the payload field. A part holds text, a `file` (inline base64 or a
/// remote URI), or an opaque JSON `data` payload owned by the A2A boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Part {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, Value>>,
    },
    File {
        file: FilePart,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, Value>>,
    },
    Data {
        data: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, Value>>,
    },
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            metadata: None,
        }
    }

    pub fn text_value(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            Self::File { .. } | Self::Data { .. } => None,
        }
    }

    pub fn metadata(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Text { metadata, .. }
            | Self::File { metadata, .. }
            | Self::Data { metadata, .. } => metadata.as_ref(),
        }
    }
}

/// A file/image payload inside a `Part` (A2A `FilePart`): inline base64 `bytes` or
/// a remote `uri`, tagged with a `mimeType`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FilePart {
    Bytes(FileWithBytes),
    Uri(FileWithUri),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileWithBytes {
    pub bytes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileWithUri {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A conversation message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Message {
    /// The A2A object discriminator (`"message"`), required by the JSON clients.
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub message_id: String,
    pub role: MessageRole,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_task_ids: Vec<String>,
}

impl Message {
    /// Construct a text-only agent message.
    pub fn agent_text(message_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: MessageKind::Message,
            task_id: None,
            context_id: None,
            message_id: message_id.into(),
            role: MessageRole::Agent,
            parts: vec![Part::text(text)],
            extensions: Vec::new(),
            metadata: None,
            reference_task_ids: Vec::new(),
        }
    }

    /// The concatenated text of this message's text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(Part::text_value)
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A2A v0.3 task lifecycle state. ProtoJSON spellings stay in the v1 projection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskState {
    #[serde(rename = "submitted")]
    Submitted,
    #[serde(rename = "working")]
    Working,
    #[serde(rename = "input-required")]
    InputRequired,
    #[serde(rename = "auth-required")]
    AuthRequired,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "canceled")]
    Canceled,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "unknown")]
    Unknown,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }
}

/// A task status snapshot: the lifecycle state plus the agent's latest message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// A task resource — the unit of work `message:send` returns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Task {
    /// The A2A object discriminator (`"task"`), required by the JSON clients.
    pub kind: TaskKind,
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    /// Durable outputs a completed task produced (A2A `artifacts`). This slice
    /// carries their text parts; richer artifact kinds are omitted until needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

/// A task artifact: a named, durable output made of parts (text only here).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
    pub parts: Vec<Part>,
}

impl Artifact {
    /// The concatenated text of this artifact's text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(Part::text_value)
            .collect::<Vec<_>>()
            .join("")
    }
}

/// The `message:send` request body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessageRequest {
    /// Optional agent selector — equivalent to the path agent in
    /// `/v1/a2a/agents/{agent}/message:send`. Accepts the legacy `tenant` and the
    /// snake_case `agent_id` spellings.
    #[serde(default, alias = "agent_id", alias = "tenant")]
    pub agent_id: Option<String>,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

/// Optional execution and delivery controls carried by `message:send` and
/// `message:stream`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessageConfiguration {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    /// A2A 1.0 replacement for the v0.3 `blocking` switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_immediately: Option<bool>,
    #[serde(
        default,
        alias = "pushNotificationConfig",
        alias = "pushNotification",
        skip_serializing_if = "Option::is_none"
    )]
    pub task_push_notification_config: Option<PushNotificationConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

/// The `message:send` response wrapper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendMessageResponse {
    pub task: Task,
}

/// One server-sent A2A update. This is the same closed union exposed by the
/// official SDK; an event cannot carry two competing payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum StreamResponse {
    Task(Task),
    Message(Message),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

impl StreamResponse {
    /// Return the A2A union member itself. The official TypeScript SDK expects
    /// JSON-RPC `result` (and REST SSE data) to be a raw Task/Message/update,
    /// not an extra `{task: ...}` wrapper.
    pub fn event_value(&self) -> Value {
        serde_json::to_value(self).expect("A2A stream events serialize")
    }

    /// HTTP+JSON protobuf oneof projection used by the v0.3 REST binding.
    pub fn oneof_value(&self) -> Value {
        match self {
            Self::Task(task) => serde_json::json!({ "task": task }),
            Self::Message(message) => serde_json::json!({ "message": message }),
            Self::StatusUpdate(update) => serde_json::json!({ "statusUpdate": update }),
            Self::ArtifactUpdate(update) => serde_json::json!({ "artifactUpdate": update }),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::StatusUpdate(update) if update.final_ || update.status.state.is_terminal()
        )
    }
}

/// Push transport authentication. `credentials` is secret input and is never
/// returned from list/get endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthenticationInfo {
    pub schemes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

/// A webhook subscription attached to one A2A task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushNotificationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<AuthenticationInfo>,
}

impl PushNotificationConfig {
    /// Secret-free representation used by read/list responses.
    pub fn redacted(&self) -> Self {
        let mut projected = self.clone();
        projected.token = None;
        if let Some(authentication) = projected.authentication.as_mut() {
            authentication.credentials = None;
        }
        projected
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ListPushNotificationConfigsResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configs: Vec<TaskPushNotificationConfig>,
    pub next_page_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskStatusUpdateEvent {
    pub kind: TaskStatusUpdateKind,
    pub task_id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(rename = "final")]
    pub final_: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskArtifactUpdateEvent {
    pub kind: TaskArtifactUpdateKind,
    pub task_id: String,
    pub context_id: String,
    pub artifact: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

/// SDK wire wrapper used by `tasks/pushNotificationConfig/set|get|list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskPushNotificationConfig {
    pub task_id: String,
    pub push_notification_config: PushNotificationConfig,
}

/// Canonical v0.3 parameters shared by get, cancel and resubscribe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskIdParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskQueryParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetTaskPushNotificationConfigParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notification_config_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteTaskPushNotificationConfigParams {
    pub id: String,
    pub push_notification_config_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
}

/// A JSON error envelope (A2A HTTP+JSON binding): `{ "error": { code, message } }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub error: Error,
}

/// One error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Error {
    pub code: i32,
    pub message: String,
}

impl ErrorResponse {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            error: Error {
                code,
                message: message.into(),
            },
        }
    }
}

// ── Agent card (discovery) ──────────────────────────────────────────────────

/// The public agent discovery card served at the well-known card path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub version: String,
    pub protocol_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    /// The absolute service endpoint an A2A client posts to. Required by the
    /// official SDKs to resolve where to send.
    pub url: String,
    /// The transport the `url` speaks. `JSONRPC` is the canonical A2A binding the
    /// SDK clients default to.
    #[serde(
        rename = "preferredTransport",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred_transport: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_interfaces: Vec<AgentInterface>,
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    /// Named security schemes a client may use to authenticate (A2A
    /// `securitySchemes`, OpenAPI 3 style). Declaration only — enforcement is
    /// the host's transport layer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub security_schemes: BTreeMap<String, SecurityScheme>,
    /// Accepted requirement combinations (A2A `security`): OR across the list,
    /// AND within one map; values are the scopes required of that scheme.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<AgentCardSignature>,
    /// Whether `agent/getAuthenticatedExtendedCard` serves a richer card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_authenticated_extended_card: Option<bool>,
}

/// An additional A2A v0.3 transport exposed by the same agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentInterface {
    pub url: String,
    pub transport: String,
}

/// One way a client can authenticate, per the A2A spec's OpenAPI 3–derived
/// security schemes. The `type` field is the wire discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum SecurityScheme {
    /// A static key in a header, query parameter, or cookie.
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// The header/query/cookie parameter name carrying the key.
        name: String,
        #[serde(rename = "in")]
        location: ApiKeyLocation,
    },
    /// An RFC 7235 HTTP authentication scheme (`bearer`, `basic`, ...).
    #[serde(rename = "http", rename_all = "camelCase")]
    Http {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        scheme: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_format: Option<String>,
    },
    /// OAuth 2.0, with one entry per supported flow.
    #[serde(rename = "oauth2", rename_all = "camelCase")]
    OAuth2 {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        flows: Box<OAuthFlows>,
        /// RFC 8414 authorization-server metadata URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth2_metadata_url: Option<String>,
    },
    /// OpenID Connect discovery.
    #[serde(rename = "openIdConnect", rename_all = "camelCase")]
    OpenIdConnect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        open_id_connect_url: String,
    },
    /// Mutual TLS: authentication is the client certificate itself.
    #[serde(rename = "mutualTLS")]
    MutualTls {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

/// Where an `apiKey` credential is carried.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyLocation {
    Query,
    Header,
    Cookie,
}

/// The OAuth 2.0 flows a scheme supports (each optional, at least one set).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthFlows {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_code: Option<AuthorizationCodeFlow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_credentials: Option<ClientCredentialsFlow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implicit: Option<ImplicitFlow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<PasswordFlow>,
}

/// Authorization-code flow (with PKCE, per the A2A spec's guidance).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationCodeFlow {
    pub authorization_url: String,
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Scope name → human description. Required on the wire (may be empty).
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Client-credentials (machine-to-machine) flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientCredentialsFlow {
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Implicit flow (legacy browser clients).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImplicitFlow {
    pub authorization_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Resource-owner password flow (legacy).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PasswordFlow {
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Feature flags advertised in the card.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<AgentExtension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_transition_history: Option<bool>,
}

/// One advertised skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentProvider {
    pub organization: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExtension {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentCardSignature {
    pub protected: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<BTreeMap<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn part_serializes_with_kind_and_text() {
        // A2A JSON parts carry a `kind` discriminator alongside the payload.
        assert_eq!(
            serde_json::to_value(Part::text("hi")).unwrap(),
            json!({ "kind": "text", "text": "hi" })
        );
    }

    #[test]
    fn part_rejects_kindless_or_conflicting_payloads() {
        // Causal graph: untrusted JSON -> tagged-union admission -> no Runtime
        // message unless exactly one discriminator-owned payload is present.
        //
        // Decision table:
        // | kind | payload members | result |
        // | text | text only | typed Text |
        // | absent | text | reject |
        // | text | text + data | reject |
        // | file | bytes + uri | reject |
        assert!(serde_json::from_value::<Part>(json!({"text":"hi"})).is_err());
        assert!(
            serde_json::from_value::<Part>(json!({
                "kind":"text", "text":"hi", "data":{}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Part>(json!({
                "kind":"file", "file":{"bytes":"AAAA", "uri":"https://x"}
            }))
            .is_err()
        );
    }

    #[test]
    fn data_part_round_trips_json_without_a_text_encoding() {
        // Causal graph: A2A data JSON -> Part::data -> A2A data JSON.
        //
        // Decision table:
        // | payload                   | data retained | text populated | output exact |
        // | nested object/array       | yes           | no             | yes          |
        // | metadata beside payload   | yes           | no             | yes          |
        // These assertions forbid the former JSON -> String -> JSON compatibility
        // path, including its ambiguity between JSON strings and encoded objects.
        for input in [
            json!({"kind":"data","data":{"nested":[1,true,{"s":"x"}]}}),
            json!({"kind":"data","data":{"literal":"value"},"metadata":{"trace":7}}),
        ] {
            let part: Part = serde_json::from_value(input.clone()).unwrap();
            assert!(part.text_value().is_none());
            assert_eq!(serde_json::to_value(part).unwrap(), input);
        }
        assert!(serde_json::from_value::<Part>(json!({"kind":"data","data":null})).is_err());
        assert!(serde_json::from_value::<Part>(json!({"kind":"data","data":"literal"})).is_err());
    }

    #[test]
    fn send_message_accepts_legacy_tenant_and_snake_case_agent() {
        let a: SendMessageRequest = serde_json::from_value(json!({
            "tenant": "agent-legacy",
            "message": { "kind":"message", "messageId":"m1", "role": "user", "parts": [{ "kind": "text", "text": "hi" }] }
        }))
        .unwrap();
        assert_eq!(a.agent_id.as_deref(), Some("agent-legacy"));
        assert_eq!(a.message.text(), "hi");

        let b: SendMessageRequest = serde_json::from_value(json!({
            "agent_id": "agent-snake",
            "message": { "kind":"message", "messageId":"m2", "role": "user", "parts": [{ "kind": "text", "text": "yo" }] }
        }))
        .unwrap();
        assert_eq!(b.agent_id.as_deref(), Some("agent-snake"));
    }

    #[test]
    fn task_state_tokens_match_protocol() {
        // The authoritative v0.3 DTO accepts only its JSON spellings.
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            json!("input-required")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Completed).unwrap(),
            json!("completed")
        );
        assert!(serde_json::from_value::<TaskState>(json!("TASK_STATE_COMPLETED")).is_err());
    }

    /// `TaskState::AuthRequired` had ZERO coverage: no test pinned its wire token.
    /// The A2A JSON spelling is `"auth-required"` (hyphenated), with the proto token
    /// ProtoJSON is rejected here and normalized only at the v1 ACL.
    #[test]
    fn task_state_auth_required_round_trips_on_the_a2a_wire_token() {
        assert_eq!(
            serde_json::to_value(TaskState::AuthRequired).unwrap(),
            json!("auth-required")
        );
        let back: TaskState = serde_json::from_value(json!("auth-required")).unwrap();
        assert_eq!(back, TaskState::AuthRequired);
        assert!(serde_json::from_value::<TaskState>(json!("TASK_STATE_AUTH_REQUIRED")).is_err());
    }

    /// `InputRequired` had only a serialize assertion (in `task_state_tokens_...`),
    /// never a full round-trip or its input alias. Close the same gap as
    /// `AuthRequired`: `"input-required"` round-trips and `TASK_STATE_INPUT_REQUIRED`
    /// is accepted.
    #[test]
    fn task_state_input_required_round_trips_and_rejects_the_proto_alias() {
        let back: TaskState = serde_json::from_value(json!("input-required")).unwrap();
        assert_eq!(back, TaskState::InputRequired);
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            json!("input-required")
        );
        assert!(serde_json::from_value::<TaskState>(json!("TASK_STATE_INPUT_REQUIRED")).is_err());
    }

    /// The outbound agent role: our replies are stamped `MessageRole::Agent`, which
    /// must serialize as the A2A JSON token `"agent"` (the proto `ROLE_AGENT` is an
    /// input alias only). A wrong outbound spelling would confuse clients keying on
    /// the role; the `User` token is pinned alongside for contrast.
    #[test]
    fn message_role_serializes_with_the_a2a_wire_tokens() {
        assert_eq!(
            serde_json::to_value(MessageRole::Agent).unwrap(),
            json!("agent")
        );
        assert_eq!(
            serde_json::to_value(MessageRole::User).unwrap(),
            json!("user")
        );
        assert!(serde_json::from_value::<MessageRole>(json!("ROLE_AGENT")).is_err());
    }

    #[test]
    fn message_text_concatenates_parts() {
        let m = Message {
            kind: MessageKind::Message,
            task_id: Some("task-t".into()),
            context_id: Some("t".into()),
            message_id: "m1".into(),
            role: MessageRole::User,
            parts: vec![Part::text("hello "), Part::text("world")],
            extensions: Vec::new(),
            metadata: None,
            reference_task_ids: Vec::new(),
        };
        assert_eq!(m.text(), "hello world");
    }

    #[test]
    fn task_roundtrips_with_camel_case_fields() {
        let task = Task {
            kind: TaskKind::Task,
            id: "task-t".into(),
            context_id: "t".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message::agent_text("s", "done")),
                timestamp: None,
            },
            history: vec![Message::agent_text("a1", "done")],
            artifacts: vec![Artifact {
                artifact_id: "out".into(),
                name: Some("out".into()),
                description: None,
                extensions: Vec::new(),
                metadata: None,
                parts: vec![Part::text("artifact body")],
            }],
            metadata: None,
        };
        let value = serde_json::to_value(&task).unwrap();
        // A2A uses camelCase on the wire; the state carries its JSON token.
        assert!(value.get("contextId").is_some());
        assert_eq!(value["kind"], "task");
        assert_eq!(value["artifacts"][0]["parts"][0]["text"], "artifact body");
        assert_eq!(value["status"]["state"], "completed");
        let parsed: Task = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, task);
    }

    #[test]
    fn task_state_canceled_uses_the_a2a_wire_token() {
        assert_eq!(
            serde_json::to_value(TaskState::Canceled).unwrap(),
            serde_json::json!("canceled")
        );
        let back: TaskState = serde_json::from_value(serde_json::json!("canceled")).unwrap();
        assert_eq!(back, TaskState::Canceled);
        assert!(
            serde_json::from_value::<TaskState>(serde_json::json!("TASK_STATE_CANCELED")).is_err()
        );
    }

    #[test]
    fn part_serde_shapes_for_text_and_file() {
        // A text part omits the absent `file`.
        assert_eq!(
            serde_json::to_value(Part::text("hi")).unwrap(),
            serde_json::json!({ "kind": "text", "text": "hi" })
        );
        // A file part carries bytes + mimeType and round-trips.
        let file = Part::File {
            file: FilePart::Bytes(FileWithBytes {
                bytes: "AAAA".into(),
                mime_type: Some("image/png".into()),
                name: None,
            }),
            metadata: None,
        };
        let v = serde_json::to_value(&file).unwrap();
        assert_eq!(v["kind"], "file");
        assert_eq!(v["file"]["bytes"], "AAAA");
        assert_eq!(v["file"]["mimeType"], "image/png");
        let back: Part = serde_json::from_value(v).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn agent_card_roundtrips_over_the_v1_fields() {
        let card = AgentCard {
            name: "assistant".into(),
            description: "Awaken agent".into(),
            documentation_url: None,
            icon_url: None,
            version: "0.0.0".into(),
            protocol_version: "1.0".into(),
            provider: None,
            url: "http://localhost/v1/a2a".into(),
            preferred_transport: Some("JSONRPC".into()),
            additional_interfaces: Vec::new(),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: false,
                extensions: Vec::new(),
                state_transition_history: None,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "chat".into(),
                name: "Chat".into(),
                description: "Chat".into(),
                tags: vec!["chat".into()],
                examples: Vec::new(),
                input_modes: Vec::new(),
                output_modes: Vec::new(),
                security: Vec::new(),
            }],
            security_schemes: BTreeMap::new(),
            security: Vec::new(),
            signatures: Vec::new(),
            supports_authenticated_extended_card: None,
        };
        let value = serde_json::to_value(&card).unwrap();
        assert_eq!(value["protocolVersion"], "1.0");
        assert_eq!(value["capabilities"]["streaming"], false);
        // A card without security config omits the fields entirely (and a
        // pre-security card parses back — the fields default).
        assert!(value.get("securitySchemes").is_none());
        assert!(value.get("security").is_none());
        let parsed: AgentCard = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, card);
    }

    /// A spec-shaped card with every scheme type round-trips with the exact
    /// A2A wire spellings (`apiKey`/`in`, `bearerFormat`, `authorizationCode`,
    /// `openIdConnectUrl`, `mutualTLS`, `supportsAuthenticatedExtendedCard`).
    #[test]
    fn security_schemes_use_the_a2a_wire_spellings() {
        let json = json!({
            "securitySchemes": {
                "api": { "type": "apiKey", "name": "X-Api-Key", "in": "header" },
                "bearer": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" },
                "oauth": {
                    "type": "oauth2",
                    "oauth2MetadataUrl": "https://auth.example.com/.well-known/oauth-authorization-server",
                    "flows": {
                        "authorizationCode": {
                            "authorizationUrl": "https://auth.example.com/authorize",
                            "tokenUrl": "https://auth.example.com/token",
                            "refreshUrl": "https://auth.example.com/token",
                            "scopes": { "tasks:read": "Read tasks" }
                        },
                        "clientCredentials": {
                            "tokenUrl": "https://auth.example.com/token",
                            "scopes": {}
                        }
                    }
                },
                "oidc": {
                    "type": "openIdConnect",
                    "openIdConnectUrl": "https://auth.example.com/.well-known/openid-configuration"
                },
                "mtls": { "type": "mutualTLS", "description": "client certificate" }
            },
            "security": [ { "oauth": ["tasks:read"] }, { "api": [] } ],
            "supportsAuthenticatedExtendedCard": true
        });

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SecuritySlice {
            security_schemes: BTreeMap<String, SecurityScheme>,
            security: Vec<BTreeMap<String, Vec<String>>>,
            supports_authenticated_extended_card: Option<bool>,
        }
        let parsed: SecuritySlice = serde_json::from_value(json.clone()).unwrap();

        assert_eq!(
            parsed.security_schemes["api"],
            SecurityScheme::ApiKey {
                description: None,
                name: "X-Api-Key".into(),
                location: ApiKeyLocation::Header,
            }
        );
        assert!(matches!(
            &parsed.security_schemes["bearer"],
            SecurityScheme::Http { scheme, bearer_format: Some(f), .. }
                if scheme == "bearer" && f == "JWT"
        ));
        let SecurityScheme::OAuth2 {
            flows,
            oauth2_metadata_url,
            ..
        } = &parsed.security_schemes["oauth"]
        else {
            panic!("oauth scheme parses as OAuth2");
        };
        assert!(
            oauth2_metadata_url
                .as_deref()
                .unwrap()
                .contains("well-known")
        );
        let code = flows.authorization_code.as_ref().unwrap();
        assert_eq!(code.token_url, "https://auth.example.com/token");
        assert_eq!(code.scopes["tasks:read"], "Read tasks");
        assert!(flows.client_credentials.is_some());
        assert!(matches!(
            &parsed.security_schemes["oidc"],
            SecurityScheme::OpenIdConnect { open_id_connect_url, .. }
                if open_id_connect_url.contains("openid-configuration")
        ));
        assert!(matches!(
            &parsed.security_schemes["mtls"],
            SecurityScheme::MutualTls {
                description: Some(_)
            }
        ));
        assert_eq!(parsed.security[0]["oauth"], vec!["tasks:read".to_string()]);
        assert_eq!(parsed.supports_authenticated_extended_card, Some(true));

        // Serializing back reproduces the spec spellings byte-for-byte.
        let reserialized = serde_json::to_value(&parsed.security_schemes).unwrap();
        assert_eq!(reserialized, json["securitySchemes"]);
    }

    #[test]
    fn api_key_query_and_cookie_locations_roundtrip() {
        for (location, token) in [
            (ApiKeyLocation::Query, "query"),
            (ApiKeyLocation::Cookie, "cookie"),
        ] {
            let json = json!({ "type": "apiKey", "name": "key", "in": token });
            let parsed: SecurityScheme = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(
                parsed,
                SecurityScheme::ApiKey {
                    description: None,
                    name: "key".into(),
                    location,
                }
            );
            assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
        }
    }

    #[test]
    fn implicit_and_password_flows_roundtrip() {
        let json = json!({
            "type": "oauth2",
            "flows": {
                "implicit": {
                    "authorizationUrl": "https://auth.example.com/authorize",
                    "scopes": { "tasks:read": "Read tasks" }
                },
                "password": {
                    "tokenUrl": "https://auth.example.com/token",
                    "scopes": {}
                }
            }
        });
        let parsed: SecurityScheme = serde_json::from_value(json.clone()).unwrap();
        let SecurityScheme::OAuth2 { flows, .. } = &parsed else {
            panic!("parses as OAuth2");
        };
        let implicit = flows.implicit.as_ref().unwrap();
        assert_eq!(
            implicit.authorization_url,
            "https://auth.example.com/authorize"
        );
        assert_eq!(implicit.scopes["tasks:read"], "Read tasks");
        let password = flows.password.as_ref().unwrap();
        assert_eq!(password.token_url, "https://auth.example.com/token");
        assert!(password.scopes.is_empty());
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn http_basic_without_bearer_format_roundtrips() {
        let json = json!({ "type": "http", "scheme": "basic" });
        let parsed: SecurityScheme = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            parsed,
            SecurityScheme::Http {
                description: None,
                scheme: "basic".into(),
                bearer_format: None,
            }
        );
        // `bearerFormat` stays absent on the wire, not null.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn unknown_security_scheme_type_is_rejected() {
        assert!(serde_json::from_value::<SecurityScheme>(json!({ "type": "digest" })).is_err());
    }
}
