//! The A2A v1.0 protocol wire types (anti-corruption boundary).
//!
//! The only place A2A (Agent2Agent) vocabulary lives. This is the subset the
//! `message:send` method and agent-card discovery need: a `Message` of text
//! `Part`s, a `Task` with a lifecycle `TaskStatus`, the request/response
//! envelopes, and a JSON error envelope. Richer A2A surface (artifacts, push
//! notifications, streaming) is intentionally omitted until a slice needs it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A2A message role. The wire tokens are the A2A JSON spellings (`user`/`agent`);
/// the gRPC/proto tokens (`ROLE_USER`/`ROLE_AGENT`) are accepted on input for
/// back-compat with earlier clients.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageRole {
    #[serde(rename = "user", alias = "ROLE_USER")]
    User,
    #[serde(rename = "agent", alias = "ROLE_AGENT")]
    Agent,
}

/// One message part. A2A parts carry a `kind` discriminator (`text`/`file`/`data`)
/// alongside the payload field. A part holds text, or a `file` (inline base64 or a
/// remote URI) for multimodal input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Part {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FilePart>,
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: Some("text".to_string()),
            text: Some(text.into()),
            file: None,
        }
    }
}

/// A file/image payload inside a `Part` (A2A `FilePart`): inline base64 `bytes` or
/// a remote `uri`, tagged with a `mimeType`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FilePart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// A conversation message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// The A2A object discriminator (`"message"`), required by the JSON clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub message_id: String,
    pub role: MessageRole,
    pub parts: Vec<Part>,
}

impl Message {
    /// Construct a text-only agent message.
    pub fn agent_text(message_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: Some("message".to_string()),
            task_id: None,
            context_id: None,
            message_id: message_id.into(),
            role: MessageRole::Agent,
            parts: vec![Part::text(text)],
        }
    }

    /// The concatenated text of this message's text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Task lifecycle state. The wire tokens are the A2A JSON spellings; the gRPC/proto
/// tokens (`TASK_STATE_*`) are accepted on input for back-compat.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskState {
    #[serde(rename = "working", alias = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "input-required", alias = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "auth-required", alias = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
    #[serde(rename = "completed", alias = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "failed", alias = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "canceled", alias = "TASK_STATE_CANCELED")]
    Canceled,
}

/// A task status snapshot: the lifecycle state plus the agent's latest message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

/// A task resource — the unit of work `message:send` returns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// The A2A object discriminator (`"task"`), required by the JSON clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    /// Durable outputs a completed task produced (A2A `artifacts`). This slice
    /// carries their text parts; richer artifact kinds are omitted until needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

/// A task artifact: a named, durable output made of parts (text only here).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
}

impl Artifact {
    /// The concatenated text of this artifact's text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// The `message:send` request body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    /// Optional agent selector — equivalent to the path agent in
    /// `/v1/a2a/agents/{agent}/message:send`. Accepts the legacy `tenant` and the
    /// snake_case `agent_id` spellings.
    #[serde(default, alias = "agent_id", alias = "tenant")]
    pub agent_id: Option<String>,
    pub message: SendMessage,
}

/// The inbound message: role, text parts, and the optional context it continues.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessage {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub context_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    pub role: MessageRole,
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl SendMessage {
    /// The concatenated text of the inbound parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// The `message:send` response wrapper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendMessageResponse {
    pub task: Task,
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
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub protocol_version: String,
    /// The absolute service endpoint an A2A client posts to. Required by the
    /// official SDKs to resolve where to send; omitted only in unit fixtures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The transport the `url` speaks. `JSONRPC` is the canonical A2A binding the
    /// SDK clients default to.
    #[serde(
        rename = "preferredTransport",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred_transport: Option<String>,
    pub capabilities: AgentCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    /// Whether `agent/getAuthenticatedExtendedCard` serves a richer card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_authenticated_extended_card: Option<bool>,
}

/// One way a client can authenticate, per the A2A spec's OpenAPI 3–derived
/// security schemes. The `type` field is the wire discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ClientCredentialsFlow {
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Implicit flow (legacy browser clients).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitFlow {
    pub authorization_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Resource-owner password flow (legacy).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PasswordFlow {
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Feature flags advertised in the card. Streaming/push are not implemented in
/// this slice, so they are advertised false.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
}

/// One advertised skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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
    fn part_accepts_legacy_kindless_wrapper() {
        let p: Part = serde_json::from_value(json!({ "text": "hi" })).unwrap();
        assert_eq!(p.text.as_deref(), Some("hi"));
    }

    #[test]
    fn send_message_accepts_legacy_tenant_and_snake_case_agent() {
        let a: SendMessageRequest = serde_json::from_value(json!({
            "tenant": "agent-legacy",
            "message": { "role": "ROLE_USER", "parts": [{ "text": "hi" }] }
        }))
        .unwrap();
        assert_eq!(a.agent_id.as_deref(), Some("agent-legacy"));
        assert_eq!(a.message.text(), "hi");

        let b: SendMessageRequest = serde_json::from_value(json!({
            "agent_id": "agent-snake",
            "message": { "role": "ROLE_USER", "parts": [{ "text": "yo" }] }
        }))
        .unwrap();
        assert_eq!(b.agent_id.as_deref(), Some("agent-snake"));
    }

    #[test]
    fn task_state_tokens_match_protocol() {
        // The A2A JSON spellings on the wire; the proto tokens are input aliases.
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            json!("input-required")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Completed).unwrap(),
            json!("completed")
        );
        assert_eq!(
            serde_json::from_value::<TaskState>(json!("TASK_STATE_COMPLETED")).unwrap(),
            TaskState::Completed
        );
    }

    /// `TaskState::AuthRequired` had ZERO coverage: no test pinned its wire token.
    /// The A2A JSON spelling is `"auth-required"` (hyphenated), with the proto token
    /// `TASK_STATE_AUTH_REQUIRED` accepted on input for back-compat. A rename would
    /// silently break a remote agent that awaits a task awaiting auth, so pin the full
    /// round-trip plus the input alias.
    #[test]
    fn task_state_auth_required_round_trips_on_the_a2a_wire_token() {
        assert_eq!(
            serde_json::to_value(TaskState::AuthRequired).unwrap(),
            json!("auth-required")
        );
        let back: TaskState = serde_json::from_value(json!("auth-required")).unwrap();
        assert_eq!(back, TaskState::AuthRequired);
        let alias: TaskState = serde_json::from_value(json!("TASK_STATE_AUTH_REQUIRED")).unwrap();
        assert_eq!(alias, TaskState::AuthRequired);
    }

    /// `InputRequired` had only a serialize assertion (in `task_state_tokens_...`),
    /// never a full round-trip or its input alias. Close the same gap as
    /// `AuthRequired`: `"input-required"` round-trips and `TASK_STATE_INPUT_REQUIRED`
    /// is accepted.
    #[test]
    fn task_state_input_required_round_trips_and_accepts_the_proto_alias() {
        let back: TaskState = serde_json::from_value(json!("input-required")).unwrap();
        assert_eq!(back, TaskState::InputRequired);
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            json!("input-required")
        );
        let alias: TaskState = serde_json::from_value(json!("TASK_STATE_INPUT_REQUIRED")).unwrap();
        assert_eq!(alias, TaskState::InputRequired);
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
        // The proto tokens are accepted on input for back-compat.
        assert_eq!(
            serde_json::from_value::<MessageRole>(json!("ROLE_AGENT")).unwrap(),
            MessageRole::Agent
        );
    }

    #[test]
    fn message_text_concatenates_parts() {
        let m = Message {
            kind: Some("message".into()),
            task_id: Some("task-t".into()),
            context_id: Some("t".into()),
            message_id: "m1".into(),
            role: MessageRole::User,
            parts: vec![Part::text("hello "), Part::text("world")],
        };
        assert_eq!(m.text(), "hello world");
    }

    #[test]
    fn task_roundtrips_with_camel_case_fields() {
        let task = Task {
            kind: Some("task".into()),
            id: "task-t".into(),
            context_id: "t".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message::agent_text("s", "done")),
            },
            history: vec![Message::agent_text("a1", "done")],
            artifacts: vec![Artifact {
                name: Some("out".into()),
                parts: vec![Part::text("artifact body")],
            }],
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
        // The back-compat `TASK_STATE_*` alias is accepted on input.
        let alias: TaskState =
            serde_json::from_value(serde_json::json!("TASK_STATE_CANCELED")).unwrap();
        assert_eq!(alias, TaskState::Canceled);
    }

    #[test]
    fn part_serde_shapes_for_text_and_file() {
        // A text part omits the absent `file`.
        assert_eq!(
            serde_json::to_value(Part::text("hi")).unwrap(),
            serde_json::json!({ "kind": "text", "text": "hi" })
        );
        // A file part carries bytes + mimeType and round-trips.
        let file = Part {
            kind: Some("file".into()),
            text: None,
            file: Some(FilePart {
                bytes: Some("AAAA".into()),
                uri: None,
                mime_type: Some("image/png".into()),
            }),
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
            version: "0.0.0".into(),
            protocol_version: "1.0".into(),
            url: Some("http://localhost/v1/a2a".into()),
            preferred_transport: Some("JSONRPC".into()),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: false,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "chat".into(),
                name: "Chat".into(),
                tags: vec!["chat".into()],
            }],
            security_schemes: BTreeMap::new(),
            security: Vec::new(),
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
