//! MCP wire DTOs shared by the client and server adapters.
//!
//! These are protocol values, not an SDK abstraction. Keeping them here prevents
//! either adapter from depending on a third-party client implementation merely to
//! serialize the MCP JSON-RPC surface.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Icon {
    pub src: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<IconTheme>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IconTheme {
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Annotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<Role>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "readOnlyHint", skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(rename = "idempotentHint", skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(rename = "openWorldHint", skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskSupport {
    #[default]
    Forbidden,
    Optional,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolExecution {
    #[serde(rename = "taskSupport", skip_serializing_if = "Option::is_none")]
    pub task_support: Option<TaskSupport>,
}

fn default_input_schema() -> Value {
    serde_json::json!({ "type": "object" })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<Icon>>,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema", skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ToolExecution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl McpToolDefinition {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            group: None,
            icons: None,
            input_schema: default_input_schema(),
            output_schema: None,
            execution: None,
            annotations: None,
            meta: None,
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.input_schema = schema;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListToolsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
}

/// Status of one durable MCP request (2025-11-25 Tasks utility).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

/// Server-owned durable request coordinates and current status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTask {
    #[serde(rename = "taskId")]
    pub task_id: String,
    pub status: TaskStatus,
    #[serde(rename = "statusMessage", skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastUpdatedAt")]
    pub last_updated_at: String,
    /// Actual retention in milliseconds; `None` is the protocol's explicit
    /// `null` meaning unlimited retention.
    #[serde(deserialize_with = "deserialize_required_nullable_u64")]
    pub ttl: Option<u64>,
    #[serde(rename = "pollInterval", skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
}

fn deserialize_required_nullable_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)
}

/// Immediate response to a task-augmented request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTaskResult {
    pub task: McpTask,
}

/// Parameters shared by `tasks/get`, `tasks/result`, and `tasks/cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIdParams {
    #[serde(rename = "taskId")]
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskMetadata>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ToolContent>,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    #[serde(rename = "resource")]
    Resource {
        uri: String,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum McpTransportError {
    #[error("Unknown tool: {0}")]
    UnknownTool(String),
    #[error("Server not found: {0}")]
    ServerNotFound(String),
    #[error("Server error: {0}")]
    ServerError(String),
    #[error("Transport error: {0}")]
    TransportError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Protocol error: {0}")]
    ProtocolError(String),
    #[error("Not supported: {0}")]
    NotSupported(String),
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Server '{0}' is restarting")]
    ServerRestarting(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InitializeCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<Icon>>,
    #[serde(rename = "websiteUrl", skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
}

impl ClientInfo {
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: InitializeCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

impl InitializeParams {
    #[must_use]
    pub fn new(config: Option<Value>) -> Self {
        Self {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: InitializeCapabilities::default(),
            client_info: ClientInfo::new("awaken-mcp", env!("CARGO_PKG_VERSION")),
            config,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerToolCapabilities {
    #[serde(rename = "listChanged", skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

/// Request kinds a server permits to be augmented with MCP Tasks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerTaskToolRequests {
    /// Presence means `tools/call` accepts `params.task` and returns
    /// [`CreateTaskResult`]. The object is intentionally extension-preserving.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerTaskRequests {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ServerTaskToolRequests>,
}

/// Server-side MCP Tasks capabilities negotiated during `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerTaskCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<serde_json::Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<ServerTaskRequests>,
}

impl ServerTaskCapabilities {
    #[must_use]
    pub fn supports_tool_call(&self) -> bool {
        self.requests
            .as_ref()
            .and_then(|requests| requests.tools.as_ref())
            .and_then(|tools| tools.call.as_ref())
            .is_some()
    }

    #[must_use]
    pub fn supports_cancel(&self) -> bool {
        self.cancel.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ServerToolCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<ServerTaskCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<Icon>>,
    #[serde(rename = "websiteUrl", skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
}

impl ServerInfo {
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

#[cfg(test)]
mod task_tests {
    use super::*;

    #[test]
    fn task_wire_shapes_and_capability_presence_follow_2025_11_25() {
        // Cause/effect graph: C1=initialize contains tasks.requests.tools.call;
        // C2=cancel is present; C3=task status is input_required with null TTL
        // and a poll interval. Effects: E1=tool-call augmentation is negotiated;
        // E2=cancel is negotiated independently; E3=task identity/status/timing
        // round-trip without turning protocol null into a fabricated duration.
        // Decision rule T1=C1+C2+C3=>E1+E2+E3.
        let capabilities: ServerCapabilities = serde_json::from_value(serde_json::json!({
            "tasks": {
                "cancel": {},
                "requests": { "tools": { "call": {} } }
            }
        }))
        .expect("valid server capabilities");
        let tasks = capabilities.tasks.expect("tasks present");
        assert!(tasks.supports_tool_call(), "T1/E1");
        assert!(tasks.supports_cancel(), "T1/E2");

        let task: McpTask = serde_json::from_value(serde_json::json!({
            "taskId": "remote-1",
            "status": "input_required",
            "statusMessage": "waiting for approval",
            "createdAt": "2026-08-30T00:00:00Z",
            "lastUpdatedAt": "2026-08-30T00:00:01Z",
            "ttl": null,
            "pollInterval": 250
        }))
        .expect("valid task");
        assert_eq!(task.task_id, "remote-1", "T1/E3");
        assert_eq!(task.status, TaskStatus::InputRequired, "T1/E3");
        assert_eq!(task.ttl, None, "T1/E3");
        assert_eq!(task.poll_interval, Some(250), "T1/E3");
        assert_eq!(serde_json::to_value(&task).unwrap()["ttl"], Value::Null);
    }

    #[test]
    fn absent_task_capabilities_fail_closed_and_task_params_use_camel_case() {
        // Cause/effect graph: C1=no tasks capability; C2=only unrelated tasks
        // members are present; C3=one task-id request; C4=a Task omits its
        // required nullable TTL. Effects: E1=no tool-call or cancel support is
        // inferred; E2=the request uses the exact taskId wire key; E3=the
        // malformed Task is rejected. Decision rules T2=C1|C2=>E1;
        // T3=C3=>E2; T4=C4=>E3.
        let absent = ServerTaskCapabilities::default();
        assert!(!absent.supports_tool_call(), "T2/E1");
        assert!(!absent.supports_cancel(), "T2/E1");
        let list_only: ServerTaskCapabilities =
            serde_json::from_value(serde_json::json!({"list": {}})).unwrap();
        assert!(!list_only.supports_tool_call(), "T2/E1");
        assert!(!list_only.supports_cancel(), "T2/E1");

        assert_eq!(
            serde_json::to_value(TaskIdParams {
                task_id: "remote-2".into()
            })
            .unwrap(),
            serde_json::json!({"taskId": "remote-2"}),
            "T3/E2"
        );

        let missing_ttl = serde_json::json!({
            "taskId": "remote-3",
            "status": "working",
            "createdAt": "2026-08-30T00:00:00Z",
            "lastUpdatedAt": "2026-08-30T00:00:01Z"
        });
        assert!(
            serde_json::from_value::<McpTask>(missing_ttl).is_err(),
            "T4/E3 required nullable fields are not optional fields"
        );
    }
}
