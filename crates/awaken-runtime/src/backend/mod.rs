//! Runtime execution backends and canonical request/result types.

mod local;

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::registry_spec::RemoteEndpoint;
use futures::channel::mpsc;

use crate::cancellation::CancellationToken;
use crate::inbox::{InboxReceiver, InboxSender};
use crate::phase::PhaseRuntime;
use crate::registry::AgentResolver;

pub use local::LocalBackend;

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

/// Canonical request surface for root and delegated backend execution.
pub struct BackendRunRequest<'a> {
    pub agent_id: &'a str,
    pub messages: Vec<Message>,
    pub sink: Arc<dyn EventSink>,
    pub resolver: &'a dyn AgentResolver,
    pub run_identity: Option<RunIdentity>,
    pub parent: Option<BackendParentContext>,
    pub phase_runtime: Option<&'a PhaseRuntime>,
    pub checkpoint_store: Option<&'a dyn ThreadRunStore>,
    pub control: BackendControl,
    pub overrides: Option<awaken_contract::contract::inference::InferenceOverride>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub inbox: Option<InboxReceiver>,
    pub is_continuation: bool,
}

/// Result of executing an agent through a runtime backend.
#[derive(Debug, Clone)]
pub struct BackendRunResult {
    pub agent_id: String,
    pub status: BackendRunStatus,
    pub response: Option<String>,
    pub steps: usize,
    pub run_id: Option<String>,
    pub inbox: Option<InboxSender>,
}

/// Terminal status of a backend run.
#[derive(Debug, Clone)]
pub enum BackendRunStatus {
    Completed,
    Failed(String),
    Cancelled,
    Timeout,
}

impl std::fmt::Display for BackendRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
            Self::Failed(msg) => write!(f, "failed: {msg}"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Timeout => write!(f, "timeout"),
        }
    }
}

/// Backend for executing an agent, either locally or through a remote transport.
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    async fn execute(
        &self,
        request: BackendRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError>;
}

/// Factory for backend implementations backed by canonical `RemoteEndpoint` config.
pub trait ExecutionBackendFactory: Send + Sync {
    fn backend(&self) -> &str;

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
}
