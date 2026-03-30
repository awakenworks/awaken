//! Agent delegation backend trait and shared types.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::Message;

/// Result of a sub-agent execution.
#[derive(Debug, Clone)]
pub struct DelegateRunResult {
    /// ID of the agent that ran.
    pub agent_id: String,
    /// Execution status.
    pub status: DelegateRunStatus,
    /// Final response text (if any).
    pub response: Option<String>,
    /// Number of steps executed.
    pub steps: usize,
    /// Child run ID for observability lineage (local delegates only).
    pub run_id: Option<String>,
}

/// Terminal status of a delegated agent run.
#[derive(Debug, Clone)]
pub enum DelegateRunStatus {
    /// Agent completed successfully.
    Completed,
    /// Agent execution failed.
    Failed(String),
    /// Agent was cancelled.
    Cancelled,
    /// Agent timed out.
    Timeout,
}

impl std::fmt::Display for DelegateRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegateRunStatus::Completed => write!(f, "completed"),
            DelegateRunStatus::Failed(msg) => write!(f, "failed: {msg}"),
            DelegateRunStatus::Cancelled => write!(f, "cancelled"),
            DelegateRunStatus::Timeout => write!(f, "timeout"),
        }
    }
}

/// Backend for executing agent delegation -- local or remote.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Execute a sub-agent with the given messages and return the result.
    ///
    /// The `event_sink` receives filtered child events (e.g. tool-call progress)
    /// that should be forwarded to the parent's event stream.
    ///
    /// `parent_run_id` and `parent_tool_call_id` link the sub-agent's identity
    /// back to the parent run, enabling progress lineage tree construction.
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError>;
}

/// Errors from agent backend execution.
#[derive(Debug, thiserror::Error)]
pub enum AgentBackendError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("remote error: {0}")]
    RemoteError(String),
}
