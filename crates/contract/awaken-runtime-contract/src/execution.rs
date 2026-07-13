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

/// How an executor can be stopped in flight. The host branches on this before it
/// offers cancel/interrupt for a run — the axis is worth typing because it differs
/// across execution altitudes (ADR-0055): the native loop observes a cooperative
/// token at a boundary; an ACP/A2A backend aborts an opaque remote turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancellation {
    /// The executor cannot be stopped in flight.
    None,
    /// A cooperative cancellation token observed at the next safe boundary (the
    /// native engine).
    CooperativeToken,
    /// The executor aborts an opaque remote/CLI turn (ACP interrupt / A2A cancel).
    RemoteAbort,
}

/// What an executor can pause a run to wait for (park-and-resume). Kept minimal —
/// only what the host branches on today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The executor never parks waiting for out-of-band input.
    None,
    /// It can park for input (a decision or a steered message).
    Input,
    /// It can park for authorization.
    Auth,
    /// It can park for input or authorization.
    Both,
}

/// The in-flight-control surface an executor supports, so the host adapts rather
/// than assuming the native-engine model for every backend (ADR-0055). Only the
/// axes the host branches on are modeled; more are added when a consumer needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    pub cancellation: Cancellation,
    pub wait: Wait,
}

impl ExecutorCapabilities {
    /// The native in-process loop: cooperative-token cancellation and durable
    /// park-and-resume for input or authorization.
    pub const NATIVE: Self = Self {
        cancellation: Cancellation::CooperativeToken,
        wait: Wait::Both,
    };
}

#[async_trait::async_trait]
pub trait RunExecutor: Send + Sync {
    async fn execute(
        &self,
        activation: crate::activation::RunActivation,
        context: crate::runtime_context::RuntimeRunContext,
    ) -> Result<Phase>;

    /// The in-flight-control surface this executor supports. Defaults to the
    /// native-engine model; a backend over an opaque remote/CLI turn overrides it.
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::NATIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DefaultExecutor;

    #[async_trait::async_trait]
    impl RunExecutor for DefaultExecutor {
        async fn execute(
            &self,
            _activation: crate::activation::RunActivation,
            _context: crate::runtime_context::RuntimeRunContext,
        ) -> Result<Phase> {
            unreachable!("capabilities-only test")
        }
    }

    #[test]
    fn default_capabilities_are_the_native_model() {
        let caps = DefaultExecutor.capabilities();
        assert_eq!(caps, ExecutorCapabilities::NATIVE);
        assert_eq!(caps.cancellation, Cancellation::CooperativeToken);
        assert_eq!(caps.wait, Wait::Both);
    }
}
