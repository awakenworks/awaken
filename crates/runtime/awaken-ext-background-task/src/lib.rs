//! Detached ordinary-tool execution as a state-only Runtime plugin.
//!
//! The extension owns typed aggregates and pure transitions. It has no SQL,
//! repository, migration, or application-service dependency. Tools read an
//! immutable State snapshot and return commands for Runtime to commit.

mod model;
mod plugin;
mod supervisor;
mod tools;

pub use model::{
    BackgroundInvocation, BackgroundTask, BackgroundTaskEnd, BackgroundTaskError, BackgroundTaskId,
    BackgroundTaskLifecycle, BackgroundTaskOrigin, BackgroundWait, TaskAttempt, TaskClaim,
    TaskExecutionPolicy, TaskFence,
};
pub use plugin::{
    BACKGROUND_TASK_PLUGIN_ID, BackgroundTaskConfig, BackgroundTaskPlugin, config_schema,
};
pub use supervisor::{
    BackgroundTaskCompletion, BackgroundTaskSupervisor, BackgroundTaskWaitCandidate,
    process_supervisor,
};

pub const BACKGROUND_TASK_STATE_PREFIX: &str = "background_task/";

#[must_use]
pub fn task_state_cell(
    id: &BackgroundTaskId,
) -> awaken_agent_contract::agent::state::StateCell<BackgroundTask> {
    awaken_agent_contract::agent::state::StateCell::new(
        awaken_agent_contract::agent::state::Scope::Thread,
        awaken_agent_contract::agent::state::MergePolicy::Exclusive,
        format!("{BACKGROUND_TASK_STATE_PREFIX}{}", id.as_str()),
    )
}

/// Decode every task in the extension-owned Thread namespace. A malformed
/// committed value fails closed instead of disappearing from reconciliation.
pub fn tasks_from_state(
    state: &awaken_agent_contract::agent::state::Store,
) -> Result<Vec<BackgroundTask>, awaken_runtime_contract::tool::ToolError> {
    tools::load_tasks(state)
}
