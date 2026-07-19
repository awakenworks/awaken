//! [`SubagentRunner`] — the neutral "run a catalog sub-agent" capability (ADR-0047 D5).
//!
//! Auxiliary agent runs — the compaction summarizer and the outcome judge — share
//! one shape: seed a named catalog agent, drive it to completion, take its last
//! assistant line. Each extension declares this one capability; the host
//! implements it once over the sub-run substrate, replacing the per-extension
//! `Summarizer` / `DelegateRunner` ports.
//!
//! Model-facing delegation ([`DelegationExecutor`](crate::delegation::DelegationExecutor)) stays
//! separate: it is a tool with awaiting/resume semantics, not a fire-and-return aux
//! run, so it is a distinct port by design.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;

use crate::CancellationToken;

/// A request to run an auxiliary sub-agent: which catalog `agent_id`, seeded with
/// `seed`, cancelled with the parent run.
pub struct SubagentRequest {
    pub agent_id: String,
    pub seed: Vec<Message>,
    pub cancellation: Option<CancellationToken>,
}

/// A sub-run's reply: its last assistant line, if any.
pub struct SubagentReply {
    pub text: Option<String>,
}

/// A sub-run failed to execute (unknown agent, sandbox/runtime/transport error).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct SubagentError(pub String);

/// Runs an auxiliary catalog sub-agent to completion. Declared by an extension
/// that needs an aux run; implemented once by the host over the sub-run substrate.
#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(&self, request: SubagentRequest) -> Result<SubagentReply, SubagentError>;
}
