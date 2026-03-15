// ── Modules from the former tirea-agent-loop crate ──────────────────────
pub mod activity;
pub mod control;
pub mod loop_runner;
pub mod run_context;
pub mod streaming;

// ── AgentOS orchestration modules ───────────────────────────────────────
pub(crate) mod agent_tools;
pub(crate) mod background_tasks;
mod behavior;
mod bundle_merge;
pub(crate) mod context;
mod errors;
#[cfg(feature = "mode")]
pub(crate) mod mode_wiring;
#[cfg(feature = "plan")]
pub(crate) mod plan_wiring;
mod policy;
mod prepare;
pub(crate) mod resolve;
mod run;
#[cfg(feature = "skills")]
pub(crate) mod skills_wiring;
pub(crate) mod stop_policy;
pub(crate) mod thread_run;
mod types;

#[cfg(test)]
mod tests;

// ── Re-exports: loop runner (former agent-loop) ─────────────────────────
pub use loop_runner::ResolvedRun;

// ── Re-exports: AgentOS orchestration ───────────────────────────────────
pub use background_tasks::{
    BackgroundCapable, BackgroundExecutable, BackgroundTaskManager, BackgroundTaskView,
    BackgroundTaskViewAction, BackgroundTaskViewState, BackgroundTasksPlugin, NewTaskSpec,
    TaskAction, TaskCancelTool, TaskId, TaskOutputTool, TaskResult, TaskResultRef, TaskState,
    TaskStatus, TaskStatusTool, TaskStore, TaskStoreError, TaskSummary, BACKGROUND_TASKS_PLUGIN_ID,
    TASK_CANCEL_TOOL_ID, TASK_OUTPUT_TOOL_ID, TASK_STATUS_TOOL_ID,
};
pub use behavior::compose_behaviors;
pub use context::{ContextPlugin, CONTEXT_PLUGIN_ID};
pub use errors::{AgentOsResolveError, AgentOsRunError};
pub use stop_policy::{
    ConsecutiveErrors, ContentMatch, LoopDetection, MaxRounds, StopOnTool, StopPolicy,
    StopPolicyInput, StopPolicyPlugin, StopPolicyStats, Timeout, TokenBudget,
};
pub use thread_run::ForwardedDecision;
pub use types::{AgentOs, PreparedRun, RunStream};

pub(crate) use types::RuntimeServices;

#[cfg(test)]
pub(crate) use crate::composition::AgentDefinition;
