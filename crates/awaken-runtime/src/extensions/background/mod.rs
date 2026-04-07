//! Background task management for agent tools.
//!
//! Provides a system for spawning, tracking, cancelling, and querying
//! background tasks. Tasks are tracked in-memory and outlive individual runs.

mod hook;
mod manager;
mod plugin;
mod send_message_tool;
pub(crate) mod state;
mod types;

pub use manager::{BackgroundTaskManager, SendError, SpawnError};
pub use plugin::BackgroundTaskPlugin;
pub use send_message_tool::SendMessageTool;
pub use state::{BackgroundTaskViewKey, PersistedTaskMeta};
pub use types::{
    TaskContext, TaskEvent, TaskId, TaskParentContext, TaskResult, TaskStatus, TaskSummary,
};

#[cfg(test)]
mod tests;
