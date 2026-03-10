pub(crate) mod agent_tools;
pub(crate) mod background_tasks;
mod behavior;
mod bundle_merge;
mod errors;
pub(crate) mod plugin;
mod policy;
mod prepare;
pub(crate) mod resolve;
mod run;
pub(crate) mod thread_run;
mod types;

#[cfg(test)]
mod tests;

pub use background_tasks::{
    BackgroundCapable, BackgroundExecutable, BackgroundTask, BackgroundTaskAction,
    BackgroundTaskManager, BackgroundTaskState, BackgroundTaskView, BackgroundTaskViewAction,
    BackgroundTaskViewState, BackgroundTasksPlugin, NewTaskSpec, TaskAction, TaskCancelTool,
    TaskCompletionNotifier, TaskId, TaskOutputTool, TaskPersistenceNotifier, TaskResult,
    TaskResultRef, TaskState, TaskStatus, TaskStatusTool, TaskStore, TaskStoreError, TaskSummary,
    BACKGROUND_TASKS_PLUGIN_ID, TASK_CANCEL_TOOL_ID, TASK_OUTPUT_TOOL_ID, TASK_STATUS_TOOL_ID,
};
pub use behavior::compose_behaviors;
pub use errors::{AgentOsResolveError, AgentOsRunError};
pub use plugin::context_window::{ContextWindowPlugin, CONTEXT_WINDOW_PLUGIN_ID};
pub use plugin::stop_policy::{
    ConsecutiveErrors, ContentMatch, LoopDetection, MaxRounds, StopOnTool, StopPolicy,
    StopPolicyInput, StopPolicyPlugin, StopPolicyStats, Timeout, TokenBudget,
};
pub use thread_run::ForwardedDecision;
pub use types::{AgentOs, PreparedRun, RunStream};

pub(crate) use types::RuntimeServices;

pub use crate::loop_runtime::loop_runner::ResolvedRun;

#[cfg(test)]
pub(crate) use crate::composition::AgentDefinition;
