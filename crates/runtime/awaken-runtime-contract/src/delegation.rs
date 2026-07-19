//! Runtime interface for executing a first-class delegated child Run.
//!
//! Local and remote Agents implement the same lifecycle. Their adapters may
//! differ, but the runtime always supplies durable parent/call/child/result
//! identities and receives either an ended result or an awaiting continuation.

use async_trait::async_trait;
use awaken_agent_contract::agent::delegation::{DelegationOrigin, DelegationResultId};
use awaken_agent_contract::agent::run::Id as RunId;
use serde_json::Value;

use crate::CancellationToken;
use crate::llm::ThreadUsage;
use crate::runtime_context::RuntimeRunContext;

/// Start a child Run. `arguments` remains the model-visible tool payload because
/// the executor owns that tool's schema; identity and cancellation are typed.
pub struct DelegationRequest {
    pub origin: DelegationOrigin,
    pub child_run_id: RunId,
    pub result_id: DelegationResultId,
    pub arguments: Value,
    /// The same resolved execution wiring a directly initiated Run receives.
    pub context: RuntimeRunContext,
}

/// Resume a child Run that previously returned an opaque continuation.
pub struct DelegationResume {
    pub origin: DelegationOrigin,
    pub child_run_id: RunId,
    pub result_id: DelegationResultId,
    pub continuation: Value,
    pub input: String,
    pub context: RuntimeRunContext,
}

/// One durable boundary reached by a delegated child Run.
pub enum DelegationStep {
    /// The child Run ended and produced its terminal result.
    Ended { text: String, usage: ThreadUsage },
    /// The child Run awaits input under this opaque durable continuation.
    Awaiting { continuation: Value },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DelegationExecutionError(pub String);

impl DelegationExecutionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Runtime-facing interface for delegated child Runs. This is the runtime API,
/// not a generic "port": its methods name Agent-domain operations directly.
#[async_trait]
pub trait DelegationExecutor: Send + Sync {
    /// Model-visible delegation tool handled by this executor.
    fn tool_id(&self) -> &str;

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError>;

    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError>;
}

/// A registered Agent hosted outside this process. Protocol-specific task ids,
/// polling, and cancellation remain inside its adapter.
#[async_trait]
pub trait RemoteAgent: Send + Sync {
    async fn run(
        &self,
        agent_id: &str,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DelegationStep, DelegationExecutionError>;

    async fn card(&self, agent_id: &str) -> Result<Value, DelegationExecutionError>;
}
