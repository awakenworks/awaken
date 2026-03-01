use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tirea_contract::io::ResumeDecisionAction;
use tirea_contract::runtime::tool_call::SuspendedToolCallsState;
use tirea_contract::runtime::ToolCallResume;
use tirea_state::State as _;
use tirea_contract::{gen_message_id, RunRequest, Visibility};
use tirea_contract::{SuspensionResponse, ToolCallDecision};
use tracing::warn;

/// Role for AG-UI input/output messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Developer,
    System,
    #[default]
    Assistant,
    User,
    Tool,
    Activity,
    Reasoning,
}

/// AG-UI message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    /// Message role (user, assistant, system, tool, developer, activity, reasoning).
    pub role: Role,
    /// Message content.
    pub content: String,
    /// Optional message ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Optional tool call ID (for tool messages).
    #[serde(rename = "toolCallId", skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            id: None,
            tool_call_id: None,
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            id: None,
            tool_call_id: None,
        }
    }

    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            id: None,
            tool_call_id: None,
        }
    }

    /// Create a tool result message.
    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            id: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// Create an activity message.
    pub fn activity(content: impl Into<String>) -> Self {
        Self {
            role: Role::Activity,
            content: content.into(),
            id: None,
            tool_call_id: None,
        }
    }

    /// Create a reasoning message.
    pub fn reasoning(content: impl Into<String>) -> Self {
        Self {
            role: Role::Reasoning,
            content: content.into(),
            id: None,
            tool_call_id: None,
        }
    }
}

/// AG-UI context entry from frontend readable values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Context {
    /// Human-readable description of the context.
    pub description: String,
    /// The context value.
    pub value: Value,
}

/// Tool execution location.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionLocation {
    /// Tool executes on the backend (server-side).
    Backend,
    /// Tool executes on the frontend (client-side).
    #[default]
    Frontend,
}

/// AG-UI tool definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for tool parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    /// Where the tool executes (frontend or backend).
    #[serde(default, skip_serializing_if = "is_default_frontend")]
    pub execute: ToolExecutionLocation,
}

fn is_default_frontend(loc: &ToolExecutionLocation) -> bool {
    *loc == ToolExecutionLocation::Frontend
}

impl Tool {
    /// Create a new backend tool definition.
    pub fn backend(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: None,
            execute: ToolExecutionLocation::Backend,
        }
    }

    /// Create a new frontend tool definition.
    pub fn frontend(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: None,
            execute: ToolExecutionLocation::Frontend,
        }
    }

    /// Set the JSON Schema parameters.
    pub fn with_parameters(mut self, parameters: Value) -> Self {
        self.parameters = Some(parameters);
        self
    }

    /// Check if this is a frontend tool.
    pub fn is_frontend(&self) -> bool {
        self.execute == ToolExecutionLocation::Frontend
    }
}

/// Request to run an AG-UI agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAgentInput {
    /// Thread identifier.
    #[serde(rename = "threadId")]
    pub thread_id: String,
    /// Run identifier.
    #[serde(rename = "runId")]
    pub run_id: String,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Available tools.
    #[serde(default)]
    pub tools: Vec<Tool>,
    /// Frontend readable context entries.
    #[serde(default)]
    pub context: Vec<Context>,
    /// Initial state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    /// Parent run ID (for sub-runs).
    #[serde(rename = "parentRunId", skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Model to use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// System prompt.
    #[serde(rename = "systemPrompt", skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Additional configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// Additional forwarded properties from AG-UI client runtimes.
    #[serde(
        rename = "forwardedProps",
        alias = "forwarded_props",
        skip_serializing_if = "Option::is_none"
    )]
    pub forwarded_props: Option<Value>,
}

impl RunAgentInput {
    /// Create a new request with minimal required fields.
    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            context: Vec::new(),
            state: None,
            parent_run_id: None,
            model: None,
            system_prompt: None,
            config: None,
            forwarded_props: None,
        }
    }

    /// Add a message.
    pub fn with_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Add messages.
    pub fn with_messages(mut self, messages: Vec<Message>) -> Self {
        self.messages.extend(messages);
        self
    }

    /// Set initial state.
    pub fn with_state(mut self, state: Value) -> Self {
        self.state = Some(state);
        self
    }

    /// Set model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set system prompt.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set forwarded props.
    pub fn with_forwarded_props(mut self, forwarded_props: Value) -> Self {
        self.forwarded_props = Some(forwarded_props);
        self
    }

    /// Validate the request.
    pub fn validate(&self) -> Result<(), RequestError> {
        if self.thread_id.is_empty() {
            return Err(RequestError::invalid_field("threadId cannot be empty"));
        }
        if self.run_id.is_empty() {
            return Err(RequestError::invalid_field("runId cannot be empty"));
        }
        Ok(())
    }

    /// Get frontend tools from the request.
    pub fn frontend_tools(&self) -> Vec<&Tool> {
        self.tools.iter().filter(|t| t.is_frontend()).collect()
    }

    /// Check if any interaction responses exist in this request.
    pub fn has_any_interaction_responses(&self) -> bool {
        !self.interaction_responses().is_empty()
    }

    /// Check if any suspension decisions exist in this request.
    pub fn has_any_suspension_decisions(&self) -> bool {
        !self.suspension_decisions().is_empty()
    }

    /// Check if this request contains non-empty user input.
    pub fn has_user_input(&self) -> bool {
        self.messages
            .iter()
            .any(|message| message.role == Role::User && !message.content.trim().is_empty())
    }

    /// Convert this AG-UI request to the internal runtime request.
    ///
    /// Mapping rules:
    /// - `thread_id`, `run_id`, `parent_run_id`, `state` are forwarded directly.
    /// - `messages` are converted via `convert_agui_messages` (assistant/activity/reasoning
    ///   inbound messages are intentionally skipped at runtime input boundary).
    /// - `resource_id` is not provided by AG-UI and remains `None`.
    pub fn into_runtime_run_request(self, agent_id: String) -> RunRequest {
        let initial_decisions = self.suspension_decisions();
        RunRequest {
            agent_id,
            thread_id: Some(self.thread_id),
            run_id: Some(self.run_id),
            parent_run_id: self.parent_run_id,
            resource_id: None,
            state: self.state,
            messages: convert_agui_messages(&self.messages),
            initial_decisions,
        }
    }

    /// Extract all interaction responses from tool messages.
    pub fn interaction_responses(&self) -> Vec<SuspensionResponse> {
        let expected_ids = self.suspended_call_response_ids();
        let mut latest_by_id: HashMap<String, (usize, Value)> = HashMap::new();

        self.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Tool)
            .filter_map(|(idx, m)| {
                m.tool_call_id.as_ref().and_then(|id| {
                    if !expected_ids.is_empty() && !expected_ids.contains(id) {
                        return None;
                    }
                    let result = parse_interaction_result_value(&m.content);
                    Some((idx, id.clone(), result))
                })
            })
            .for_each(|(idx, id, result)| {
                // Last write wins for duplicate IDs.
                latest_by_id.insert(id, (idx, result));
            });

        let mut responses: Vec<(usize, SuspensionResponse)> = latest_by_id
            .into_iter()
            .map(|(id, (idx, result))| (idx, SuspensionResponse::new(id, result)))
            .collect();
        responses.sort_by_key(|(idx, _)| *idx);
        responses
            .into_iter()
            .map(|(_, response)| response)
            .collect()
    }

    /// Extract all suspension decisions from tool messages.
    pub fn suspension_decisions(&self) -> Vec<ToolCallDecision> {
        self.interaction_responses()
            .into_iter()
            .map(interaction_response_to_decision)
            .collect()
    }

    /// Get all approved interaction IDs.
    pub fn approved_target_ids(&self) -> Vec<String> {
        self.suspension_decisions()
            .into_iter()
            .filter(|d| matches!(d.resume.action, ResumeDecisionAction::Resume))
            .map(|d| d.target_id)
            .collect()
    }

    /// Get all denied interaction IDs.
    pub fn denied_target_ids(&self) -> Vec<String> {
        self.suspension_decisions()
            .into_iter()
            .filter(|d| matches!(d.resume.action, ResumeDecisionAction::Cancel))
            .map(|d| d.target_id)
            .collect()
    }

    fn suspended_call_response_ids(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        let Some(state) = self.state.as_ref() else {
            return ids;
        };

        if let Some(calls) = state
            .get(SuspendedToolCallsState::PATH)
            .and_then(|v| v.get("calls"))
            .and_then(Value::as_object)
        {
            for call in calls.values() {
                if let Some(id) = call
                    .get("pending")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                {
                    ids.insert(id.to_string());
                }
                if let Some(id) = call.get("call_id").and_then(Value::as_str) {
                    ids.insert(id.to_string());
                }
                if let Some(id) = call
                    .get("suspension")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                {
                    ids.insert(id.to_string());
                }
            }
        }

        ids
    }
}

fn parse_interaction_result_value(content: &str) -> Value {
    serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.to_string()))
}

fn interaction_response_to_decision(response: SuspensionResponse) -> ToolCallDecision {
    let action = decision_action_from_result(&response.result);
    let reason = if matches!(action, ResumeDecisionAction::Cancel) {
        decision_reason_from_result(&response.result)
    } else {
        None
    };
    ToolCallDecision {
        target_id: response.target_id.clone(),
        resume: ToolCallResume {
            decision_id: format!("decision_{}", response.target_id),
            action,
            result: response.result,
            reason,
            updated_at: current_unix_millis(),
        },
    }
}

fn decision_action_from_result(result: &Value) -> ResumeDecisionAction {
    match result {
        Value::Bool(approved) => {
            if *approved {
                ResumeDecisionAction::Resume
            } else {
                ResumeDecisionAction::Cancel
            }
        }
        Value::String(value) => {
            if is_denied_token(value) {
                ResumeDecisionAction::Cancel
            } else {
                ResumeDecisionAction::Resume
            }
        }
        Value::Object(obj) => {
            if obj
                .get("approved")
                .and_then(Value::as_bool)
                .map(|approved| !approved)
                .unwrap_or(false)
            {
                return ResumeDecisionAction::Cancel;
            }
            if [
                "denied",
                "reject",
                "rejected",
                "cancel",
                "canceled",
                "cancelled",
                "abort",
                "aborted",
            ]
            .iter()
            .any(|key| obj.get(*key).and_then(Value::as_bool).unwrap_or(false))
            {
                return ResumeDecisionAction::Cancel;
            }
            if ["status", "decision", "action"].iter().any(|key| {
                obj.get(*key)
                    .and_then(Value::as_str)
                    .map(is_denied_token)
                    .unwrap_or(false)
            }) {
                return ResumeDecisionAction::Cancel;
            }
            ResumeDecisionAction::Resume
        }
        _ => ResumeDecisionAction::Resume,
    }
}

fn decision_reason_from_result(result: &Value) -> Option<String> {
    match result {
        Value::String(text) => {
            if text.trim().is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Object(obj) => obj
            .get("reason")
            .and_then(Value::as_str)
            .or_else(|| obj.get("message").and_then(Value::as_str))
            .or_else(|| obj.get("error").and_then(Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

fn is_denied_token(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false"
            | "no"
            | "denied"
            | "deny"
            | "reject"
            | "rejected"
            | "cancel"
            | "canceled"
            | "cancelled"
            | "abort"
            | "aborted"
    )
}

fn current_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Convert AG-UI message to internal message.
pub fn core_message_from_ag_ui(msg: &Message) -> tirea_contract::Message {
    let role = match msg.role {
        Role::System => tirea_contract::Role::System,
        Role::Developer => tirea_contract::Role::System,
        Role::User => tirea_contract::Role::User,
        Role::Assistant => tirea_contract::Role::Assistant,
        Role::Tool => tirea_contract::Role::Tool,
        Role::Activity => tirea_contract::Role::Assistant,
        Role::Reasoning => tirea_contract::Role::Assistant,
    };

    tirea_contract::Message {
        id: Some(msg.id.clone().unwrap_or_else(gen_message_id)),
        role,
        content: msg.content.clone(),
        tool_calls: None,
        tool_call_id: msg.tool_call_id.clone(),
        visibility: Visibility::default(),
        metadata: None,
    }
}

/// Convert AG-UI messages to internal messages.
pub fn convert_agui_messages(messages: &[Message]) -> Vec<tirea_contract::Message> {
    messages
        .iter()
        .filter(|m| {
            m.role != Role::Assistant && m.role != Role::Activity && m.role != Role::Reasoning
        })
        .map(core_message_from_ag_ui)
        .collect()
}

/// Error type for request processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestError {
    /// Error code.
    pub code: String,
    /// Error message.
    pub message: String,
}

impl RequestError {
    /// Create an invalid field error.
    pub fn invalid_field(message: impl Into<String>) -> Self {
        Self {
            code: "INVALID_FIELD".into(),
            message: message.into(),
        }
    }

    /// Create a validation error.
    pub fn validation(message: impl Into<String>) -> Self {
        Self {
            code: "VALIDATION_ERROR".into(),
            message: message.into(),
        }
    }

    /// Create an internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "INTERNAL_ERROR".into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for RequestError {}

impl From<String> for RequestError {
    fn from(message: String) -> Self {
        Self::validation(message)
    }
}

/// Build a context string from AG-UI context entries to append to the system prompt.
pub fn build_context_addendum(request: &RunAgentInput) -> Option<String> {
    if request.context.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for entry in &request.context {
        let value_str = match &entry.value {
            Value::String(s) => s.clone(),
            other => match serde_json::to_string(other) {
                Ok(value) => value,
                Err(err) => {
                    warn!(
                        error = %err,
                        description = %entry.description,
                        "failed to stringify AG-UI context value"
                    );
                    "<unserializable-context-value>".to_string()
                }
            },
        };
        parts.push(format!("[{}]: {}", entry.description, value_str));
    }
    Some(format!(
        "\n\nThe following context is available from the frontend:\n{}",
        parts.join("\n")
    ))
}
