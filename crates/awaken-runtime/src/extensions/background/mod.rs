//! Background task management for agent tools.
//!
//! Provides a system for spawning, tracking, cancelling, and querying
//! background tasks. Tasks are tracked in-memory and outlive individual runs.

mod hook;
mod manager;
mod plugin;
mod state;
mod types;

pub use manager::BackgroundTaskManager;
pub use plugin::BackgroundTaskPlugin;
pub use state::PersistedTaskMeta;
pub use types::{TaskId, TaskResult, TaskStatus, TaskSummary};

#[cfg(test)]
mod tests;
