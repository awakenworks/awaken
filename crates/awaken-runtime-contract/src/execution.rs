use thiserror::Error;

use awaken_agent_contract::agent::run::Phase;

#[derive(Debug, Error)]
pub enum Error {
    #[error("runtime resolution failed: {0}")]
    Resolution(String),
    #[error("runtime execution failed: {0}")]
    Execution(String),
    #[error("runtime commit failed: {0}")]
    Commit(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[async_trait::async_trait]
pub trait RunExecutor: Send + Sync {
    async fn execute(
        &self,
        activation: crate::activation::RunActivation,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<Phase>;
}
