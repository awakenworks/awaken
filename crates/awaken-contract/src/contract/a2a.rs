//! Shared A2A v1.0 wire types used by the runtime and server adapters.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Public agent discovery card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    /// Human-readable agent name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Ordered list of supported transport bindings.
    pub supported_interfaces: Vec<AgentInterface>,
    /// Optional provider information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    /// Card version.
    pub version: String,
    /// Optional documentation URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    /// Agent capabilities.
    pub capabilities: AgentCapabilities,
    /// Declared security schemes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub security_schemes: BTreeMap<String, Value>,
    /// Authorization requirements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
    /// Default accepted input modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_input_modes: Vec<String>,
    /// Default produced output modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_output_modes: Vec<String>,
    /// Declared skills.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
    /// Optional card signatures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<Value>,
    /// Optional icon URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// One supported A2A interface binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterface {
    /// Base URL of the interface.
    pub url: String,
    /// Protocol binding identifier, e.g. `HTTP+JSON`.
    pub protocol_binding: String,
    /// Supported protocol version for this interface.
    pub protocol_version: String,
    /// Optional tenant path parameter value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Provider metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    pub organization: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Feature flags advertised in the agent card.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default)]
    pub state_transition_history: bool,
    #[serde(default)]
    pub extended_agent_card: bool,
}

/// One advertised skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,
}

/// A task resource.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Task identifier.
    pub id: String,
    /// Conversation or workflow context identifier.
    pub context_id: String,
    /// Current task status.
    pub status: TaskStatus,
    /// Generated artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    /// Conversation history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Message>,
    /// Optional task metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Task status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// Task lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskState {
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
}

impl TaskState {
    /// Whether the task can still make progress.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    /// Whether the task is waiting on external action.
    pub fn is_interrupted(self) -> bool {
        matches!(self, Self::InputRequired | Self::AuthRequired)
    }
}

/// Conversation message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Optional task identifier this message belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Optional context identifier this message belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Stable message identifier.
    pub message_id: String,
    /// Message role.
    pub role: MessageRole,
    /// Message payload parts.
    pub parts: Vec<Part>,
    /// Optional metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Message {
    /// Construct a text-only user message.
    pub fn user_text(message_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            task_id: None,
            context_id: None,
            message_id: message_id.into(),
            role: MessageRole::User,
            parts: vec![Part::text(text)],
            metadata: None,
        }
    }

    /// Construct a text-only agent message.
    pub fn agent_text(message_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            task_id: None,
            context_id: None,
            message_id: message_id.into(),
            role: MessageRole::Agent,
            parts: vec![Part::text(text)],
            metadata: None,
        }
    }

    /// Extract concatenated text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Message role enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageRole {
    #[serde(rename = "ROLE_USER")]
    User,
    #[serde(rename = "ROLE_AGENT")]
    Agent,
}

/// One multimodal part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    /// Plain text payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64-encoded binary payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Remote file or media URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Structured JSON data payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Optional media type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Optional file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Optional metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Part {
    /// Construct a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            raw: None,
            url: None,
            data: None,
            media_type: None,
            filename: None,
            metadata: None,
        }
    }

    /// Returns true when one supported payload member is present.
    pub fn has_payload(&self) -> bool {
        self.text.is_some() || self.raw.is_some() || self.url.is_some() || self.data.is_some()
    }
}

/// Output artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// SendMessage request payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// SendMessage configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageConfiguration {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_output_modes: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "pushNotificationConfig"
    )]
    pub task_push_notification_config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_immediately: Option<bool>,
}

/// SendMessage response wrapper.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Task>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

impl SendMessageResponse {
    /// Wrap a task response.
    pub fn task(task: Task) -> Self {
        Self {
            task: Some(task),
            message: None,
        }
    }
}

/// Streaming response wrapper.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StreamResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Task>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_update: Option<TaskStatusUpdateEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_update: Option<TaskArtifactUpdateEvent>,
}

/// Task status update event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Task artifact update event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub artifact: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Task list response payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<Task>,
    pub total_size: usize,
    pub page_size: usize,
    pub next_page_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_card_roundtrip_uses_v1_interface_fields() {
        let card = AgentCard {
            name: "default".into(),
            description: "Awaken agent".into(),
            supported_interfaces: vec![AgentInterface {
                url: "https://example.com/v1/a2a".into(),
                protocol_binding: "HTTP+JSON".into(),
                protocol_version: "1.0".into(),
                tenant: None,
            }],
            provider: None,
            version: "1.0.0".into(),
            documentation_url: Some("https://example.com/docs".into()),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: false,
                state_transition_history: false,
                extended_agent_card: false,
            },
            security_schemes: BTreeMap::new(),
            security: Vec::new(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![AgentSkill {
                id: "chat".into(),
                name: "Chat".into(),
                description: Some("Conversational assistance".into()),
                tags: vec!["chat".into()],
                examples: Vec::new(),
                input_modes: vec!["text/plain".into()],
                output_modes: vec!["text/plain".into()],
            }],
            signatures: Vec::new(),
            icon_url: None,
        };

        let value = serde_json::to_value(&card).unwrap();
        assert!(value.get("supportedInterfaces").is_some());
        assert!(
            value.get("url").is_none(),
            "legacy top-level url must not be emitted"
        );
        let parsed: AgentCard = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.supported_interfaces[0].protocol_version, "1.0");
    }

    #[test]
    fn part_uses_wrapper_fields_without_kind() {
        let part = Part::text("hello");
        let value = serde_json::to_value(&part).unwrap();
        assert_eq!(value, json!({"text": "hello"}));
    }

    #[test]
    fn message_text_concatenates_text_parts() {
        let message = Message {
            task_id: Some("task-1".into()),
            context_id: Some("task-1".into()),
            message_id: "msg-1".into(),
            role: MessageRole::User,
            parts: vec![Part::text("hello "), Part::text("world")],
            metadata: None,
        };
        assert_eq!(message.text(), "hello world");
    }

    #[test]
    fn send_message_configuration_accepts_http_alias() {
        let cfg: SendMessageConfiguration = serde_json::from_value(json!({
            "acceptedOutputModes": ["text/plain"],
            "pushNotificationConfig": {"url": "https://example.com"},
            "historyLength": 3,
            "returnImmediately": true
        }))
        .unwrap();

        assert_eq!(cfg.accepted_output_modes, vec!["text/plain"]);
        assert!(cfg.task_push_notification_config.is_some());
        assert_eq!(cfg.history_length, Some(3));
        assert_eq!(cfg.return_immediately, Some(true));
    }

    #[test]
    fn task_state_helpers_match_protocol_lifecycle() {
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::InputRequired.is_interrupted());
        assert!(!TaskState::Working.is_terminal());
    }
}
