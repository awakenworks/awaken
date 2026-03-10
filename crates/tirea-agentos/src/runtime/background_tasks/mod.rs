//! Background task management for agent tools.
//!
//! Provides a unified system for spawning, tracking, cancelling, and querying
//! background tasks. Tools that implement [`BackgroundExecutable`] can be
//! wrapped with [`BackgroundCapable`] for automatic `run_in_background` support.
//!
//! # Architecture
//!
//! - [`BackgroundTaskManager`] — thread-scoped handle table (spawn, cancel, query)
//! - [`BackgroundExecutable`] — trait for tools that support context-free background execution
//! - [`BackgroundCapable<T>`] — decorator that adds `run_in_background` parameter to a tool
//! - [`TaskStatusTool`] / [`TaskCancelTool`] / [`TaskOutputTool`] — built-in tools for LLM interaction
//! - [`TaskCompletionNotifier`] — pluggable callback for completion delivery (e.g. mailbox)
//!
//! Tasks are thread-scoped and outlive individual runs.

mod manager;
mod plugin;
mod store;
mod tools;
mod types;
mod wrapper;

pub use manager::{BackgroundTaskManager, TaskCompletionNotifier};
pub use plugin::{BackgroundTasksPlugin, BACKGROUND_TASKS_PLUGIN_ID};
pub use store::{NewTaskSpec, TaskPersistenceNotifier, TaskStore, TaskStoreError};
pub use tools::{
    TaskCancelTool, TaskOutputTool, TaskStatusTool, TASK_CANCEL_TOOL_ID, TASK_OUTPUT_TOOL_ID,
    TASK_STATUS_TOOL_ID,
};
pub(crate) use types::{derived_task_view_from_doc, legacy_tasks_from_doc};
#[allow(unused_imports)]
pub use types::{
    new_task_id, task_thread_id, BackgroundTask, BackgroundTaskAction, BackgroundTaskState,
    BackgroundTaskView, BackgroundTaskViewAction, BackgroundTaskViewState, TaskAction, TaskId,
    TaskResult, TaskResultRef, TaskState, TaskStatus, TaskSummary, TASK_THREAD_KIND_METADATA_KEY,
    TASK_THREAD_KIND_METADATA_VALUE, TASK_THREAD_PREFIX,
};
pub use wrapper::{BackgroundCapable, BackgroundExecutable};
