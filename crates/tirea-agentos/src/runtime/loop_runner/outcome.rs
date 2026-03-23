use super::*;
use serde_json::{json, Value};

/// Aggregated token usage for one loop run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub thinking_tokens: usize,
    pub total_tokens: usize,
}

/// Aggregated runtime metrics for one loop run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopStats {
    pub duration_ms: u64,
    pub steps: usize,
    pub llm_calls: usize,
    pub llm_retries: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LoopFailure {
    Llm(String),
    State(String),
}

/// Unified terminal state for loop execution.
#[derive(Debug)]
pub struct LoopOutcome {
    pub run_ctx: crate::contracts::RunContext,
    pub termination: TerminationReason,
    pub response: Option<String>,
    pub usage: LoopUsage,
    pub stats: LoopStats,
    /// Error details when `termination` is `Error`. Read in tests.
    #[allow(dead_code)]
    pub(super) failure: Option<LoopFailure>,
}

impl LoopOutcome {
    /// Build a `RunFinish.result` payload from the unified outcome.
    pub fn run_finish_result(&self) -> Option<Value> {
        if !matches!(self.termination, TerminationReason::NaturalEnd) {
            return None;
        }
        self.response
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|text| json!({ "response": text }))
    }

    /// Project unified outcome into stream `RunFinish` event.
    pub fn to_run_finish_event(self, run_id: String) -> AgentEvent {
        AgentEvent::RunFinish {
            thread_id: self.run_ctx.thread_id().to_string(),
            run_id,
            result: self.run_finish_result(),
            termination: self.termination,
        }
    }
}

/// Error type for agent loop operations.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("LLM error: {0}")]
    LlmError(String),
    #[error("State error: {0}")]
    StateError(String),
    /// External cancellation signal requested run termination.
    #[error("Run cancelled")]
    Cancelled,
}

impl From<crate::contracts::runtime::ToolExecutorError> for AgentLoopError {
    fn from(value: crate::contracts::runtime::ToolExecutorError) -> Self {
        match value {
            crate::contracts::runtime::ToolExecutorError::Cancelled { .. } => Self::Cancelled,
            crate::contracts::runtime::ToolExecutorError::Failed { message } => {
                Self::StateError(message)
            }
        }
    }
}

/// Helper to create a tool map from an iterator of tools.
pub fn tool_map<I, T>(tools: I) -> HashMap<String, Arc<dyn Tool>>
where
    I: IntoIterator<Item = T>,
    T: Tool + 'static,
{
    tools
        .into_iter()
        .map(|t| {
            let name = t.descriptor().id.clone();
            (name, Arc::new(t) as Arc<dyn Tool>)
        })
        .collect()
}

/// Helper to create a tool map from `Arc<dyn Tool>`.
pub fn tool_map_from_arc<I>(tools: I) -> HashMap<String, Arc<dyn Tool>>
where
    I: IntoIterator<Item = Arc<dyn Tool>>,
{
    tools
        .into_iter()
        .map(|t| (t.descriptor().id.clone(), t))
        .collect()
}
