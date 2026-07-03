//! Tool execution ports.
//!
//! Two concerns stay separate (tool-and-capability.md): a tool *implementation*
//! (`Tool` typed, `RawTool` schema-erased) and the *executor* port the loop
//! calls to run one resolved call. Where a call physically runs is an
//! implementation detail of whoever implements `ToolExecutor` — owned by the
//! orchestration layer above — and stays out of the neutral runtime contract.

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Command as StateCommand;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::llm::ToolCall;

/// Neutral result of one tool invocation. `is_error` lets a tool return a
/// model-visible failure without aborting the run; `state` carries any state
/// transitions the tool wants staged onto the commit boundary (never a direct
/// store write — the same path hooks use).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<StateCommand>,
}

impl ToolOutput {
    pub fn ok(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
            state: Vec::new(),
        }
    }

    pub fn error(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: content.into(),
            is_error: true,
            state: Vec::new(),
        }
    }

    /// Stage state transitions to be committed with this tool's result.
    #[must_use]
    pub fn with_state(mut self, state: Vec<StateCommand>) -> Self {
        self.state = state;
        self
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
}

/// Schema-erased tool: the dynamic call boundary used by the runtime and by
/// MCP/server/client adapters. Concrete implementations live in
/// extension/adapter crates, never in neutral crates.
#[async_trait]
pub trait RawTool: Send + Sync {
    fn id(&self) -> &str;
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError>;
}

/// Preferred typed tool API. Authors implement this with concrete argument and
/// output types; an adapter erases it into a `RawTool` for execution.
#[async_trait]
pub trait Tool: Send + Sync {
    type Args: serde::de::DeserializeOwned + Send;
    type Output: Serialize + Send;

    fn id(&self) -> &str;
    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError>;
}

/// The port the execution loop calls to run one already-authorized tool call.
/// Where the call runs is hidden behind this port and owned by its implementer
/// (the orchestration layer above), not the runtime core.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError>;
}
