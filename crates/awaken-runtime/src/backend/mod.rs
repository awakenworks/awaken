//! Runtime execution backends and canonical request/result types.

mod local;

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::{Message, gen_message_id};
use awaken_contract::contract::storage::{RunRecord, ThreadRunStore};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::now_ms;
use awaken_contract::registry_spec::RemoteEndpoint;
use awaken_contract::state::PersistedState;
use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cancellation::CancellationToken;
use crate::inbox::{InboxReceiver, InboxSender};
use crate::loop_runner::{AgentLoopError, AgentRunResult};
use crate::phase::PhaseRuntime;
use crate::registry::{ExecutionResolver, ResolvedBackendAgent, ResolvedExecution};

pub use local::LocalBackend;

const BACKEND_OUTPUT_STATE_KEY: &str = "__runtime_backend_output";

/// Optional parent lineage for a backend run.
#[derive(Debug, Clone, Default)]
pub struct BackendParentContext {
    pub parent_run_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub parent_tool_call_id: Option<String>,
}

/// Cooperative runtime controls exposed to a backend implementation.
#[derive(Default)]
pub struct BackendControl {
    pub cancellation_token: Option<CancellationToken>,
    pub decision_rx: Option<mpsc::UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
}

/// How a backend can be interrupted after execution starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCancellationCapability {
    None,
    CooperativeToken,
    RemoteAbort,
    CooperativeTokenAndRemoteAbort,
}

impl BackendCancellationCapability {
    #[must_use]
    pub const fn supports_cooperative_token(self) -> bool {
        matches!(
            self,
            Self::CooperativeToken | Self::CooperativeTokenAndRemoteAbort
        )
    }

    #[must_use]
    pub const fn supports_remote_abort(self) -> bool {
        matches!(
            self,
            Self::RemoteAbort | Self::CooperativeTokenAndRemoteAbort
        )
    }
}

/// How a backend maintains state across root turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendContinuationCapability {
    None,
    InProcessState,
    RemoteState,
}

/// Which interrupted states can be represented without flattening them to errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendWaitCapability {
    None,
    Input,
    Auth,
    InputAndAuth,
}

/// What transcript contract the backend consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendTranscriptCapability {
    FullTranscript,
    IncrementalUserMessagesWithRemoteState,
    SinglePrompt,
}

/// What output shape the backend preserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendOutputCapability {
    Text,
    TextAndArtifacts,
}

/// Optional execution capabilities exposed by a backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub cancellation: BackendCancellationCapability,
    pub decisions: bool,
    pub overrides: bool,
    pub frontend_tools: bool,
    pub continuation: BackendContinuationCapability,
    pub waits: BackendWaitCapability,
    pub transcript: BackendTranscriptCapability,
    pub output: BackendOutputCapability,
}

impl BackendCapabilities {
    #[must_use]
    pub const fn full() -> Self {
        Self {
            cancellation: BackendCancellationCapability::CooperativeToken,
            decisions: true,
            overrides: true,
            frontend_tools: true,
            continuation: BackendContinuationCapability::InProcessState,
            waits: BackendWaitCapability::InputAndAuth,
            transcript: BackendTranscriptCapability::FullTranscript,
            output: BackendOutputCapability::TextAndArtifacts,
        }
    }

    #[must_use]
    pub const fn remote_stateless_text() -> Self {
        Self {
            cancellation: BackendCancellationCapability::None,
            decisions: false,
            overrides: false,
            frontend_tools: false,
            continuation: BackendContinuationCapability::None,
            waits: BackendWaitCapability::None,
            transcript: BackendTranscriptCapability::SinglePrompt,
            output: BackendOutputCapability::Text,
        }
    }

    #[must_use]
    pub fn unsupported_root_features(
        &self,
        request: &BackendRootRunRequest<'_>,
    ) -> Vec<&'static str> {
        let mut unsupported = Vec::new();
        if (!request.decisions.is_empty() || request.control.decision_rx.is_some())
            && !self.decisions
        {
            unsupported.push("decisions");
        }
        if request.overrides.is_some() && !self.overrides {
            unsupported.push("overrides");
        }
        if !request.frontend_tools.is_empty() && !self.frontend_tools {
            unsupported.push("frontend_tools");
        }
        if request.is_continuation && self.continuation == BackendContinuationCapability::None {
            unsupported.push("continuation");
        }
        unsupported
    }

    #[must_use]
    pub fn unsupported_delegate_features(
        &self,
        request: &BackendDelegateRunRequest<'_>,
    ) -> Vec<&'static str> {
        let mut unsupported = Vec::new();
        if request.policy.persistence != BackendDelegatePersistence::Ephemeral {
            unsupported.push("delegate_persistence");
        }
        if request.policy.continuation != BackendDelegateContinuation::Disabled
            && self.continuation == BackendContinuationCapability::None
        {
            unsupported.push("continuation");
        }
        unsupported
    }
}

impl Default for BackendCapabilities {
    fn default() -> Self {
        Self::full()
    }
}

/// Root execution request shared by local and remote root execution.
pub struct BackendRootRunRequest<'a> {
    pub agent_id: &'a str,
    pub messages: Vec<Message>,
    pub new_messages: Vec<Message>,
    pub sink: Arc<dyn EventSink>,
    pub resolver: &'a dyn ExecutionResolver,
    pub run_identity: RunIdentity,
    pub checkpoint_store: Option<&'a dyn ThreadRunStore>,
    pub control: BackendControl,
    pub decisions: Vec<(String, ToolCallResume)>,
    pub overrides: Option<awaken_contract::contract::inference::InferenceOverride>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub local: Option<BackendLocalRootContext<'a>>,
    pub inbox: Option<InboxReceiver>,
    pub is_continuation: bool,
}

/// Local-only dependencies carried by the root request context.
#[derive(Clone, Copy)]
pub struct BackendLocalRootContext<'a> {
    pub phase_runtime: &'a PhaseRuntime,
}

/// Delegate execution persistence policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendDelegatePersistence {
    Ephemeral,
}

/// Delegate execution continuation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendDelegateContinuation {
    Disabled,
}

/// Explicit policy for delegated agent tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendDelegatePolicy {
    pub persistence: BackendDelegatePersistence,
    pub continuation: BackendDelegateContinuation,
}

impl Default for BackendDelegatePolicy {
    fn default() -> Self {
        Self {
            persistence: BackendDelegatePersistence::Ephemeral,
            continuation: BackendDelegateContinuation::Disabled,
        }
    }
}

/// Delegate execution request. Delegates are explicitly child invocations.
pub struct BackendDelegateRunRequest<'a> {
    pub agent_id: &'a str,
    pub messages: Vec<Message>,
    pub new_messages: Vec<Message>,
    pub sink: Arc<dyn EventSink>,
    pub resolver: &'a dyn ExecutionResolver,
    pub parent: BackendParentContext,
    pub control: BackendControl,
    pub policy: BackendDelegatePolicy,
}

/// Best-effort abort request for an in-flight backend execution.
pub struct BackendAbortRequest<'a> {
    pub agent_id: &'a str,
    pub run_identity: &'a RunIdentity,
    pub parent: Option<&'a BackendParentContext>,
    pub persisted_state: Option<&'a PersistedState>,
    pub is_continuation: bool,
}

/// Structured output preserved by a backend result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackendRunOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<BackendOutputArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

impl BackendRunOutput {
    #[must_use]
    pub fn from_text(text: Option<String>) -> Self {
        Self {
            text,
            artifacts: Vec::new(),
            raw: None,
        }
    }

    #[must_use]
    pub fn text_or<'a>(&'a self, fallback: &'a Option<String>) -> Option<String> {
        self.text.clone().or_else(|| fallback.clone())
    }
}

/// Backend artifact in a transport-neutral shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackendOutputArtifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub content: Value,
}

/// Result of executing an agent through a runtime backend.
#[derive(Debug, Clone)]
pub struct BackendRunResult {
    pub agent_id: String,
    pub status: BackendRunStatus,
    pub termination: TerminationReason,
    pub status_reason: Option<String>,
    pub response: Option<String>,
    pub output: BackendRunOutput,
    pub steps: usize,
    pub run_id: Option<String>,
    pub inbox: Option<InboxSender>,
    pub state: Option<PersistedState>,
}

/// Terminal status of a backend run.
#[derive(Debug, Clone)]
pub enum BackendRunStatus {
    Completed,
    WaitingInput(Option<String>),
    WaitingAuth(Option<String>),
    Suspended(Option<String>),
    Failed(String),
    Cancelled,
    Timeout,
}

impl std::fmt::Display for BackendRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
            Self::WaitingInput(Some(msg)) => write!(f, "waiting_input: {msg}"),
            Self::WaitingInput(None) => write!(f, "waiting_input"),
            Self::WaitingAuth(Some(msg)) => write!(f, "waiting_auth: {msg}"),
            Self::WaitingAuth(None) => write!(f, "waiting_auth"),
            Self::Suspended(Some(msg)) => write!(f, "suspended: {msg}"),
            Self::Suspended(None) => write!(f, "suspended"),
            Self::Failed(msg) => write!(f, "failed: {msg}"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Timeout => write!(f, "timeout"),
        }
    }
}

impl BackendRunStatus {
    #[must_use]
    pub fn durable_run_status(&self, termination: &TerminationReason) -> RunStatus {
        match self {
            Self::WaitingInput(_) | Self::WaitingAuth(_) | Self::Suspended(_) => RunStatus::Waiting,
            Self::Completed => termination.to_run_status().0,
            Self::Failed(_) | Self::Cancelled | Self::Timeout => RunStatus::Done,
        }
    }

    #[must_use]
    pub fn durable_status_reason(&self, termination: &TerminationReason) -> Option<String> {
        match self {
            Self::WaitingInput(_) => Some("input_required".to_string()),
            Self::WaitingAuth(_) => Some("auth_required".to_string()),
            Self::Suspended(_) => Some("suspended".to_string()),
            Self::Timeout => Some("timeout".to_string()),
            Self::Failed(_) => Some("error".to_string()),
            Self::Cancelled => Some("cancelled".to_string()),
            Self::Completed => termination.to_run_status().1,
        }
    }

    #[must_use]
    pub fn result_status_label(&self, termination: &TerminationReason) -> &'static str {
        match self {
            Self::Completed => run_status_label(termination.to_run_status().0),
            Self::WaitingInput(_) => "waiting_input",
            Self::WaitingAuth(_) => "waiting_auth",
            Self::Suspended(_) => "suspended",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
        }
    }
}

/// Backend for executing an agent, either locally or through a remote transport.
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_stateless_text()
    }

    async fn abort(&self, _request: BackendAbortRequest<'_>) -> Result<(), ExecutionBackendError> {
        Ok(())
    }

    async fn execute_root(
        &self,
        _request: BackendRootRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        Err(ExecutionBackendError::ExecutionFailed(
            "backend does not support root execution".into(),
        ))
    }

    async fn execute_delegate(
        &self,
        _request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError> {
        Err(ExecutionBackendError::ExecutionFailed(
            "backend does not support delegated execution".into(),
        ))
    }
}

/// Factory for backend implementations backed by canonical `RemoteEndpoint` config.
pub trait ExecutionBackendFactory: Send + Sync {
    fn backend(&self) -> &str;

    fn validate(&self, endpoint: &RemoteEndpoint) -> Result<(), ExecutionBackendFactoryError> {
        self.build(endpoint).map(|_| ())
    }

    fn build(
        &self,
        endpoint: &RemoteEndpoint,
    ) -> Result<Arc<dyn ExecutionBackend>, ExecutionBackendFactoryError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionBackendFactoryError {
    #[error("{0}")]
    InvalidConfig(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionBackendError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("remote error: {0}")]
    RemoteError(String),
    #[error(transparent)]
    Loop(#[from] AgentLoopError),
}

pub fn execution_capabilities(
    execution: &ResolvedExecution,
) -> Result<BackendCapabilities, ExecutionBackendError> {
    match execution {
        ResolvedExecution::Local(_) => Ok(LocalBackend::new().capabilities()),
        ResolvedExecution::NonLocal(agent) => Ok(agent.backend()?.capabilities()),
    }
}

pub fn validate_root_execution_request(
    execution: &ResolvedExecution,
    request: &BackendRootRunRequest<'_>,
) -> Result<(), ExecutionBackendError> {
    let unsupported = execution_capabilities(execution)?.unsupported_root_features(request);
    if !unsupported.is_empty() {
        return Err(ExecutionBackendError::ExecutionFailed(format!(
            "agent '{}' backend does not support: {}",
            request.agent_id,
            unsupported.join(", ")
        )));
    }
    Ok(())
}

pub fn validate_delegate_execution_request(
    execution: &ResolvedExecution,
    request: &BackendDelegateRunRequest<'_>,
) -> Result<(), ExecutionBackendError> {
    let unsupported = execution_capabilities(execution)?.unsupported_delegate_features(request);
    if !unsupported.is_empty() {
        return Err(ExecutionBackendError::ExecutionFailed(format!(
            "agent '{}' backend does not support: {}",
            request.agent_id,
            unsupported.join(", ")
        )));
    }
    Ok(())
}

pub async fn execute_resolved_delegate_execution(
    execution: &ResolvedExecution,
    request: BackendDelegateRunRequest<'_>,
) -> Result<BackendRunResult, ExecutionBackendError> {
    validate_delegate_execution_request(execution, &request)?;
    match execution {
        ResolvedExecution::Local(_) => LocalBackend::new().execute_delegate(request).await,
        ResolvedExecution::NonLocal(agent) => agent.backend()?.execute_delegate(request).await,
    }
}

/// Execute a remote root run including canonical runtime lifecycle events and persistence.
pub async fn execute_remote_root_lifecycle(
    agent: &ResolvedBackendAgent,
    request: BackendRootRunRequest<'_>,
    run_created_at: u64,
    runtime_cancellation_token: CancellationToken,
    previous_state: Option<PersistedState>,
) -> Result<AgentRunResult, AgentLoopError> {
    let backend = agent.backend().map_err(|error| {
        AgentLoopError::RuntimeError(crate::RuntimeError::ResolveFailed {
            message: error.to_string(),
        })
    })?;
    let run_identity = request.run_identity.clone();
    let sink = request.sink.clone();
    let checkpoint_store = request.checkpoint_store;
    let mut messages = request.messages.clone();
    let request_is_continuation = request.is_continuation;

    sink.emit(AgentEvent::RunStart {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
    })
    .await;
    sink.emit(AgentEvent::StepStart {
        message_id: gen_message_id(),
    })
    .await;

    let execution_started_at = now_ms();
    let backend_execution = backend.execute_root(request);
    tokio::pin!(backend_execution);
    let delegate_result = tokio::select! {
        result = &mut backend_execution => {
            match result {
                Ok(result) => result,
                Err(error) => {
                    let error_message = remote_backend_error_message(error);
                    let termination = TerminationReason::Error(error_message.clone());
                    let latest_state = load_checkpoint_state(
                        checkpoint_store,
                        &run_identity.run_id,
                        previous_state.clone(),
                    )
                    .await;
                    return finish_remote_root_run(
                        checkpoint_store,
                        &run_identity.thread_id,
                        &run_identity.run_id,
                        &run_identity.agent_id,
                        run_identity.parent_run_id.clone(),
                        run_created_at,
                        messages,
                        BackendRunStatus::Failed(error_message),
                        termination,
                        None,
                        0,
                        String::new(),
                        BackendRunOutput::default(),
                        latest_state,
                        &sink,
                    )
                    .await;
                }
            }
        }
        _ = runtime_cancellation_token.cancelled() => {
            let latest_state = load_checkpoint_state(
                checkpoint_store,
                &run_identity.run_id,
                previous_state.clone(),
            )
            .await;
            if backend.capabilities().cancellation.supports_remote_abort()
                && let Err(error) = backend
                    .abort(BackendAbortRequest {
                        agent_id: &run_identity.agent_id,
                        run_identity: &run_identity,
                        parent: None,
                        persisted_state: latest_state.as_ref(),
                        is_continuation: request_is_continuation,
                    })
                    .await
            {
                tracing::warn!(
                    agent_id = %run_identity.agent_id,
                    run_id = %run_identity.run_id,
                    error = %error,
                    "non-local backend abort hook failed after cancellation"
                );
            }
            return finish_remote_root_run(
                checkpoint_store,
                &run_identity.thread_id,
                &run_identity.run_id,
                &run_identity.agent_id,
                run_identity.parent_run_id.clone(),
                run_created_at,
                messages,
                BackendRunStatus::Cancelled,
                TerminationReason::Cancelled,
                None,
                0,
                String::new(),
                BackendRunOutput::default(),
                latest_state,
                &sink,
            )
            .await;
        }
    };

    let termination = delegate_result.termination.clone();
    let status_reason = delegate_result.status_reason.clone();
    let mut output = delegate_result.output.clone();
    let response = output
        .text_or(&delegate_result.response)
        .unwrap_or_default();
    if output.text.is_none() && !response.is_empty() {
        output.text = Some(response.clone());
    }
    let status = delegate_result.status;
    let steps = delegate_result.steps;
    let state = delegate_result.state.or(previous_state);
    if !response.is_empty() {
        sink.emit(AgentEvent::TextDelta {
            delta: response.clone(),
        })
        .await;
        messages.push(Message::assistant(response.clone()));
    }

    if matches!(
        termination,
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested
    ) {
        sink.emit(AgentEvent::InferenceComplete {
            model: agent.spec.model_id.clone(),
            usage: None,
            duration_ms: now_ms().saturating_sub(execution_started_at),
        })
        .await;
    }

    finish_remote_root_run(
        checkpoint_store,
        &run_identity.thread_id,
        &run_identity.run_id,
        &run_identity.agent_id,
        run_identity.parent_run_id.clone(),
        run_created_at,
        messages,
        status,
        termination,
        status_reason,
        steps,
        response,
        output,
        state,
        &sink,
    )
    .await
}

async fn load_checkpoint_state(
    storage: Option<&dyn ThreadRunStore>,
    run_id: &str,
    fallback: Option<PersistedState>,
) -> Option<PersistedState> {
    let Some(storage) = storage else {
        return fallback;
    };
    match storage.load_run(run_id).await {
        Ok(Some(run)) => run.state.or(fallback),
        Ok(None) => fallback,
        Err(error) => {
            tracing::warn!(run_id, error = %error, "failed to load latest checkpoint state");
            fallback
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_remote_root_run(
    storage: Option<&dyn ThreadRunStore>,
    thread_id: &str,
    run_id: &str,
    agent_id: &str,
    parent_run_id: Option<String>,
    run_created_at: u64,
    messages: Vec<Message>,
    backend_status: BackendRunStatus,
    termination: TerminationReason,
    status_reason_override: Option<String>,
    steps: usize,
    response: String,
    output: BackendRunOutput,
    state: Option<PersistedState>,
    sink: &Arc<dyn EventSink>,
) -> Result<AgentRunResult, AgentLoopError> {
    let status = backend_status.durable_run_status(&termination);
    let status_reason =
        status_reason_override.or_else(|| backend_status.durable_status_reason(&termination));
    let state = state_with_backend_output(state, &output);
    let mut result_json = json!({
        "response": response,
        "status": backend_status.result_status_label(&termination),
    });
    if output != BackendRunOutput::default() {
        result_json["output"] = serde_json::to_value(&output).unwrap_or(Value::Null);
    }
    if let Some(reason) = &status_reason {
        result_json["status_reason"] = Value::String(reason.clone());
    }

    persist_remote_root_checkpoint(
        storage,
        thread_id,
        run_id,
        agent_id,
        parent_run_id,
        run_created_at,
        &messages,
        status,
        status_reason,
        steps,
        state,
    )
    .await?;

    sink.emit(AgentEvent::StepEnd).await;
    sink.emit(AgentEvent::RunFinish {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        result: Some(result_json),
        termination: termination.clone(),
    })
    .await;

    Ok(AgentRunResult {
        response,
        termination,
        steps,
    })
}

fn state_with_backend_output(
    state: Option<PersistedState>,
    output: &BackendRunOutput,
) -> Option<PersistedState> {
    if output == &BackendRunOutput::default() {
        return state;
    }

    let mut state = state.unwrap_or(PersistedState {
        revision: 0,
        extensions: std::collections::HashMap::new(),
    });
    if let Ok(value) = serde_json::to_value(output) {
        state
            .extensions
            .insert(BACKEND_OUTPUT_STATE_KEY.to_string(), value);
    }
    Some(state)
}

#[allow(clippy::too_many_arguments)]
async fn persist_remote_root_checkpoint(
    storage: Option<&dyn ThreadRunStore>,
    thread_id: &str,
    run_id: &str,
    agent_id: &str,
    parent_run_id: Option<String>,
    run_created_at: u64,
    messages: &[Message],
    status: RunStatus,
    status_reason: Option<String>,
    steps: usize,
    state: Option<PersistedState>,
) -> Result<(), AgentLoopError> {
    let Some(storage) = storage else {
        return Ok(());
    };

    let record = RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: agent_id.to_string(),
        parent_run_id,
        status,
        termination_code: status_reason,
        created_at: run_created_at / 1000,
        updated_at: now_ms() / 1000,
        steps,
        input_tokens: 0,
        output_tokens: 0,
        state,
    };
    storage
        .checkpoint(thread_id, messages, &record)
        .await
        .map_err(|error| AgentLoopError::StorageError(error.to_string()))
}

fn remote_backend_error_message(error: ExecutionBackendError) -> String {
    match error {
        ExecutionBackendError::AgentNotFound(message)
        | ExecutionBackendError::ExecutionFailed(message)
        | ExecutionBackendError::RemoteError(message) => message,
        ExecutionBackendError::Loop(error) => error.to_string(),
    }
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_status_timeout_is_first_class_at_runtime_boundary() {
        let status = BackendRunStatus::Timeout;

        assert_eq!(
            status.durable_run_status(&TerminationReason::Error("polling timeout exceeded".into())),
            RunStatus::Done
        );
        assert_eq!(
            status
                .durable_status_reason(&TerminationReason::Error("polling timeout exceeded".into()))
                .as_deref(),
            Some("timeout")
        );
        assert_eq!(
            status
                .result_status_label(&TerminationReason::Error("polling timeout exceeded".into())),
            "timeout"
        );
    }

    #[test]
    fn backend_status_waiting_is_first_class_at_runtime_boundary() {
        let status = BackendRunStatus::WaitingInput(Some("need details".into()));

        assert_eq!(
            status.durable_run_status(&TerminationReason::Error("should not win".into())),
            RunStatus::Waiting
        );
        assert_eq!(
            status
                .durable_status_reason(&TerminationReason::Error("should not win".into()))
                .as_deref(),
            Some("input_required")
        );
        assert_eq!(
            status.result_status_label(&TerminationReason::Error("should not win".into())),
            "waiting_input"
        );
    }
}
