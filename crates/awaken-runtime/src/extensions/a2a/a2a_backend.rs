//! Remote A2A agent delegation backend -- HTTP client for A2A v1.0 HTTP+JSON.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use crate::backend::{
    BackendAbortRequest, BackendCancellationCapability, BackendCapabilities,
    BackendContinuationCapability, BackendDelegateRunRequest, BackendOutputArtifact,
    BackendOutputCapability, BackendRootRunRequest, BackendRunOutput, BackendRunResult,
    BackendRunStatus, BackendTranscriptCapability, BackendWaitCapability, ExecutionBackend,
    ExecutionBackendError, ExecutionBackendFactory, ExecutionBackendFactoryError,
};
use async_trait::async_trait;
use awaken_contract::contract::content::{
    AudioSource, ContentBlock, DocumentSource, ImageSource, VideoSource,
};
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, Role, Visibility};
use awaken_contract::now_ms;
use awaken_contract::registry_spec::RemoteEndpoint;
use awaken_contract::state::PersistedState;
use awaken_protocol_a2a::{
    Message as A2aMessage, MessageRole, Part, SendMessageConfiguration, SendMessageRequest,
    SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent, TaskState,
    TaskStatusUpdateEvent,
};
use futures::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const A2A_VERSION: &str = "1.0";
const A2A_BACKEND: &str = "a2a";
const HISTORY_LENGTH_OPTION_KEY: &str = "history_length";
const POLL_INTERVAL_OPTION_KEY: &str = "poll_interval_ms";
const REMOTE_STATE_KEY: &str = "__runtime_remote_backend";
const RETURN_IMMEDIATELY_OPTION_KEY: &str = "return_immediately";
const WAIT_REASON_AUTH_REQUIRED: &str = "auth_required";
const WAIT_REASON_INPUT_REQUIRED: &str = "input_required";

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
    /// Optional upstream task history window for follow-up requests.
    pub history_length: Option<u32>,
    /// Whether sendMessage should return immediately with an in-progress task.
    pub return_immediately: bool,
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
            history_length: None,
            return_immediately: true,
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

    #[must_use]
    pub fn with_history_length(mut self, history_length: u32) -> Self {
        self.history_length = Some(history_length);
        self
    }

    #[must_use]
    pub fn with_return_immediately(mut self, return_immediately: bool) -> Self {
        self.return_immediately = return_immediately;
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
    #[error("A2A option `{key}` must be a boolean")]
    InvalidBoolOption { key: &'static str },
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

        if let Some(value) = endpoint.options.get(HISTORY_LENGTH_OPTION_KEY) {
            let history_length =
                value
                    .as_u64()
                    .ok_or(A2aEndpointConfigError::InvalidU64Option {
                        key: HISTORY_LENGTH_OPTION_KEY,
                    })?;
            let history_length = u32::try_from(history_length).map_err(|_| {
                A2aEndpointConfigError::InvalidU64Option {
                    key: HISTORY_LENGTH_OPTION_KEY,
                }
            })?;
            config = config.with_history_length(history_length);
        }

        if let Some(value) = endpoint.options.get(RETURN_IMMEDIATELY_OPTION_KEY) {
            let return_immediately =
                value
                    .as_bool()
                    .ok_or(A2aEndpointConfigError::InvalidBoolOption {
                        key: RETURN_IMMEDIATELY_OPTION_KEY,
                    })?;
            config = config.with_return_immediately(return_immediately);
        }

        Ok(config)
    }
}

/// Backend that delegates to a remote agent via A2A HTTP protocol.
pub struct A2aBackend {
    config: A2aConfig,
    client: reqwest::Client,
    in_flight_tasks: Mutex<HashMap<String, String>>,
}

/// Factory for the built-in A2A remote backend.
pub struct A2aBackendFactory;

impl ExecutionBackendFactory for A2aBackendFactory {
    fn backend(&self) -> &str {
        A2A_BACKEND
    }

    fn validate(&self, endpoint: &RemoteEndpoint) -> Result<(), ExecutionBackendFactoryError> {
        A2aConfig::try_from_remote_endpoint(endpoint)
            .map(|_| ())
            .map_err(|error| ExecutionBackendFactoryError::InvalidConfig(error.to_string()))
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn ExecutionBackend>, ExecutionBackendFactoryError> {
        let config = A2aConfig::try_from_remote_endpoint(endpoint)
            .map_err(|error| ExecutionBackendFactoryError::InvalidConfig(error.to_string()))?;
        Ok(Arc::new(A2aBackend::new(config)))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedRemoteBackendState {
    #[serde(default = "remote_state_schema_version")]
    version: u32,
    #[serde(default)]
    targets: BTreeMap<String, PersistedA2aThreadState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedA2aThreadState {
    #[serde(default = "remote_state_schema_version")]
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_ms: Option<u64>,
}

#[derive(Debug)]
enum SubmissionOutcome {
    DirectMessage(DirectMessageSnapshot),
    Task(TaskSnapshot),
}

const REMOTE_STATE_SCHEMA_VERSION: u32 = 1;

fn remote_state_schema_version() -> u32 {
    REMOTE_STATE_SCHEMA_VERSION
}

enum A2aExecutionRequest<'a> {
    Root(Box<BackendRootRunRequest<'a>>),
    Delegate(BackendDelegateRunRequest<'a>),
}

impl<'a> A2aExecutionRequest<'a> {
    fn agent_id(&self) -> &'a str {
        match self {
            Self::Root(request) => request.agent_id,
            Self::Delegate(request) => request.agent_id,
        }
    }

    fn run_identity(&self) -> Option<&RunIdentity> {
        match self {
            Self::Root(request) => Some(&request.run_identity),
            Self::Delegate(_) => None,
        }
    }

    fn checkpoint_store(
        &self,
    ) -> Option<&'a dyn awaken_contract::contract::storage::ThreadRunStore> {
        match self {
            Self::Root(request) => request.checkpoint_store,
            Self::Delegate(_) => None,
        }
    }

    fn turn_messages(&self) -> &[Message] {
        let (messages, new_messages) = match self {
            Self::Root(request) => (&request.messages, &request.new_messages),
            Self::Delegate(request) => (&request.messages, &request.new_messages),
        };
        if new_messages.is_empty() {
            messages.as_slice()
        } else {
            new_messages.as_slice()
        }
    }

    fn is_root(&self) -> bool {
        matches!(self, Self::Root(_))
    }

    fn is_continuation(&self) -> bool {
        match self {
            Self::Root(request) => request.is_continuation,
            Self::Delegate(_) => false,
        }
    }
}

impl A2aBackend {
    /// Create a new A2A backend with the given configuration.
    pub fn new(config: A2aConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            in_flight_tasks: Mutex::new(HashMap::new()),
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

    fn remote_target_key(&self) -> String {
        format!("{A2A_BACKEND}:{}", self.interface_base_url())
    }

    async fn load_persisted_state(
        &self,
        request: &A2aExecutionRequest<'_>,
    ) -> Result<Option<PersistedState>, ExecutionBackendError> {
        let Some(storage) = request.checkpoint_store() else {
            return Ok(None);
        };
        let Some(run_identity) = request.run_identity() else {
            return Ok(None);
        };

        Ok(storage
            .latest_run(&run_identity.thread_id)
            .await
            .map_err(|error| {
                ExecutionBackendError::ExecutionFailed(format!(
                    "failed to load thread state for '{}': {error}",
                    run_identity.thread_id
                ))
            })?
            .and_then(|run| run.state))
    }

    fn build_turn_message(
        &self,
        request: &A2aExecutionRequest<'_>,
        persisted: Option<&PersistedState>,
    ) -> Result<A2aMessage, ExecutionBackendError> {
        let prior_state =
            persisted.and_then(|state| read_remote_state_entry(state, &self.remote_target_key()));
        let parts = request
            .turn_messages()
            .iter()
            .filter(|message| message.visibility == Visibility::All && message.role == Role::User)
            .flat_map(|message| message.content.iter())
            .filter_map(content_block_to_a2a_part)
            .collect::<Vec<_>>();

        if parts.is_empty() {
            return Err(ExecutionBackendError::ExecutionFailed(
                "no user message content to send".into(),
            ));
        }

        let root_identity = request.run_identity();
        let task_id = prior_state
            .as_ref()
            .and_then(|state| reusable_prior_task_id(state, request.is_continuation()))
            .or_else(|| root_identity.map(|identity| identity.run_id.clone()));
        let context_id = prior_state
            .as_ref()
            .and_then(|state| state.context_id.clone())
            .or_else(|| root_identity.map(|identity| identity.thread_id.clone()));

        Ok(A2aMessage {
            task_id,
            context_id,
            message_id: uuid::Uuid::now_v7().to_string(),
            role: MessageRole::User,
            parts,
            metadata: None,
        })
    }

    /// Submit a task to the remote A2A endpoint.
    async fn submit_task(
        &self,
        message: A2aMessage,
    ) -> Result<SubmissionOutcome, ExecutionBackendError> {
        let url = format!("{}/message:send", self.interface_base_url());

        let request = SendMessageRequest {
            tenant: None,
            message,
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: vec![
                    "text/plain".to_string(),
                    "application/json".to_string(),
                    "application/octet-stream".to_string(),
                ],
                task_push_notification_config: None,
                history_length: self.config.history_length,
                return_immediately: Some(self.config.return_immediately),
            }),
            metadata: None,
        };

        let response = self
            .build_request(reqwest::Method::POST, &url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                ExecutionBackendError::RemoteError(format!("failed to submit A2A task: {e}"))
            })?;

        let response = response.error_for_status().map_err(|e| {
            ExecutionBackendError::RemoteError(format!("A2A submission rejected: {e}"))
        })?;

        let response = response.json::<SendMessageResponse>().await.map_err(|e| {
            ExecutionBackendError::RemoteError(format!("failed to decode A2A submission: {e}"))
        })?;

        SubmissionOutcome::from_response(response)
    }

    /// Fetch the current task snapshot from the remote endpoint.
    async fn fetch_task(&self, task_id: &str) -> Result<TaskSnapshot, ExecutionBackendError> {
        let url = format!("{}/tasks/{task_id}", self.interface_base_url());

        let response = self
            .build_request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|e| {
                ExecutionBackendError::RemoteError(format!("failed to query task: {e}"))
            })?;

        let response = response
            .error_for_status()
            .map_err(|e| ExecutionBackendError::RemoteError(format!("task query rejected: {e}")))?;

        let task = response.json::<Task>().await.map_err(|e| {
            ExecutionBackendError::RemoteError(format!("failed to decode task status: {e}"))
        })?;

        Ok(TaskSnapshot::from_task(task))
    }

    async fn cancel_task(&self, task_id: &str) -> Result<(), ExecutionBackendError> {
        let url = format!("{}/tasks/{task_id}:cancel", self.interface_base_url());

        let response = self
            .build_request(reqwest::Method::POST, &url)
            .send()
            .await
            .map_err(|error| {
                ExecutionBackendError::RemoteError(format!(
                    "failed to cancel A2A task '{task_id}': {error}"
                ))
            })?;

        response.error_for_status().map_err(|error| {
            ExecutionBackendError::RemoteError(format!(
                "A2A task cancel rejected for '{task_id}': {error}"
            ))
        })?;

        Ok(())
    }

    async fn subscribe_to_completion(
        &self,
        snapshot: TaskSnapshot,
    ) -> Result<Option<PollCompletion>, ExecutionBackendError> {
        let url = format!(
            "{}/tasks/{}:subscribe",
            self.interface_base_url(),
            snapshot.task_id
        );
        let response = self
            .build_request(reqwest::Method::GET, &url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|error| {
                ExecutionBackendError::RemoteError(format!(
                    "failed to subscribe to A2A task '{}': {error}",
                    snapshot.task_id
                ))
            })?;

        if subscribe_requires_poll_fallback(response.status()) {
            return Ok(None);
        }

        let response = response.error_for_status().map_err(|error| {
            ExecutionBackendError::RemoteError(format!(
                "A2A task subscribe rejected for '{}': {error}",
                snapshot.task_id
            ))
        })?;

        let deadline = tokio::time::Instant::now() + self.config.timeout;
        let mut stream = response.bytes_stream();
        let mut decoder = SseDataDecoder::default();
        let mut latest = snapshot;

        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => {
                    return Ok(Some(PollCompletion::TimedOut(
                        latest.with_timeout_message(),
                    )));
                }
                Ok(Some(Ok(chunk))) => {
                    let chunk = std::str::from_utf8(chunk.as_ref()).map_err(|error| {
                        ExecutionBackendError::RemoteError(format!(
                            "failed to decode A2A subscribe stream for '{}': {error}",
                            latest.task_id
                        ))
                    })?;
                    for event in decoder.push(chunk) {
                        latest.apply_stream_response(parse_stream_response(&event)?);
                        if latest.is_done() {
                            return Ok(Some(PollCompletion::Finished(latest)));
                        }
                    }
                }
                Ok(Some(Err(error))) => {
                    tracing::warn!(
                        task_id = %latest.task_id,
                        error = %error,
                        "A2A subscribe stream failed; falling back to polling"
                    );
                    return Ok(None);
                }
                Ok(None) => {
                    if let Some(event) = decoder.finish() {
                        latest.apply_stream_response(parse_stream_response(&event)?);
                        if latest.is_done() {
                            return Ok(Some(PollCompletion::Finished(latest)));
                        }
                    }
                    return Ok(None);
                }
            }
        }
    }

    async fn observe_to_completion(
        &self,
        snapshot: TaskSnapshot,
    ) -> Result<PollCompletion, ExecutionBackendError> {
        if snapshot.is_done() {
            return Ok(PollCompletion::Finished(snapshot));
        }

        if let Some(completion) = self.subscribe_to_completion(snapshot.clone()).await? {
            return Ok(completion);
        }

        self.poll_to_completion(&snapshot.task_id).await
    }

    /// Poll until the task reaches a terminal state or timeout.
    async fn poll_to_completion(
        &self,
        task_id: &str,
    ) -> Result<PollCompletion, ExecutionBackendError> {
        let deadline = tokio::time::Instant::now() + self.config.timeout;

        loop {
            let snapshot = self.fetch_task(task_id).await?;
            if snapshot.is_done() {
                return Ok(PollCompletion::Finished(snapshot));
            }

            if tokio::time::Instant::now() >= deadline {
                return Ok(PollCompletion::TimedOut(TaskSnapshot {
                    state: snapshot.state,
                    output_text: snapshot.output_text,
                    output: snapshot.output,
                    failure_message: Some("polling timeout exceeded".to_string()),
                    task_id: task_id.to_string(),
                    context_id: snapshot.context_id,
                }));
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }
}

#[async_trait]
impl ExecutionBackend for A2aBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            cancellation: BackendCancellationCapability::RemoteAbort,
            decisions: false,
            overrides: false,
            frontend_tools: false,
            continuation: BackendContinuationCapability::RemoteState,
            waits: BackendWaitCapability::InputAndAuth,
            transcript: BackendTranscriptCapability::IncrementalUserMessagesWithRemoteState,
            output: BackendOutputCapability::TextAndArtifacts,
        }
    }

    async fn abort(&self, request: BackendAbortRequest<'_>) -> Result<(), ExecutionBackendError> {
        let Some(task_id) = self
            .in_flight_tasks
            .lock()
            .get(&request.run_identity.run_id)
            .cloned()
        else {
            return Ok(());
        };

        self.cancel_task(&task_id).await?;
        self.in_flight_tasks
            .lock()
            .remove(&request.run_identity.run_id);
        Ok(())
    }

    async fn execute_root(
        &self,
        request: BackendRootRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        self.execute_turn(A2aExecutionRequest::Root(Box::new(request)))
            .await
    }

    async fn execute_delegate(
        &self,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        self.execute_turn(A2aExecutionRequest::Delegate(request))
            .await
    }
}

impl A2aBackend {
    async fn execute_turn(
        &self,
        request: A2aExecutionRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        let persisted_state = self.load_persisted_state(&request).await?;
        let turn_message = self.build_turn_message(&request, persisted_state.as_ref())?;
        let submitted_task_id = turn_message.task_id.clone();
        let submitted_context_id = turn_message.context_id.clone();
        let run_id = request.run_identity().map(|run| run.run_id.clone());
        if let (Some(run_id), Some(task_id)) = (&run_id, turn_message.task_id.as_ref()) {
            self.in_flight_tasks
                .lock()
                .insert(run_id.clone(), task_id.clone());
        }

        match self.submit_task(turn_message).await? {
            SubmissionOutcome::DirectMessage(mut snapshot) => {
                if let Some(run_id) = &run_id {
                    self.in_flight_tasks.lock().remove(run_id);
                }
                if snapshot.task_id.is_none() {
                    snapshot.task_id = submitted_task_id;
                }
                if snapshot.context_id.is_none() {
                    snapshot.context_id = submitted_context_id;
                }
                let output_text = snapshot.output.text.clone();
                Ok(BackendRunResult {
                    agent_id: request.agent_id().to_string(),
                    status: BackendRunStatus::Completed,
                    termination: TerminationReason::NaturalEnd,
                    status_reason: None,
                    response: output_text,
                    output: snapshot.output.clone(),
                    steps: 1,
                    run_id: None,
                    inbox: None,
                    state: update_persisted_state_from_direct(
                        persisted_state,
                        &self.remote_target_key(),
                        &snapshot,
                    ),
                })
            }
            SubmissionOutcome::Task(submitted_snapshot) => {
                if let Some(run_id) = &run_id {
                    self.in_flight_tasks
                        .lock()
                        .insert(run_id.clone(), submitted_snapshot.task_id.clone());
                }

                let completion = self.observe_to_completion(submitted_snapshot).await;

                if let Some(run_id) = &run_id {
                    self.in_flight_tasks.lock().remove(run_id);
                }

                let root_run = request.is_root();
                let completion_result = map_completion_result(completion?, root_run);
                let snapshot = completion_result.snapshot;
                Ok(BackendRunResult {
                    agent_id: request.agent_id().to_string(),
                    status: completion_result.status,
                    termination: completion_result.termination,
                    status_reason: completion_result.status_reason,
                    response: snapshot.output_text.clone(),
                    output: snapshot.output.clone(),
                    steps: 1,
                    run_id: None,
                    inbox: None,
                    state: update_persisted_state(
                        persisted_state,
                        &self.remote_target_key(),
                        &snapshot,
                    ),
                })
            }
        }
    }
}

enum PollCompletion {
    Finished(TaskSnapshot),
    TimedOut(TaskSnapshot),
}

struct CompletionResult {
    snapshot: TaskSnapshot,
    status: BackendRunStatus,
    termination: TerminationReason,
    status_reason: Option<String>,
}

fn map_completion_result(completion: PollCompletion, root_run: bool) -> CompletionResult {
    match completion {
        PollCompletion::TimedOut(snapshot) => CompletionResult {
            snapshot,
            status: BackendRunStatus::Timeout,
            termination: TerminationReason::Error("polling timeout exceeded".into()),
            status_reason: None,
        },
        PollCompletion::Finished(snapshot) => {
            let (status, termination, status_reason) = match snapshot.state {
                TaskState::Completed => (
                    BackendRunStatus::Completed,
                    TerminationReason::NaturalEnd,
                    None,
                ),
                TaskState::Canceled => (
                    BackendRunStatus::Cancelled,
                    TerminationReason::Cancelled,
                    None,
                ),
                TaskState::Failed => {
                    let message = snapshot
                        .failure_message
                        .clone()
                        .unwrap_or_else(|| "remote agent run failed".into());
                    (
                        BackendRunStatus::Failed(message.clone()),
                        TerminationReason::Error(message),
                        None,
                    )
                }
                TaskState::Rejected => {
                    let message = snapshot
                        .failure_message
                        .clone()
                        .unwrap_or_else(|| "remote agent rejected the task".into());
                    (
                        BackendRunStatus::Failed(message.clone()),
                        TerminationReason::Error(message),
                        None,
                    )
                }
                TaskState::InputRequired => {
                    interrupted_completion(snapshot.failure_message.clone(), root_run, false)
                }
                TaskState::AuthRequired => {
                    interrupted_completion(snapshot.failure_message.clone(), root_run, true)
                }
                TaskState::Submitted | TaskState::Working => (
                    BackendRunStatus::Failed("remote agent did not reach a terminal state".into()),
                    TerminationReason::Error("remote agent did not reach a terminal state".into()),
                    None,
                ),
            };
            CompletionResult {
                snapshot,
                status,
                termination,
                status_reason,
            }
        }
    }
}

fn interrupted_completion(
    failure_message: Option<String>,
    root_run: bool,
    auth_required: bool,
) -> (BackendRunStatus, TerminationReason, Option<String>) {
    let (default_message, wait_reason) = if auth_required {
        (
            "remote agent requires authentication",
            WAIT_REASON_AUTH_REQUIRED,
        )
    } else {
        (
            "remote agent requires additional input",
            WAIT_REASON_INPUT_REQUIRED,
        )
    };

    if root_run {
        let message = failure_message.clone();
        (
            if auth_required {
                BackendRunStatus::WaitingAuth(message)
            } else {
                BackendRunStatus::WaitingInput(message)
            },
            TerminationReason::Suspended,
            Some(wait_reason.to_string()),
        )
    } else {
        let message = failure_message.unwrap_or_else(|| default_message.into());
        (
            if auth_required {
                BackendRunStatus::WaitingAuth(Some(message.clone()))
            } else {
                BackendRunStatus::WaitingInput(Some(message.clone()))
            },
            TerminationReason::Error(message),
            None,
        )
    }
}

impl SubmissionOutcome {
    fn from_response(response: SendMessageResponse) -> Result<Self, ExecutionBackendError> {
        if let Some(task) = response.task {
            return Ok(Self::Task(TaskSnapshot::from_task(task)));
        }
        if let Some(message) = response.message {
            return Ok(Self::DirectMessage(DirectMessageSnapshot::from_message(
                message,
            )));
        }

        Err(ExecutionBackendError::RemoteError(
            "sendMessage response did not contain a task or message".into(),
        ))
    }
}

#[derive(Debug, Clone)]
struct DirectMessageSnapshot {
    task_id: Option<String>,
    context_id: Option<String>,
    output: BackendRunOutput,
}

impl DirectMessageSnapshot {
    fn from_message(message: A2aMessage) -> Self {
        let raw = serde_json::to_value(&message).ok();
        Self {
            task_id: message.task_id,
            context_id: message.context_id,
            output: BackendRunOutput {
                text: extract_text_from_parts(&message.parts),
                artifacts: Vec::new(),
                raw,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct TaskSnapshot {
    task_id: String,
    context_id: Option<String>,
    state: TaskState,
    output_text: Option<String>,
    output: BackendRunOutput,
    failure_message: Option<String>,
}

impl TaskSnapshot {
    fn from_task(task: Task) -> Self {
        let output_text = extract_output_text(&task);
        let output = extract_backend_output(&task, output_text.clone());
        let failure_message = task
            .status
            .message
            .as_ref()
            .and_then(extract_text_from_message)
            .or_else(|| default_failure_message(task.status.state));

        Self {
            task_id: task.id,
            context_id: Some(task.context_id),
            state: task.status.state,
            output_text,
            output,
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

    fn apply_stream_response(&mut self, response: StreamResponse) {
        if let Some(task) = response.task {
            *self = Self::from_task(task);
            return;
        }

        if let Some(update) = response.status_update {
            self.apply_status_update(update);
        }

        if let Some(update) = response.artifact_update {
            self.apply_artifact_update(update);
        }

        if let Some(message) = response.message
            && let Some(text) = extract_text_from_message(&message)
        {
            self.output_text = Some(text);
            self.output = BackendRunOutput {
                text: self.output_text.clone(),
                artifacts: self.output.artifacts.clone(),
                raw: serde_json::to_value(&message).ok(),
            };
        }
    }

    fn apply_status_update(&mut self, update: TaskStatusUpdateEvent) {
        self.task_id = update.task_id;
        self.context_id = Some(update.context_id);
        self.state = update.status.state;
        let message_text = update
            .status
            .message
            .as_ref()
            .and_then(extract_text_from_message);
        if matches!(
            self.state,
            TaskState::Failed
                | TaskState::Rejected
                | TaskState::Canceled
                | TaskState::InputRequired
                | TaskState::AuthRequired
        ) {
            self.failure_message =
                message_text.or_else(|| default_failure_message(update.status.state));
        } else {
            self.failure_message = None;
            if message_text.is_some() {
                self.output_text = message_text;
                self.output.text = self.output_text.clone();
            }
        }
    }

    fn apply_artifact_update(&mut self, update: TaskArtifactUpdateEvent) {
        self.task_id = update.task_id;
        self.context_id = Some(update.context_id);
        let Some(text) = extract_text_from_parts(&update.artifact.parts) else {
            return;
        };
        if update.append.unwrap_or(false) {
            match &mut self.output_text {
                Some(existing) if !existing.is_empty() => {
                    existing.push_str("\n\n");
                    existing.push_str(&text);
                }
                slot => *slot = Some(text),
            }
        } else {
            self.output_text = Some(text);
        }
        self.output.text = self.output_text.clone();
        self.output.artifacts.push(BackendOutputArtifact {
            id: Some(update.artifact.artifact_id),
            name: update.artifact.name,
            media_type: first_media_type(&update.artifact.parts),
            content: parts_to_json(update.artifact.parts),
        });
    }

    fn with_timeout_message(mut self) -> Self {
        self.failure_message = Some("polling timeout exceeded".to_string());
        self
    }
}

#[derive(Default)]
struct SseDataDecoder {
    buffer: String,
    pending_data: Vec<String>,
}

impl SseDataDecoder {
    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        while let Some(newline_index) = self.buffer.find('\n') {
            let mut line = self.buffer.drain(..=newline_index).collect::<String>();
            if let Some(stripped) = line.strip_suffix('\n') {
                line = stripped.to_string();
            }
            if let Some(stripped) = line.strip_suffix('\r') {
                line = stripped.to_string();
            }

            if line.is_empty() {
                if !self.pending_data.is_empty() {
                    events.push(self.pending_data.join("\n"));
                    self.pending_data.clear();
                }
                continue;
            }

            if line.starts_with(':') {
                continue;
            }

            if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                self.pending_data.push(data.to_string());
            }
        }

        events
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending_data.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending_data).join("\n"))
        }
    }
}

fn subscribe_requires_poll_fallback(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::METHOD_NOT_ALLOWED
            | reqwest::StatusCode::NOT_IMPLEMENTED
    )
}

fn parse_stream_response(payload: &str) -> Result<StreamResponse, ExecutionBackendError> {
    serde_json::from_str::<StreamResponse>(payload).map_err(|error| {
        ExecutionBackendError::RemoteError(format!(
            "failed to decode A2A subscribe event payload: {error}"
        ))
    })
}

fn default_failure_message(state: TaskState) -> Option<String> {
    if matches!(
        state,
        TaskState::Failed
            | TaskState::Rejected
            | TaskState::Canceled
            | TaskState::InputRequired
            | TaskState::AuthRequired
    ) {
        Some(format!("remote task ended in {}", task_state_name(state)))
    } else {
        None
    }
}

fn update_persisted_state(
    state: Option<PersistedState>,
    target_key: &str,
    snapshot: &TaskSnapshot,
) -> Option<PersistedState> {
    let mut persisted = state.unwrap_or(PersistedState {
        revision: 0,
        extensions: HashMap::new(),
    });
    let mut remote_state = persisted
        .extensions
        .remove(REMOTE_STATE_KEY)
        .and_then(|value| serde_json::from_value::<PersistedRemoteBackendState>(value).ok())
        .unwrap_or_default();

    remote_state.version = REMOTE_STATE_SCHEMA_VERSION;
    remote_state.targets.insert(
        target_key.to_string(),
        PersistedA2aThreadState {
            version: REMOTE_STATE_SCHEMA_VERSION,
            task_id: Some(snapshot.task_id.clone()),
            context_id: snapshot.context_id.clone(),
            last_state: Some(task_state_name(snapshot.state).to_string()),
            updated_at_ms: Some(now_ms()),
        },
    );

    persisted.extensions.insert(
        REMOTE_STATE_KEY.to_string(),
        serde_json::to_value(remote_state).ok()?,
    );
    Some(persisted)
}

fn update_persisted_state_from_direct(
    state: Option<PersistedState>,
    target_key: &str,
    snapshot: &DirectMessageSnapshot,
) -> Option<PersistedState> {
    if snapshot.task_id.is_none() && snapshot.context_id.is_none() {
        return state;
    }

    let mut persisted = state.unwrap_or(PersistedState {
        revision: 0,
        extensions: HashMap::new(),
    });
    let mut remote_state = persisted
        .extensions
        .remove(REMOTE_STATE_KEY)
        .and_then(|value| serde_json::from_value::<PersistedRemoteBackendState>(value).ok())
        .unwrap_or_default();
    let prior = remote_state
        .targets
        .get(target_key)
        .cloned()
        .unwrap_or_default();

    remote_state.version = REMOTE_STATE_SCHEMA_VERSION;
    remote_state.targets.insert(
        target_key.to_string(),
        PersistedA2aThreadState {
            version: REMOTE_STATE_SCHEMA_VERSION,
            task_id: snapshot.task_id.clone().or(prior.task_id),
            context_id: snapshot.context_id.clone().or(prior.context_id),
            last_state: Some("DIRECT_MESSAGE".to_string()),
            updated_at_ms: Some(now_ms()),
        },
    );

    persisted.extensions.insert(
        REMOTE_STATE_KEY.to_string(),
        serde_json::to_value(remote_state).ok()?,
    );
    Some(persisted)
}

fn read_remote_state_entry(
    state: &PersistedState,
    target_key: &str,
) -> Option<PersistedA2aThreadState> {
    state
        .extensions
        .get(REMOTE_STATE_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value::<PersistedRemoteBackendState>(value).ok())
        .and_then(|state| state.targets.get(target_key).cloned())
}

fn reusable_prior_task_id(
    state: &PersistedA2aThreadState,
    explicit_continuation: bool,
) -> Option<String> {
    if state
        .last_state
        .as_deref()
        .is_some_and(is_interrupted_remote_state)
        || (explicit_continuation && state.last_state.is_none())
    {
        state.task_id.clone()
    } else {
        None
    }
}

fn is_interrupted_remote_state(state: &str) -> bool {
    matches!(
        state,
        "TASK_STATE_INPUT_REQUIRED" | "TASK_STATE_AUTH_REQUIRED"
    )
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

fn extract_backend_output(task: &Task, text: Option<String>) -> BackendRunOutput {
    let artifacts = task
        .artifacts
        .iter()
        .map(|artifact| BackendOutputArtifact {
            id: Some(artifact.artifact_id.clone()),
            name: artifact.name.clone(),
            media_type: first_media_type(&artifact.parts),
            content: parts_to_json(artifact.parts.clone()),
        })
        .collect();
    BackendRunOutput {
        text,
        artifacts,
        raw: serde_json::to_value(task).ok(),
    }
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

fn first_media_type(parts: &[Part]) -> Option<String> {
    parts.iter().find_map(|part| part.media_type.clone())
}

fn parts_to_json(parts: Vec<Part>) -> Value {
    serde_json::to_value(parts).unwrap_or(Value::Null)
}

fn content_block_to_a2a_part(block: &ContentBlock) -> Option<Part> {
    match block {
        ContentBlock::Text { text } => Some(Part::text(text.clone())),
        ContentBlock::Image { source } => match source {
            ImageSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            ImageSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        ContentBlock::Document { source, title } => match source {
            DocumentSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: title.clone(),
                metadata: None,
            }),
            DocumentSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: title.clone(),
                metadata: None,
            }),
        },
        ContentBlock::Audio { source } => match source {
            AudioSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            AudioSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        ContentBlock::Video { source } => match source {
            VideoSource::Url { url } => Some(Part {
                text: None,
                raw: None,
                url: Some(url.clone()),
                data: None,
                media_type: Some(infer_media_type_from_url(url)),
                filename: None,
                metadata: None,
            }),
            VideoSource::Base64 { media_type, data } => Some(Part {
                text: None,
                raw: Some(data.clone()),
                url: None,
                data: None,
                media_type: Some(media_type.clone()),
                filename: None,
                metadata: None,
            }),
        },
        _ => None,
    }
}

fn infer_media_type_from_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".mp3") {
        "audio/mpeg".to_string()
    } else if lower.ends_with(".wav") {
        "audio/wav".to_string()
    } else if lower.ends_with(".mp4") {
        "video/mp4".to_string()
    } else if lower.ends_with(".pdf") {
        "application/pdf".to_string()
    } else {
        "application/octet-stream".to_string()
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
        let snapshot = TaskSnapshot::from_task(task);
        assert_eq!(snapshot.output.text.as_deref(), Some("hello\n\n world"));
        assert_eq!(snapshot.output.artifacts.len(), 1);
        assert_eq!(snapshot.output.artifacts[0].id.as_deref(), Some("response"));
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
        let err = SubmissionOutcome::from_response(SendMessageResponse::default()).unwrap_err();
        assert!(err.to_string().contains("task or message"));
    }

    #[test]
    fn send_message_response_preserves_direct_message_path() {
        let outcome = SubmissionOutcome::from_response(SendMessageResponse {
            message: Some(A2aMessage {
                task_id: None,
                context_id: None,
                message_id: "msg-1".into(),
                role: MessageRole::Agent,
                parts: vec![Part::text("hello")],
                metadata: None,
            }),
            ..Default::default()
        })
        .unwrap();

        let SubmissionOutcome::DirectMessage(snapshot) = outcome else {
            panic!("expected direct message outcome");
        };
        assert_eq!(snapshot.output.text.as_deref(), Some("hello"));
    }

    #[test]
    fn a2a_config_builder() {
        let config = A2aConfig::new("https://api.example.com/v1/a2a")
            .with_bearer_token("tok_123")
            .with_target_agent_id("worker")
            .with_poll_interval(Duration::from_millis(5000))
            .with_timeout(Duration::from_secs(60))
            .with_history_length(4)
            .with_return_immediately(false);

        assert_eq!(config.base_url, "https://api.example.com/v1/a2a");
        assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
        assert_eq!(config.target_agent_id.as_deref(), Some("worker"));
        assert_eq!(config.poll_interval, Duration::from_millis(5000));
        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.history_length, Some(4));
        assert!(!config.return_immediately);
    }

    #[test]
    fn a2a_config_try_from_remote_endpoint_reads_canonical_fields() {
        let mut options = BTreeMap::new();
        options.insert(POLL_INTERVAL_OPTION_KEY.into(), json!(1500));
        options.insert(HISTORY_LENGTH_OPTION_KEY.into(), json!(3));
        options.insert(RETURN_IMMEDIATELY_OPTION_KEY.into(), json!(false));
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
        assert_eq!(config.history_length, Some(3));
        assert!(!config.return_immediately);
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

        let _backend: Arc<dyn crate::backend::ExecutionBackend> = backend;
    }

    #[test]
    fn a2a_backend_factory_validates_endpoint_config_without_building() {
        A2aBackendFactory
            .validate(&RemoteEndpoint {
                backend: "a2a".into(),
                base_url: "https://api.example.com/v1/a2a".into(),
                ..Default::default()
            })
            .unwrap();

        let err = A2aBackendFactory
            .validate(&RemoteEndpoint {
                backend: "a2a".into(),
                base_url: "https://api.example.com/v1/a2a".into(),
                auth: Some(awaken_contract::registry_spec::RemoteAuth {
                    auth_type: "basic".into(),
                    params: BTreeMap::new(),
                }),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("only supports bearer auth"));
    }

    #[test]
    fn timed_out_poll_completion_maps_to_timeout_status() {
        let timed_out_snapshot = TaskSnapshot {
            task_id: "task-1".into(),
            context_id: Some("ctx-1".into()),
            state: TaskState::Working,
            output_text: Some("partial output".into()),
            output: BackendRunOutput::from_text(Some("partial output".into())),
            failure_message: Some("polling timeout exceeded".into()),
        };

        let result =
            map_completion_result(PollCompletion::TimedOut(timed_out_snapshot.clone()), true);

        assert!(matches!(result.status, BackendRunStatus::Timeout));
        assert_eq!(result.snapshot.output_text, timed_out_snapshot.output_text);
        assert!(matches!(
            result.termination,
            TerminationReason::Error(message) if message == "polling timeout exceeded"
        ));
        assert!(result.status_reason.is_none());
    }

    #[test]
    fn interrupted_root_poll_completion_maps_to_suspended_waiting_reason() {
        let input_required = TaskSnapshot {
            task_id: "task-1".into(),
            context_id: Some("ctx-1".into()),
            state: TaskState::InputRequired,
            output_text: Some("Need more details".into()),
            output: BackendRunOutput::from_text(Some("Need more details".into())),
            failure_message: Some("Need more details".into()),
        };
        let auth_required = TaskSnapshot {
            task_id: "task-2".into(),
            context_id: Some("ctx-2".into()),
            state: TaskState::AuthRequired,
            output_text: Some("Sign in first".into()),
            output: BackendRunOutput::from_text(Some("Sign in first".into())),
            failure_message: Some("Sign in first".into()),
        };

        let input_result = map_completion_result(PollCompletion::Finished(input_required), true);
        assert!(matches!(
            input_result.status,
            BackendRunStatus::WaitingInput(Some(ref message)) if message == "Need more details"
        ));
        assert_eq!(input_result.termination, TerminationReason::Suspended);
        assert_eq!(
            input_result.status_reason.as_deref(),
            Some(WAIT_REASON_INPUT_REQUIRED)
        );

        let auth_result = map_completion_result(PollCompletion::Finished(auth_required), true);
        assert!(matches!(
            auth_result.status,
            BackendRunStatus::WaitingAuth(Some(ref message)) if message == "Sign in first"
        ));
        assert_eq!(auth_result.termination, TerminationReason::Suspended);
        assert_eq!(
            auth_result.status_reason.as_deref(),
            Some(WAIT_REASON_AUTH_REQUIRED)
        );
    }

    #[test]
    fn interrupted_delegate_poll_completion_remains_failure() {
        let snapshot = TaskSnapshot {
            task_id: "task-1".into(),
            context_id: Some("ctx-1".into()),
            state: TaskState::InputRequired,
            output_text: None,
            output: BackendRunOutput::default(),
            failure_message: Some("Need more details".into()),
        };

        let result = map_completion_result(PollCompletion::Finished(snapshot), false);
        assert!(matches!(
            result.status,
            BackendRunStatus::WaitingInput(Some(ref message)) if message == "Need more details"
        ));
        assert!(matches!(
            result.termination,
            TerminationReason::Error(ref message) if message == "Need more details"
        ));
        assert!(result.status_reason.is_none());
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

    #[test]
    fn update_persisted_state_roundtrips_remote_task_binding() {
        let persisted = update_persisted_state(
            None,
            "a2a:https://gateway.example.com/v1/a2a/worker",
            &TaskSnapshot {
                task_id: "task-1".into(),
                context_id: Some("ctx-1".into()),
                state: TaskState::Completed,
                output_text: Some("done".into()),
                output: BackendRunOutput::from_text(Some("done".into())),
                failure_message: None,
            },
        )
        .expect("state should serialize");

        let remote =
            read_remote_state_entry(&persisted, "a2a:https://gateway.example.com/v1/a2a/worker")
                .expect("remote state entry");
        assert_eq!(remote.task_id.as_deref(), Some("task-1"));
        assert_eq!(remote.context_id.as_deref(), Some("ctx-1"));
        assert_eq!(remote.last_state.as_deref(), Some("TASK_STATE_COMPLETED"));
        assert_eq!(remote.version, REMOTE_STATE_SCHEMA_VERSION);
        assert!(remote.updated_at_ms.is_some());
    }

    #[test]
    fn completed_remote_task_is_not_reused_for_next_turn() {
        let state = PersistedA2aThreadState {
            task_id: Some("completed-task".into()),
            context_id: Some("ctx-1".into()),
            last_state: Some("TASK_STATE_COMPLETED".into()),
            ..Default::default()
        };

        assert_eq!(reusable_prior_task_id(&state, false), None);
    }

    #[test]
    fn interrupted_remote_task_is_reused_for_resume_turn() {
        let state = PersistedA2aThreadState {
            task_id: Some("waiting-task".into()),
            context_id: Some("ctx-1".into()),
            last_state: Some("TASK_STATE_INPUT_REQUIRED".into()),
            ..Default::default()
        };

        assert_eq!(
            reusable_prior_task_id(&state, false).as_deref(),
            Some("waiting-task")
        );
    }

    #[test]
    fn legacy_state_without_last_state_reuses_task_only_for_explicit_continuation() {
        let state = PersistedA2aThreadState {
            task_id: Some("legacy-task".into()),
            context_id: Some("ctx-1".into()),
            last_state: None,
            ..Default::default()
        };

        assert_eq!(reusable_prior_task_id(&state, false), None);
        assert_eq!(
            reusable_prior_task_id(&state, true).as_deref(),
            Some("legacy-task")
        );
    }

    #[test]
    fn update_persisted_state_from_direct_message_records_remote_ids() {
        let persisted = update_persisted_state_from_direct(
            None,
            "a2a:https://gateway.example.com/v1/a2a/worker",
            &DirectMessageSnapshot {
                task_id: Some("task-direct".into()),
                context_id: Some("ctx-direct".into()),
                output: BackendRunOutput::from_text(Some("done".into())),
            },
        )
        .expect("direct message state should serialize");

        let remote =
            read_remote_state_entry(&persisted, "a2a:https://gateway.example.com/v1/a2a/worker")
                .expect("remote state entry");
        assert_eq!(remote.task_id.as_deref(), Some("task-direct"));
        assert_eq!(remote.context_id.as_deref(), Some("ctx-direct"));
        assert_eq!(remote.last_state.as_deref(), Some("DIRECT_MESSAGE"));
    }

    #[test]
    fn update_persisted_state_from_direct_message_without_ids_keeps_state() {
        let original = PersistedState {
            revision: 7,
            extensions: HashMap::new(),
        };

        let persisted = update_persisted_state_from_direct(
            Some(original.clone()),
            "a2a:https://gateway.example.com/v1/a2a/worker",
            &DirectMessageSnapshot {
                task_id: None,
                context_id: None,
                output: BackendRunOutput::from_text(Some("done".into())),
            },
        )
        .expect("state should pass through");

        assert_eq!(persisted, original);
    }

    #[test]
    fn sse_decoder_collects_json_payloads() {
        let mut decoder = SseDataDecoder::default();
        let events = decoder.push(
            "data: {\"task\":{\"id\":\"task-1\"}}\n\
             \n\
             data: {\"statusUpdate\":{\"taskId\":\"task-1\"}}\n\
             \n",
        );
        assert_eq!(
            events,
            vec![
                "{\"task\":{\"id\":\"task-1\"}}".to_string(),
                "{\"statusUpdate\":{\"taskId\":\"task-1\"}}".to_string()
            ]
        );
    }

    #[test]
    fn stream_status_update_preserves_terminal_message() {
        let mut snapshot = TaskSnapshot::from_task(make_task(TaskState::Working));
        snapshot.apply_stream_response(StreamResponse {
            status_update: Some(TaskStatusUpdateEvent {
                task_id: "task-1".into(),
                context_id: "ctx-1".into(),
                status: awaken_protocol_a2a::TaskStatus {
                    state: TaskState::InputRequired,
                    message: Some(A2aMessage {
                        task_id: Some("task-1".into()),
                        context_id: Some("ctx-1".into()),
                        message_id: "msg-1".into(),
                        role: MessageRole::Agent,
                        parts: vec![Part::text("Need more details")],
                        metadata: None,
                    }),
                    timestamp: None,
                },
                metadata: None,
            }),
            ..Default::default()
        });

        assert_eq!(snapshot.state, TaskState::InputRequired);
        assert_eq!(
            snapshot.failure_message.as_deref(),
            Some("Need more details")
        );
    }

    #[test]
    fn stream_artifact_append_accumulates_output_text() {
        let mut snapshot = TaskSnapshot::from_task(make_task(TaskState::Working));
        snapshot.apply_stream_response(StreamResponse {
            artifact_update: Some(TaskArtifactUpdateEvent {
                task_id: "task-1".into(),
                context_id: "ctx-1".into(),
                artifact: awaken_protocol_a2a::Artifact {
                    artifact_id: "response".into(),
                    name: None,
                    description: None,
                    parts: vec![Part::text("hello")],
                    metadata: None,
                },
                append: Some(false),
                last_chunk: Some(false),
                metadata: None,
            }),
            ..Default::default()
        });
        snapshot.apply_stream_response(StreamResponse {
            artifact_update: Some(TaskArtifactUpdateEvent {
                task_id: "task-1".into(),
                context_id: "ctx-1".into(),
                artifact: awaken_protocol_a2a::Artifact {
                    artifact_id: "response".into(),
                    name: None,
                    description: None,
                    parts: vec![Part::text("world")],
                    metadata: None,
                },
                append: Some(true),
                last_chunk: Some(true),
                metadata: None,
            }),
            ..Default::default()
        });

        assert_eq!(snapshot.output_text.as_deref(), Some("hello\n\nworld"));
    }
}
