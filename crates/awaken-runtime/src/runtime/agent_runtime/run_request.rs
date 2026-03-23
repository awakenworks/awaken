//! RunRequest — unified request for starting or resuming a run.

use awaken_contract::contract::inference::InferenceOverride;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::ToolCallResume;

/// Unified request for starting or resuming a run.
pub struct RunRequest {
    /// New messages to append before running.
    pub messages: Vec<Message>,
    /// Thread ID. Existing → load history; new → create.
    pub thread_id: String,
    /// Target agent ID.
    /// `None` = infer from latest thread state/run record, fallback to default.
    pub agent_id: Option<String>,
    /// Runtime parameter overrides for this run.
    pub overrides: Option<InferenceOverride>,
    /// Resume decisions for suspended tool calls. Empty = fresh run.
    pub decisions: Vec<(String, ToolCallResume)>,
}

impl RunRequest {
    /// Build a message-first request with default options.
    pub fn new(thread_id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            messages,
            thread_id: thread_id.into(),
            agent_id: None,
            overrides: None,
            decisions: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: InferenceOverride) -> Self {
        self.overrides = Some(overrides);
        self
    }

    #[must_use]
    pub fn with_decisions(mut self, decisions: Vec<(String, ToolCallResume)>) -> Self {
        self.decisions = decisions;
        self
    }
}
