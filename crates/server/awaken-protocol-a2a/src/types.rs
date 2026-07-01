//! The A2A v1.0 protocol wire types (anti-corruption boundary).
//!
//! The only place A2A (Agent2Agent) vocabulary lives. This is the subset the
//! `message:send` method and agent-card discovery need: a `Message` of text
//! `Part`s, a `Task` with a lifecycle `TaskStatus`, the request/response
//! envelopes, and a JSON error envelope. Richer A2A surface (artifacts, push
//! notifications, streaming) is intentionally omitted until a slice needs it.

use serde::{Deserialize, Serialize};

/// A2A message role. The wire tokens are the A2A enum names.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageRole {
    #[serde(rename = "ROLE_USER")]
    User,
    #[serde(rename = "ROLE_AGENT")]
    Agent,
}

/// One message part. A2A uses wrapper fields (no `kind` discriminator); this slice
/// carries text only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Part {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
        }
    }
}

/// A conversation message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
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

/// Task lifecycle state (the A2A enum tokens).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
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
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
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
    pub capabilities: AgentCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
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
    fn part_serializes_as_wrapper_field() {
        assert_eq!(
            serde_json::to_value(Part::text("hi")).unwrap(),
            json!({ "text": "hi" })
        );
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
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            json!("TASK_STATE_INPUT_REQUIRED")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Completed).unwrap(),
            json!("TASK_STATE_COMPLETED")
        );
    }

    #[test]
    fn message_text_concatenates_parts() {
        let m = Message {
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
            id: "task-t".into(),
            context_id: "t".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message::agent_text("s", "done")),
            },
            history: vec![Message::agent_text("a1", "done")],
        };
        let value = serde_json::to_value(&task).unwrap();
        // A2A uses camelCase on the wire; the state carries its enum token.
        assert!(value.get("contextId").is_some());
        assert_eq!(value["status"]["state"], "TASK_STATE_COMPLETED");
        let parsed: Task = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, task);
    }

    #[test]
    fn agent_card_roundtrips_over_the_v1_fields() {
        let card = AgentCard {
            name: "assistant".into(),
            description: "Awaken agent".into(),
            version: "0.0.0".into(),
            protocol_version: "1.0".into(),
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
        };
        let value = serde_json::to_value(&card).unwrap();
        assert_eq!(value["protocolVersion"], "1.0");
        assert_eq!(value["capabilities"]["streaming"], false);
        let parsed: AgentCard = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, card);
    }
}
