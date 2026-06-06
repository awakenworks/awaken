//! Background task management for agent tools.
//!
//! Provides a system for spawning, tracking, cancelling, and querying
//! background tasks. Tasks are tracked in-memory and outlive individual runs.

mod cancel_task_tool;
mod execution_context;
mod hook;
mod manager;
mod plugin;
mod run_cancellation;
mod send_message_tool;
pub(crate) mod state;
mod types;

pub(crate) use cancel_task_tool::CANCEL_TASK_TOOL_ID;
pub use cancel_task_tool::CancelTaskTool;
pub use execution_context::current_background_task_id;
pub(crate) use execution_context::{
    BackgroundTaskExecutionContext, ToolLineageContext, current_background_task_context,
    current_durable_message_sink, current_tool_lineage_context, scope_background_task_context,
    scope_durable_message_sink, scope_tool_lineage_context,
};
pub use manager::{BackgroundTaskManager, SendError, SpawnError};
pub use plugin::BackgroundTaskPlugin;
pub(crate) use run_cancellation::{
    dedup_managers, managers_for_resolved_agent, spawn_run_cancellation_guard,
};
pub use send_message_tool::{
    DurableMessageRequest, DurableMessageSink, FailedDurableMessage, FailedDurableMessageKey,
    FailedDurableMessageState, MessageDispatchHook, MessageOutbox, MessageOutboxKey, OutboxEntry,
    OutboxRoute, SEND_MESSAGE_TOOL_ID, SendMessageReceipt, SendMessageTool,
};
pub use state::{
    BackgroundTaskStateKey, BackgroundTaskStateSnapshot, BackgroundTaskViewKey, PersistedTaskMeta,
};
pub use types::{
    AgentTaskContext, BACKGROUND_TASKS_PLUGIN_ID, TaskContext, TaskEvent, TaskId,
    TaskParentContext, TaskResult, TaskStatus, TaskSummary,
};

#[cfg(test)]
mod tests;
