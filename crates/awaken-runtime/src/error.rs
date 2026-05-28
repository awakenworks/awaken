use awaken_runtime_contract::StateError;
use thiserror::Error;

/// Runtime-specific errors that wrap [`StateError`] and add variants
/// for agent resolution and run management.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("thread already has an active run: {thread_id}")]
    ThreadAlreadyRunning { thread_id: String },
    #[error("agent not found: {agent_id}")]
    AgentNotFound { agent_id: String },
    #[error("resolve failed: {message}")]
    ResolveFailed { message: String },
}
