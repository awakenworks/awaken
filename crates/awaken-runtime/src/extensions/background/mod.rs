//! Background task management for agent tools.
//!
//! Provides a system for spawning, tracking, cancelling, and querying
//! background tasks. Tasks are tracked in-memory and outlive individual runs.

mod cancel_task_tool;
mod execution_context;
mod hook;
mod manager;
mod plugin;
mod send_message_tool;
pub(crate) mod state;
mod types;

pub(crate) use cancel_task_tool::CANCEL_TASK_TOOL_ID;
pub use cancel_task_tool::CancelTaskTool;
pub(crate) use execution_context::{
    BackgroundTaskExecutionContext, ToolLineageContext, current_background_task_context,
    current_tool_lineage_context, scope_background_task_context, scope_tool_lineage_context,
};
pub use manager::{BackgroundTaskManager, SendError, SpawnError};
pub use plugin::BackgroundTaskPlugin;
pub use send_message_tool::SendMessageTool;
pub use state::{BackgroundTaskViewKey, PersistedTaskMeta};
pub use types::{
    AgentTaskContext, TaskContext, TaskEvent, TaskId, TaskParentContext, TaskResult, TaskStatus,
    TaskSummary,
};

#[cfg(test)]
mod tests;
