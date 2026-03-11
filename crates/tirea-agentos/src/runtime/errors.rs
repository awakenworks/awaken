use crate::composition::AgentOsWiringError;
use crate::contracts::storage::ThreadStoreError;
use crate::loop_runtime::loop_runner::AgentLoopError;

#[derive(Debug, thiserror::Error)]
pub enum AgentOsResolveError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("provider not found: {provider_id} (for model id: {model_id})")]
    ProviderNotFound {
        provider_id: String,
        model_id: String,
    },

    #[error(transparent)]
    Wiring(#[from] AgentOsWiringError),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentOsRunError {
    #[error(transparent)]
    Resolve(#[from] AgentOsResolveError),

    #[error(transparent)]
    Loop(#[from] AgentLoopError),

    #[error("agent state store error: {0}")]
    ThreadStore(#[from] ThreadStoreError),

    #[error("agent state store not configured")]
    AgentStateStoreNotConfigured,
}
