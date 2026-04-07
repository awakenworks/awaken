//! Remote A2A agent delegation backend -- HTTP client for A2A v1.0 HTTP+JSON.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::{Message, Role};
use awaken_contract::registry_spec::RemoteEndpoint;
use awaken_protocol_a2a::{
    Message as A2aMessage, MessageRole, Part, SendMessageConfiguration, SendMessageRequest,
    SendMessageResponse, Task, TaskState,
};
use serde_json::Value;

use super::backend::{
    AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
    DelegateRunResult, DelegateRunStatus,
};

const A2A_VERSION: &str = "1.0";
const A2A_BACKEND: &str = "a2a";
const POLL_INTERVAL_OPTION_KEY: &str = "poll_interval_ms";

/// Configuration for a remote A2A agent endpoint.
#[derive(Debug, Clone)]
pub struct A2aConfig {
    /// Base URL of the remote A2A HTTP+JSON interface
    /// (for example `https://api.example.com/v1/a2a`).
    pub base_url: String,
    /// Optional bearer token for authentication.
    pub bearer_token: Option<String>,
    /// Optional tenant path segment used to target a specific remote agent.
    pub target_agent_id: Option<String>,
    /// Interval between poll requests.
    pub poll_interval: Duration,
    /// Maximum time to wait for task completion.
    pub timeout: Duration,
}

impl A2aConfig {
    /// Create a new A2A config with defaults for poll interval and timeout.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer_token: None,
            target_agent_id: None,
            poll_interval: Duration::from_millis(2000),
            timeout: Duration::from_secs(300),
        }
    }

    #[must_use]
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    #[must_use]
    pub fn with_target_agent_id(mut self, id: impl Into<String>) -> Self {
        self.target_agent_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum A2aEndpointConfigError {
    #[error("remote endpoint backend must be `a2a`, got `{0}`")]
    UnsupportedBackend(String),
    #[error("remote endpoint base_url must not be empty")]
    EmptyBaseUrl,
    #[error("A2A backend only supports bearer auth, got `{0}`")]
    UnsupportedAuthType(String),
    #[error("A2A bearer auth requires a string `token` field")]
    MissingBearerToken,
    #[error("A2A option `{key}` must be an unsigned integer")]
    InvalidU64Option { key: &'static str },
}

impl A2aConfig {
    pub(crate) fn try_from_remote_endpoint(
        endpoint: &RemoteEndpoint,
    ) -> Result<Self, A2aEndpointConfigError> {
        if endpoint.backend != A2A_BACKEND {
            return Err(A2aEndpointConfigError::UnsupportedBackend(
                endpoint.backend.clone(),
            ));
        }

        if endpoint.base_url.trim().is_empty() {
            return Err(A2aEndpointConfigError::EmptyBaseUrl);
        }

        let mut config =
            Self::new(&endpoint.base_url).with_timeout(Duration::from_millis(endpoint.timeout_ms));

        if let Some(auth) = &endpoint.auth {
            if auth.auth_type != "bearer" {
                return Err(A2aEndpointConfigError::UnsupportedAuthType(
                    auth.auth_type.clone(),
                ));
            }

            let token = auth
                .param_str("token")
                .filter(|token| !token.is_empty())
                .ok_or(A2aEndpointConfigError::MissingBearerToken)?;
            config = config.with_bearer_token(token);
        }

        if let Some(target) = endpoint.target.as_deref() {
            config = config.with_target_agent_id(target);
        }

        if let Some(value) = endpoint.options.get(POLL_INTERVAL_OPTION_KEY) {
            let poll_interval_ms =
                value
                    .as_u64()
                    .ok_or(A2aEndpointConfigError::InvalidU64Option {
                        key: POLL_INTERVAL_OPTION_KEY,
                    })?;
            config = config.with_poll_interval(Duration::from_millis(poll_interval_ms));
        }

        Ok(config)
    }
}

/// Backend that delegates to a remote agent via A2A HTTP protocol.
pub struct A2aBackend {
    config: A2aConfig,
    client: reqwest::Client,
}

/// Factory for the built-in A2A remote backend.
pub struct A2aBackendFactory;

impl AgentBackendFactory for A2aBackendFactory {
    fn backend(&self) -> &str {
        A2A_BACKEND
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn AgentBackend>, AgentBackendFactoryError> {
        let config = A2aConfig::try_from_remote_endpoint(endpoint)
            .map_err(|error| AgentBackendFactoryError::InvalidConfig(error.to_string()))?;
        Ok(Arc::new(A2aBackend::new(config)))
    }
}

impl A2aBackend {
    /// Create a new A2A backend with the given configuration.
    pub fn new(config: A2aConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    fn interface_base_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        match self.config.target_agent_id.as_deref() {
            Some(target) => format!("{base}/{target}"),
            None => base.to_string(),
        }
    }

    /// Build a request with the standard A2A version header and optional bearer token.
    fn build_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let builder = self
            .client
            .request(method, url)
            .header("A2A-Version", A2A_VERSION)
            .header(reqwest::header::ACCEPT, "application/json");
        match &self.config.bearer_token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Submit a task to the remote A2A endpoint.
    async fn submit_task(&self, prompt: &str) -> Result<SubmittedTask, AgentBackendError> {
        let url = format!("{}/message:send", self.interface_base_url());

        let request = SendMessageRequest {
            tenant: None,
            message: A2aMessage {
                task_id: None,
                context_id: None,
                message_id: uuid::Uuid::now_v7().to_string(),
                role: MessageRole::User,
                parts: vec![Part::text(prompt.to_string())],
                metadata: None,
            },
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: vec!["text/plain".to_string()],
                task_push_notification_config: None,
                history_length: None,
                return_immediately: Some(true),
            }),
            metadata: None,
        };

        let response = self
            .build_request(reqwest::Method::POST, &url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                AgentBackendError::RemoteError(format!("failed to submit A2A task: {e}"))
            })?;

        let response = response
            .error_for_status()
            .map_err(|e| AgentBackendError::RemoteError(format!("A2A submission rejected: {e}")))?;

        let response = response.json::<SendMessageResponse>().await.map_err(|e| {
            AgentBackendError::RemoteError(format!("failed to decode A2A submission: {e}"))
        })?;

        SubmittedTask::from_response(response)
    }

    /// Fetch the current task snapshot from the remote endpoint.
    async fn fetch_task(&self, task_id: &str) -> Result<TaskSnapshot, AgentBackendError> {
        let url = format!("{}/tasks/{task_id}", self.interface_base_url());

        let response = self
            .build_request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|e| AgentBackendError::RemoteError(format!("failed to query task: {e}")))?;

        let response = response
            .error_for_status()
            .map_err(|e| AgentBackendError::RemoteError(format!("task query rejected: {e}")))?;

        let task = response.json::<Task>().await.map_err(|e| {
            AgentBackendError::RemoteError(format!("failed to decode task status: {e}"))
        })?;

        Ok(TaskSnapshot::from_task(task))
    }

    /// Poll until the task reaches a terminal state or timeout.
    async fn poll_to_completion(&self, task_id: &str) -> Result<TaskSnapshot, AgentBackendError> {
        let deadline = tokio::time::Instant::now() + self.config.timeout;

        loop {
            let snapshot = self.fetch_task(task_id).await?;
            if snapshot.is_done() {
                return Ok(snapshot);
            }

            if tokio::time::Instant::now() >= deadline {
                return Ok(TaskSnapshot {
                    state: TaskState::Failed,
                    output_text: snapshot.output_text,
                    failure_message: Some("polling timeout exceeded".to_string()),
                    task_id: task_id.to_string(),
                });
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }
}

#[async_trait]
impl AgentBackend for A2aBackend {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        _event_sink: Arc<dyn EventSink>,
        _parent_run_id: Option<String>,
        _parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        let prompt = messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if prompt.trim().is_empty() {
            return Err(AgentBackendError::ExecutionFailed(
                "no user message content to send".into(),
            ));
        }

        let submitted = self.submit_task(&prompt).await?;
        let snapshot = if submitted.snapshot.is_done() {
            submitted.snapshot
        } else {
            self.poll_to_completion(&submitted.snapshot.task_id).await?
        };

        let status = match snapshot.state {
            TaskState::Completed => DelegateRunStatus::Completed,
            TaskState::Canceled => DelegateRunStatus::Cancelled,
            TaskState::Failed => DelegateRunStatus::Failed(
                snapshot
                    .failure_message
                    .unwrap_or_else(|| "remote agent run failed".into()),
            ),
            TaskState::Rejected => DelegateRunStatus::Failed(
                snapshot
                    .failure_message
                    .unwrap_or_else(|| "remote agent rejected the task".into()),
            ),
            TaskState::InputRequired => DelegateRunStatus::Failed(
                snapshot
                    .failure_message
                    .unwrap_or_else(|| "remote agent requires additional input".into()),
            ),
            TaskState::AuthRequired => DelegateRunStatus::Failed(
                snapshot
                    .failure_message
                    .unwrap_or_else(|| "remote agent requires authentication".into()),
            ),
            TaskState::Submitted | TaskState::Working => DelegateRunStatus::Timeout,
        };

        Ok(DelegateRunResult {
            agent_id: agent_id.to_string(),
            status,
            response: snapshot.output_text,
            steps: 1,
            run_id: None,
            inbox: None,
        })
    }
}

#[derive(Debug)]
struct SubmittedTask {
    snapshot: TaskSnapshot,
}

impl SubmittedTask {
    fn from_response(response: SendMessageResponse) -> Result<Self, AgentBackendError> {
        if let Some(task) = response.task {
            return Ok(Self {
                snapshot: TaskSnapshot::from_task(task),
            });
        }
        if let Some(message) = response.message {
            return Ok(Self {
                snapshot: TaskSnapshot {
                    task_id: uuid::Uuid::now_v7().to_string(),
                    state: TaskState::Completed,
                    output_text: extract_text_from_message(&message),
                    failure_message: None,
                },
            });
        }

        Err(AgentBackendError::RemoteError(
            "sendMessage response did not contain a task or message".into(),
        ))
    }
}

#[derive(Debug, Clone)]
struct TaskSnapshot {
    task_id: String,
    state: TaskState,
    output_text: Option<String>,
    failure_message: Option<String>,
}

impl TaskSnapshot {
    fn from_task(task: Task) -> Self {
        let output_text = extract_output_text(&task);
        let failure_message = task
            .status
            .message
            .as_ref()
            .and_then(extract_text_from_message)
            .or_else(|| {
                if matches!(
                    task.status.state,
                    TaskState::Failed
                        | TaskState::Rejected
                        | TaskState::Canceled
                        | TaskState::InputRequired
                        | TaskState::AuthRequired
                ) {
                    Some(format!(
                        "remote task ended in {}",
                        task_state_name(task.status.state)
                    ))
                } else {
                    None
                }
            });

        Self {
            task_id: task.id,
            state: task.status.state,
            output_text,
            failure_message,
        }
    }

    fn is_done(&self) -> bool {
        matches!(
            self.state,
            TaskState::Completed
                | TaskState::Failed
                | TaskState::Canceled
                | TaskState::Rejected
                | TaskState::InputRequired
                | TaskState::AuthRequired
        )
    }
}

fn extract_output_text(task: &Task) -> Option<String> {
    for artifact in &task.artifacts {
        if let Some(text) = extract_text_from_parts(&artifact.parts) {
            return Some(text);
        }
    }
    if let Some(message) = &task.status.message
        && let Some(text) = extract_text_from_message(message)
    {
        return Some(text);
    }
    task.history
        .iter()
        .rev()
        .find_map(extract_text_from_message)
}

fn extract_text_from_message(message: &A2aMessage) -> Option<String> {
    extract_text_from_parts(&message.parts)
}

fn extract_text_from_parts(parts: &[Part]) -> Option<String> {
    let texts = parts
        .iter()
        .filter_map(|part| {
            part.text
                .as_deref()
                .map(ToOwned::to_owned)
                .or_else(|| part.data.as_ref().map(Value::to_string))
        })
        .collect::<Vec<_>>();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "TASK_STATE_SUBMITTED",
        TaskState::Working => "TASK_STATE_WORKING",
        TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
        TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        TaskState::Completed => "TASK_STATE_COMPLETED",
        TaskState::Failed => "TASK_STATE_FAILED",
        TaskState::Canceled => "TASK_STATE_CANCELED",
        TaskState::Rejected => "TASK_STATE_REJECTED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use serde_json::json;

    fn make_task(state: TaskState) -> Task {
        Task {
            id: "task-1".into(),
            context_id: "ctx-1".into(),
            status: awaken_protocol_a2a::TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
        }
    }

    #[test]
    fn extract_output_prefers_artifacts() {
        let task = Task {
            artifacts: vec![awaken_protocol_a2a::Artifact {
                artifact_id: "response".into(),
                name: None,
                description: None,
                parts: vec![Part::text("hello"), Part::text(" world")],
                metadata: None,
            }],
            ..make_task(TaskState::Completed)
        };
        assert_eq!(
            extract_output_text(&task).as_deref(),
            Some("hello\n\n world")
        );
    }

    #[test]
    fn extract_output_falls_back_to_status_message_then_history() {
        let status_message = A2aMessage {
            task_id: Some("task-1".into()),
            context_id: Some("ctx-1".into()),
            message_id: "msg-1".into(),
            role: MessageRole::Agent,
            parts: vec![Part::text("status output")],
            metadata: None,
        };
        let task = Task {
            status: awaken_protocol_a2a::TaskStatus {
                state: TaskState::Completed,
                message: Some(status_message.clone()),
                timestamp: None,
            },
            history: vec![A2aMessage {
                task_id: Some("task-1".into()),
                context_id: Some("ctx-1".into()),
                message_id: "msg-2".into(),
                role: MessageRole::Agent,
                parts: vec![Part::text("history output")],
                metadata: None,
            }],
            ..make_task(TaskState::Completed)
        };
        assert_eq!(extract_output_text(&task).as_deref(), Some("status output"));
    }

    #[test]
    fn task_snapshot_maps_failure_states() {
        let task = Task {
            status: awaken_protocol_a2a::TaskStatus {
                state: TaskState::Rejected,
                message: Some(A2aMessage {
                    task_id: Some("task-1".into()),
                    context_id: Some("ctx-1".into()),
                    message_id: "msg-1".into(),
                    role: MessageRole::Agent,
                    parts: vec![Part::text("policy rejected")],
                    metadata: None,
                }),
                timestamp: None,
            },
            ..make_task(TaskState::Rejected)
        };
        let snapshot = TaskSnapshot::from_task(task);
        assert_eq!(snapshot.state, TaskState::Rejected);
        assert_eq!(snapshot.failure_message.as_deref(), Some("policy rejected"));
    }

    #[test]
    fn submitted_task_requires_follow_up_polling() {
        let snapshot = TaskSnapshot::from_task(make_task(TaskState::Submitted));
        assert!(!snapshot.is_done());
    }

    #[test]
    fn send_message_response_requires_task_or_message() {
        let err = SubmittedTask::from_response(SendMessageResponse::default()).unwrap_err();
        assert!(err.to_string().contains("task or message"));
    }

    #[test]
    fn a2a_config_builder() {
        let config = A2aConfig::new("https://api.example.com/v1/a2a")
            .with_bearer_token("tok_123")
            .with_target_agent_id("worker")
            .with_poll_interval(Duration::from_millis(5000))
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.base_url, "https://api.example.com/v1/a2a");
        assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
        assert_eq!(config.target_agent_id.as_deref(), Some("worker"));
        assert_eq!(config.poll_interval, Duration::from_millis(5000));
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn a2a_config_try_from_remote_endpoint_reads_canonical_fields() {
        let mut options = BTreeMap::new();
        options.insert(POLL_INTERVAL_OPTION_KEY.into(), json!(1500));
        let endpoint = RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://api.example.com/v1/a2a".into(),
            auth: Some(awaken_contract::registry_spec::RemoteAuth::bearer(
                "tok_123",
            )),
            target: Some("worker".into()),
            timeout_ms: 60_000,
            options,
        };

        let config = A2aConfig::try_from_remote_endpoint(&endpoint).unwrap();
        assert_eq!(config.base_url, "https://api.example.com/v1/a2a");
        assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
        assert_eq!(config.target_agent_id.as_deref(), Some("worker"));
        assert_eq!(config.poll_interval, Duration::from_millis(1500));
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn a2a_config_try_from_remote_endpoint_rejects_non_bearer_auth() {
        let endpoint = RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://api.example.com/v1/a2a".into(),
            auth: Some(awaken_contract::registry_spec::RemoteAuth {
                auth_type: "basic".into(),
                params: BTreeMap::new(),
            }),
            ..Default::default()
        };

        let err = A2aConfig::try_from_remote_endpoint(&endpoint).unwrap_err();
        assert!(err.to_string().contains("only supports bearer auth"));
    }

    #[test]
    fn a2a_backend_factory_builds_backend_for_a2a_endpoint() {
        let backend = A2aBackendFactory
            .build(&RemoteEndpoint {
                backend: "a2a".into(),
                base_url: "https://api.example.com/v1/a2a".into(),
                ..Default::default()
            })
            .unwrap();

        let _backend: Arc<dyn AgentBackend> = backend;
    }

    #[test]
    fn extract_text_from_parts_supports_structured_data() {
        let parts = vec![Part {
            text: None,
            raw: None,
            url: None,
            data: Some(json!({"ok": true})),
            media_type: Some("application/json".into()),
            filename: None,
            metadata: None,
        }];
        assert_eq!(
            extract_text_from_parts(&parts).as_deref(),
            Some("{\"ok\":true}")
        );
    }
}
