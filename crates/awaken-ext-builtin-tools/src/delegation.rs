//! Delegation tool (ADR-0007): `agent_run`.
//!
//! `agent_run` delegates to another agent as an in-process sub-run. The runtime
//! owns sub-run execution (`RunIngress`), so the host injects a concrete
//! [`AgentRunner`] backed by it; the extension owns the model-visible tool and
//! the fail-closed roster check happens in the host's runner. One stable
//! descriptor id carries the target as an argument (never `agent_run_<id>`).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError};
use serde::Deserialize;

use crate::erasure::erase;

/// Run a delegate agent to completion and return its result text. The host backs
/// this with the runtime's sub-run execution and validates `agent_id` against the
/// resolved delegate roster (fail closed).
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(&self, agent_id: &str, input: &str) -> Result<String, ToolError>;
}

/// `agent_run` over an injected [`AgentRunner`].
pub struct AgentRunTool(Arc<dyn AgentRunner>);

impl AgentRunTool {
    pub fn new(runner: Arc<dyn AgentRunner>) -> Self {
        Self(runner)
    }
}

#[derive(Deserialize)]
pub struct AgentRunArgs {
    pub agent_id: String,
    pub input: String,
}

#[async_trait]
impl Tool for AgentRunTool {
    type Args = AgentRunArgs;
    type Output = String;
    fn id(&self) -> &str {
        "agent_run"
    }
    async fn call(&self, args: AgentRunArgs) -> Result<String, ToolError> {
        self.0.run(&args.agent_id, &args.input).await
    }
}

/// The delegation tool, erased for `Runtime::with_tool` registration.
pub fn delegation_tools(runner: Arc<dyn AgentRunner>) -> Vec<Arc<dyn RawTool>> {
    vec![erase(AgentRunTool::new(runner))]
}
