//! Runtime execution backends and canonical request/result types.

mod checkpoint;
mod local;

mod capabilities;
pub use capabilities::{
    BackendCancellationCapability, BackendContinuationCapability, BackendOutputCapability,
    BackendTranscriptCapability, BackendWaitCapability,
};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::contract::event::AgentEvent;
use awaken_runtime_contract::contract::event_sink::EventSink;
use awaken_runtime_contract::contract::identity::RunIdentity;
use awaken_runtime_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_runtime_contract::contract::message::{Message, gen_message_id};
use awaken_runtime_contract::contract::suspension::ToolCallResume;
use awaken_runtime_contract::contract::tool::ToolDescriptor;
use awaken_runtime_contract::now_ms;
use awaken_runtime_contract::registry_spec::RemoteEndpoint;
use awaken_runtime_contract::state::PersistedState;
use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cancellation::CancellationToken;
use crate::checkpoint_store::RuntimeCheckpointStore;
use crate::inbox::{InboxReceiver, InboxSender};
use crate::loop_runner::{AgentLoopError, AgentRunResult, PendingBoundaryHandler};
use crate::phase::PhaseRuntime;
use crate::{
    registry::{AgentResolver, ResolvedBackendAgent},
    resolution::ExecutionPlan,
};
use checkpoint::persist_remote_root_checkpoint;

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
    pub pending_boundary: Option<Arc<dyn PendingBoundaryHandler>>,
}

/// Root execution request shared by local and remote root execution.
pub struct BackendRootRunRequest<'a> {
    pub agent_id: &'a str,
    pub messages: Vec<Message>,
    pub new_messages: Vec<Message>,
    pub sink: Arc<dyn EventSink>,
    pub resolver: &'a dyn AgentResolver,
    pub run_identity: RunIdentity,
    pub checkpoint_store: Option<&'a dyn RuntimeCheckpointStore>,
    pub commit: crate::loop_runner::CommitWiring<'a>,
    pub control: BackendControl,
    pub decisions: Vec<(String, ToolCallResume)>,
    pub overrides: Option<awaken_runtime_contract::contract::inference::InferenceOverride>,
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
    pub resolver: &'a dyn AgentResolver,
    pub parent: BackendParentContext,
    pub control: BackendControl,
    pub policy: BackendDelegatePolicy,
    /// Initial state to seed the child run with before the first step.
    pub state_seed: Option<PersistedState>,
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
    /// Run-scoped persisted state, written to the run record.
    pub state: Option<PersistedState>,
    /// Thread-scoped persisted state (ADR-0038 C4), written to the per-thread
    /// `thread_state` store in the same commit. Only set by backends that own a
    /// live state registry (e.g. the in-process local backend) and can classify
    /// keys by scope; opaque remote/A2A backends leave this `None` and carry all
    /// keys on `state`.
    pub thread_state: Option<PersistedState>,
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
    fn capabilities(&self) -> crate::resolution::BackendProfile {
        crate::resolution::BackendProfile::remote_stateless_text()
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

/// JSON schema metadata for one version of a backend-specific agent config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionBackendConfigSchema {
    pub version: u32,
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub default_config: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_schema: Option<Value>,
}

impl ExecutionBackendConfigSchema {
    #[must_use]
    pub fn generic_remote() -> Self {
        Self {
            version: 1,
            schema: json!({
                "type": "object",
                "title": "Backend config",
                "additionalProperties": true,
            }),
            display_name: None,
            description: None,
            default_config: json!({}),
            ui_schema: None,
        }
    }
}

/// Schema for the built-in in-process Awaken backend.
#[must_use]
pub fn awaken_backend_config_schema() -> ExecutionBackendConfigSchema {
    ExecutionBackendConfigSchema {
        version: 1,
        schema: json!({
            "type": "object",
            "title": "Awaken backend config",
            "description": "In-process Awaken agent execution.",
            "additionalProperties": false,
            "required": ["model_id", "system_prompt", "max_rounds"],
            "properties": {
                "model_id": {
                    "type": "string",
                    "title": "Model",
                    "minLength": 1
                },
                "system_prompt": {
                    "type": "string",
                    "title": "System prompt"
                },
                "max_rounds": {
                    "type": "integer",
                    "title": "Max rounds",
                    "minimum": 1,
                    "default": 10
                }
            }
        }),
        display_name: Some("Awaken".to_string()),
        description: Some("Run the agent inside this Awaken runtime.".to_string()),
        default_config: json!({
            "model_id": "",
            "system_prompt": "",
            "max_rounds": 10,
        }),
        ui_schema: Some(json!({
            "system_prompt": {
                "ui:widget": "textarea"
            }
        })),
    }
}

/// Factory for backend implementations backed by canonical `RemoteEndpoint` config.
pub trait ExecutionBackendFactory: Send + Sync {
    fn backend(&self) -> &str;

    fn config_schema(&self) -> ExecutionBackendConfigSchema {
        ExecutionBackendConfigSchema::generic_remote()
    }

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
    execution: &ExecutionPlan,
) -> Result<crate::resolution::BackendProfile, ExecutionBackendError> {
    match execution {
        ExecutionPlan::Local(_) => Ok(LocalBackend::new().capabilities()),
        ExecutionPlan::Remote(agent) => Ok(agent.backend()?.capabilities()),
    }
}

fn root_request_requirements(
    request: &BackendRootRunRequest<'_>,
) -> crate::resolution::BackendRequirements {
    use crate::resolution::{
        BackendRequirements, ContinuationCapability, DecisionCapability, FrontendToolCapability,
        OverrideCapability,
    };
    let has_seeded = !request.decisions.is_empty();
    let has_live = request.control.decision_rx.is_some();
    let decisions = match (has_seeded, has_live) {
        (false, false) => None,
        (false, true) => Some(DecisionCapability::LiveOnly),
        (true, false) => Some(DecisionCapability::DurableResume),
        (true, true) => Some(DecisionCapability::LiveAndDurable),
    };
    BackendRequirements {
        cancellation: None,
        continuation: request
            .is_continuation
            .then_some(ContinuationCapability::InProcessState),
        decisions,
        overrides: request
            .overrides
            .as_ref()
            .map(|_| OverrideCapability::InferenceParams),
        frontend_tools: (!request.frontend_tools.is_empty())
            .then_some(FrontendToolCapability::DescriptorsOnly),
        persistence: None,
        waits: None,
        transcript: None,
        output: None,
    }
}

fn delegate_request_requirements(
    request: &BackendDelegateRunRequest<'_>,
) -> crate::resolution::BackendRequirements {
    use crate::resolution::{BackendRequirements, ContinuationCapability};
    BackendRequirements {
        cancellation: None,
        continuation: (request.policy.continuation != BackendDelegateContinuation::Disabled)
            .then_some(ContinuationCapability::InProcessState),
        decisions: None,
        overrides: None,
        frontend_tools: None,
        persistence: None,
        waits: None,
        transcript: None,
        output: None,
    }
}

fn check_profile(
    profile: &crate::resolution::BackendProfile,
    req: &crate::resolution::BackendRequirements,
    agent_id: &str,
) -> Result<(), ExecutionBackendError> {
    use crate::resolution::CapabilityDecision;
    if let CapabilityDecision::Unsupported(mismatches) = profile.check(req) {
        let listed = mismatches
            .iter()
            .map(|m| m.capability)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExecutionBackendError::ExecutionFailed(format!(
            "agent '{agent_id}' backend does not support: {listed}"
        )));
    }
    Ok(())
}

pub fn validate_root_execution_request(
    execution: &ExecutionPlan,
    request: &BackendRootRunRequest<'_>,
) -> Result<(), ExecutionBackendError> {
    let profile = execution_capabilities(execution)?;
    check_profile(
        &profile,
        &root_request_requirements(request),
        request.agent_id,
    )
}

pub fn validate_delegate_execution_request(
    execution: &ExecutionPlan,
    request: &BackendDelegateRunRequest<'_>,
) -> Result<(), ExecutionBackendError> {
    if request.policy.persistence != BackendDelegatePersistence::Ephemeral {
        return Err(ExecutionBackendError::ExecutionFailed(format!(
            "agent '{}' backend does not support: delegate_persistence",
            request.agent_id
        )));
    }
    if request.state_seed.is_some() && !matches!(execution, ExecutionPlan::Local(_)) {
        return Err(ExecutionBackendError::ExecutionFailed(format!(
            "agent '{}' backend does not support: delegate_state_seed",
            request.agent_id
        )));
    }
    let profile = execution_capabilities(execution)?;
    check_profile(
        &profile,
        &delegate_request_requirements(request),
        request.agent_id,
    )
}

pub async fn execute_resolved_delegate_execution(
    execution: &ExecutionPlan,
    request: BackendDelegateRunRequest<'_>,
) -> Result<BackendRunResult, ExecutionBackendError> {
    validate_delegate_execution_request(execution, &request)?;
    match execution {
        ExecutionPlan::Local(agent) => {
            LocalBackend::execute_resolved(agent.as_ref(), request).await
        }
        ExecutionPlan::Remote(agent) => agent.backend()?.execute_delegate(request).await,
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
    let commit = request.commit;
    let mut messages = request.messages.clone();
    let input_message_count = messages.len();
    let request_is_continuation = request.is_continuation;

    sink.emit(AgentEvent::RunStart {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
        identity: Some(run_identity.clone()),
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
                    let latest_state = load_checkpoint_state_for_active_remote_run(
                        checkpoint_store,
                        &run_identity.run_id,
                        previous_state.clone(),
                    )
                    .await?;
                    return finish_remote_root_run(
                        checkpoint_store,
                        &run_identity.thread_id,
                        &run_identity.run_id,
                        &run_identity.agent_id,
                        run_identity.parent_run_id.clone(),
                        &run_identity,
                        run_created_at,
                        messages,
                        input_message_count,
                        BackendRunStatus::Failed(error_message),
                        termination,
                        None,
                        0,
                        String::new(),
                        BackendRunOutput::default(),
                        latest_state,
                        None,
                        commit,
                        &sink,
                    )
                    .await;
                }
            }
        }
        _ = runtime_cancellation_token.cancelled() => {
            let latest_state = load_checkpoint_state_for_active_remote_run(
                checkpoint_store,
                &run_identity.run_id,
                previous_state.clone(),
            )
            .await?;
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
                &run_identity,
                run_created_at,
                messages,
                input_message_count,
                BackendRunStatus::Cancelled,
                TerminationReason::Cancelled,
                None,
                0,
                String::new(),
                BackendRunOutput::default(),
                latest_state,
                None,
                commit,
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
    let thread_state = delegate_result.thread_state;
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
        &run_identity,
        run_created_at,
        messages,
        input_message_count,
        status,
        termination,
        status_reason,
        steps,
        response,
        output,
        state,
        thread_state,
        commit,
        &sink,
    )
    .await
}

async fn load_checkpoint_state(
    storage: Option<&dyn RuntimeCheckpointStore>,
    run_id: &str,
    fallback: Option<PersistedState>,
) -> Result<Option<PersistedState>, AgentLoopError> {
    let Some(storage) = storage else {
        return Ok(fallback);
    };
    match storage.load_run(run_id).await {
        Ok(Some(run)) => Ok(run.state),
        Ok(None) => Err(AgentLoopError::StorageError(format!(
            "checkpoint state for run '{run_id}' was not found"
        ))),
        Err(error) => Err(AgentLoopError::StorageError(format!(
            "failed to load latest checkpoint state for run '{run_id}': {error}"
        ))),
    }
}

async fn load_checkpoint_state_for_active_remote_run(
    storage: Option<&dyn RuntimeCheckpointStore>,
    run_id: &str,
    fallback: Option<PersistedState>,
) -> Result<Option<PersistedState>, AgentLoopError> {
    if fallback.is_none() {
        return Ok(None);
    }
    load_checkpoint_state(storage, run_id, fallback).await
}

#[allow(clippy::too_many_arguments)]
async fn finish_remote_root_run(
    storage: Option<&dyn RuntimeCheckpointStore>,
    thread_id: &str,
    run_id: &str,
    agent_id: &str,
    parent_run_id: Option<String>,
    run_identity: &RunIdentity,
    run_created_at: u64,
    messages: Vec<Message>,
    input_message_count: usize,
    backend_status: BackendRunStatus,
    termination: TerminationReason,
    status_reason_override: Option<String>,
    steps: usize,
    response: String,
    output: BackendRunOutput,
    state: Option<PersistedState>,
    thread_state: Option<PersistedState>,
    commit: crate::loop_runner::CommitWiring<'_>,
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
        input_message_count,
        status,
        Some(termination.clone()),
        status_reason.clone(),
        (!response.is_empty()).then(|| response.clone()),
        match &termination {
            TerminationReason::Error(message) => Some(json!({ "message": message })),
            _ => None,
        },
        run_identity,
        steps,
        state,
        thread_state,
        commit,
    )
    .await?;

    sink.emit(AgentEvent::StepEnd).await;
    sink.emit(AgentEvent::RunFinish {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        identity: Some(run_identity.clone()),
        result: Some(result_json),
        termination: termination.clone(),
    })
    .await;

    Ok(AgentRunResult {
        run_id: run_id.to_string(),
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
        RunStatus::Created => "created",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Done => "done",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::storage::{RunRecord, StorageError};
    use awaken_runtime_contract::thread::Thread;

    struct FailingCheckpointStore;

    #[async_trait::async_trait]
    impl RuntimeCheckpointStore for FailingCheckpointStore {
        async fn load_thread(&self, _thread_id: &str) -> Result<Option<Thread>, StorageError> {
            Ok(None)
        }

        async fn load_messages(
            &self,
            _thread_id: &str,
        ) -> Result<Option<Vec<Message>>, StorageError> {
            Ok(None)
        }

        async fn load_committed_messages(
            &self,
            _thread_id: &str,
        ) -> Result<Option<Vec<Message>>, StorageError> {
            Ok(None)
        }

        async fn load_run(&self, _run_id: &str) -> Result<Option<RunRecord>, StorageError> {
            Err(StorageError::Io("injected read failure".into()))
        }

        async fn latest_run(&self, _thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
            Ok(None)
        }
    }

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

    #[tokio::test]
    async fn load_checkpoint_state_propagates_durable_read_errors() {
        let error = load_checkpoint_state(Some(&FailingCheckpointStore), "run-1", None)
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentLoopError::StorageError(ref message)
                if message.contains("failed to load latest checkpoint state")
                    && message.contains("injected read failure")),
            "unexpected error: {error}"
        );
    }

    struct MissingCheckpointStore;

    #[async_trait]
    impl RuntimeCheckpointStore for MissingCheckpointStore {
        async fn load_thread(&self, _thread_id: &str) -> Result<Option<Thread>, StorageError> {
            Ok(None)
        }

        async fn load_messages(
            &self,
            _thread_id: &str,
        ) -> Result<Option<Vec<Message>>, StorageError> {
            Ok(None)
        }

        async fn load_committed_messages(
            &self,
            _thread_id: &str,
        ) -> Result<Option<Vec<Message>>, StorageError> {
            Ok(None)
        }

        async fn load_run(&self, _run_id: &str) -> Result<Option<RunRecord>, StorageError> {
            Ok(None)
        }

        async fn latest_run(&self, _thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn load_checkpoint_state_rejects_missing_durable_run_even_with_fallback() {
        let fallback = PersistedState {
            revision: 9,
            extensions: std::collections::HashMap::new(),
        };
        let error = load_checkpoint_state(Some(&MissingCheckpointStore), "run-1", Some(fallback))
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentLoopError::StorageError(ref message)
                if message.contains("checkpoint state for run 'run-1' was not found")),
            "unexpected error: {error}"
        );
    }
}
