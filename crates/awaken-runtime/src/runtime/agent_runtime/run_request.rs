//! RunRequest — unified request for starting or resuming a run.

use std::collections::HashMap;

use crate::inbox::{InboxReceiver, InboxSender};
use awaken_contract::contract::inference::InferenceOverride;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, RunRequestOrigin};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::contract::tool::ToolDescriptor;
use awaken_contract::contract::tool_intercept::{AdapterKind, RunMode};

/// Read-only snapshot of cached thread state, passed from mailbox to runtime.
#[non_exhaustive]
pub struct ThreadContextSnapshot {
    pub messages: Vec<Message>,
    pub latest_run: Option<RunRecord>,
    pub run_cache: HashMap<String, RunRecord>,
}

impl ThreadContextSnapshot {
    #[must_use]
    pub fn new(
        messages: Vec<Message>,
        latest_run: Option<RunRecord>,
        run_cache: HashMap<String, RunRecord>,
    ) -> Self {
        Self {
            messages,
            latest_run,
            run_cache,
        }
    }
}

/// In-process inbox pair owned by a single run.
pub struct RunInbox {
    pub sender: InboxSender,
    pub receiver: InboxReceiver,
}

/// Unified request for starting or resuming a run.
pub struct RunRequest {
    /// New messages to append before running.
    pub messages: Vec<Message>,
    /// True when `messages` already exist in the thread message log.
    ///
    /// Mailbox-backed dispatch reconstructs new input messages from
    /// `RunRecord.request` and the thread log, so the runtime must use them as
    /// the current turn without appending a duplicate copy.
    pub messages_already_persisted: bool,
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
    pub origin: RunRequestOrigin,
    /// Execution mode used by framework-level policy hooks.
    pub run_mode: RunMode,
    /// Protocol or adapter that submitted this request.
    pub adapter: AdapterKind,
    /// Parent run ID for child run linkage (tracing/lineage).
    pub parent_run_id: Option<String>,
    /// Parent thread ID for message routing back to parent.
    pub parent_thread_id: Option<String>,
    /// Continue a previous run instead of creating a new one.
    pub continue_run_id: Option<String>,
    /// Optional canonical run ID preallocated by the caller.
    pub run_id_hint: Option<String>,
    /// Optional transport dispatch/task ID that should be used as the run ID.
    pub dispatch_id_hint: Option<String>,
    /// Queue dispatch that delivered this run request, if any.
    pub dispatch_id: Option<String>,
    /// External session/dispatch identifier associated with this run.
    pub session_id: Option<String>,
    /// Transport request identifier associated with this run.
    pub transport_request_id: Option<String>,
    /// Optional in-process inbox pair for background-task notifications.
    pub run_inbox: Option<RunInbox>,
}

impl RunRequest {
    /// Build a message-first request with default options.
    pub fn new(thread_id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            messages,
            messages_already_persisted: false,
            thread_id: thread_id.into(),
            agent_id: None,
            overrides: None,
            decisions: Vec::new(),
            frontend_tools: Vec::new(),
            origin: RunRequestOrigin::User,
            run_mode: RunMode::Foreground,
            adapter: AdapterKind::Internal,
            parent_run_id: None,
            parent_thread_id: None,
            continue_run_id: None,
            run_id_hint: None,
            dispatch_id_hint: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            run_inbox: None,
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
    pub fn with_origin(mut self, origin: RunRequestOrigin) -> Self {
        self.origin = origin;
        self
    }

    #[must_use]
    pub fn with_run_mode(mut self, run_mode: RunMode) -> Self {
        self.run_mode = run_mode;
        self
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter: AdapterKind) -> Self {
        self.adapter = adapter;
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

    #[must_use]
    pub fn with_run_id_hint(mut self, run_id_hint: impl Into<String>) -> Self {
        self.run_id_hint = Some(run_id_hint.into());
        self
    }

    #[must_use]
    pub fn with_dispatch_id_hint(mut self, dispatch_id_hint: impl Into<String>) -> Self {
        self.dispatch_id_hint = Some(dispatch_id_hint.into());
        self
    }

    #[must_use]
    pub fn with_trace_dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = Some(dispatch_id.into());
        self
    }

    #[must_use]
    pub fn with_dispatch_id(mut self, dispatch_id: impl Into<String>) -> Self {
        self.dispatch_id = Some(dispatch_id.into());
        self
    }

    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    #[must_use]
    pub fn with_transport_request_id(mut self, transport_request_id: impl Into<String>) -> Self {
        self.transport_request_id = Some(transport_request_id.into());
        self
    }

    #[must_use]
    pub fn with_inbox(mut self, sender: InboxSender, receiver: InboxReceiver) -> Self {
        self.run_inbox = Some(RunInbox { sender, receiver });
        self
    }

    #[must_use]
    pub fn with_messages_already_persisted(mut self, value: bool) -> Self {
        self.messages_already_persisted = value;
        self
    }
}
