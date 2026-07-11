//! The delegation port: run a sub-agent behind the model-visible delegation tool.
//!
//! The runtime routes the tool whose id is [`AgentResolver::tool_id`] to this port
//! instead of the tool registry, so the kernel executes delegation as a first-class
//! concern without ever naming a concrete tool id. Native (in-process) and remote
//! (A2A) agents are *peer* implementations chosen by `agent_id`: the resolver is a
//! gateway over a heterogeneous agent space, and the caller cannot tell a local
//! sub-run from a remote one.

use async_trait::async_trait;
use serde_json::Value;

use crate::CancellationToken;
use crate::llm::ThreadUsage;

/// A request to run a delegate. `arguments` is the raw tool-call payload (the
/// resolver, which owns the delegation tool's schema, reads the target agent and
/// input from it) — so the kernel stays agnostic of the delegate arg shape.
pub struct AgentRequest {
    pub arguments: Value,
    /// Cancelled when the parent run is interrupted, so the delegate stops too.
    pub cancellation: Option<CancellationToken>,
}

/// One step of a delegated agent.
pub enum AgentStep {
    /// The delegate finished with this reply text. `usage` is the delegate's own
    /// token spend (per model), which the kernel folds into the parent thread's
    /// running total so a session's usage counts delegated work — empty for a
    /// remote delegate or a deterministic model that reported none.
    Done { text: String, usage: ThreadUsage },
    /// The delegate parked needing more input; `handle` is opaque, durable state
    /// used to resume it (e.g. a remote task id). The parent parks until the
    /// delegation is resumed with new input.
    Parked { handle: Value },
}

/// A delegation failure (the delegate could not be run).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AgentError(pub String);

impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Runs sub-agents behind the delegation tool. Native and remote agents are peers,
/// selected by `agent_id`. The kernel owns *when* a delegation runs, parks, or
/// resumes; this port owns *how* the chosen agent is run.
#[async_trait]
pub trait AgentResolver: Send + Sync {
    /// The delegation tool id this resolver backs (e.g. `agent_run`). The kernel
    /// routes that tool to the resolver instead of the tool registry, so the kernel
    /// never hard-codes a concrete tool id.
    fn tool_id(&self) -> &str;

    /// Run a delegation turn for `request.agent_id`.
    async fn run(&self, request: AgentRequest) -> Result<AgentStep, AgentError>;

    /// Resume a parked delegation (from a prior [`AgentStep::Parked`] `handle`) with
    /// new user `input`.
    async fn resume(
        &self,
        handle: &Value,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<AgentStep, AgentError>;
}
