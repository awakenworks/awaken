//! RunRequest — unified request for starting or resuming a run.

use awaken_contract::contract::inference::InferenceOverride;
use awaken_contract::contract::mailbox::MailboxJobOrigin;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool::ToolDescriptor;

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
    /// Frontend-defined tools for this run.
    pub frontend_tools: Vec<ToolDescriptor>,
    /// Where this request originated.
    pub origin: MailboxJobOrigin,
    /// Parent run ID for child run linkage (tracing/lineage).
    pub parent_run_id: Option<String>,
    /// Parent thread ID for message routing back to parent.
    pub parent_thread_id: Option<String>,
    /// Continue a previous run instead of creating a new one.
    pub continue_run_id: Option<String>,
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
            frontend_tools: Vec::new(),
            origin: MailboxJobOrigin::User,
            parent_run_id: None,
            parent_thread_id: None,
            continue_run_id: None,
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

    #[must_use]
    pub fn with_frontend_tools(mut self, tools: Vec<ToolDescriptor>) -> Self {
        self.frontend_tools = tools;
        self
    }

    #[must_use]
    pub fn with_origin(mut self, origin: MailboxJobOrigin) -> Self {
        self.origin = origin;
        self
    }

    #[must_use]
    pub fn with_parent_run_id(mut self, parent_run_id: impl Into<String>) -> Self {
        self.parent_run_id = Some(parent_run_id.into());
        self
    }

    #[must_use]
    pub fn with_parent_thread_id(mut self, parent_thread_id: impl Into<String>) -> Self {
        self.parent_thread_id = Some(parent_thread_id.into());
        self
    }

    #[must_use]
    pub fn with_continue_run_id(mut self, continue_run_id: impl Into<String>) -> Self {
        self.continue_run_id = Some(continue_run_id.into());
        self
    }
}
