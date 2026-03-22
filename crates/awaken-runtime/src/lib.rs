#![allow(missing_docs)]

pub mod agent;
pub mod builder;
pub mod engine;
mod error;
pub mod extensions;
pub mod plugins;
pub mod registry;
pub mod runtime;
pub mod state;

// ── error ──
pub use error::RuntimeError;

// ── agent internals re-exported for extensions and tests ──
pub use agent::loop_runner::{
    AgentLoopError, AgentRunResult, LoopStatePlugin, build_agent_env, prepare_resume,
    run_agent_loop, run_agent_loop_controlled,
};
pub use agent::state::AddContextMessage;
pub use agent::state::{
    AccumulatedContextMessages, AccumulatedContextMessagesUpdate, AccumulatedOverrides,
    AccumulatedOverridesUpdate, AccumulatedToolExclusions, AccumulatedToolExclusionsUpdate,
    AccumulatedToolInclusions, AccumulatedToolInclusionsUpdate, ContextMessageAction,
    ContextMessageStore, ContextMessageStoreValue, ContextThrottleMap, ContextThrottleState,
    ContextThrottleUpdate, ExcludeTool, IncludeOnlyTools, RunLifecycle, RunLifecycleState,
    RunLifecycleUpdate, SetInferenceOverride, ToolCallState, ToolCallStateMap, ToolCallStates,
    ToolCallStatesUpdate, ToolInclusionSet,
};
pub use agent::tool_permission::AllowAllToolsPlugin;

// ── compaction ──
pub use agent::compaction::{
    CompactionConfig, CompactionConfigKey, CompactionPlugin, CompactionState, CompactionStateKey,
    ContextSummarizer, ContextTransform, DefaultSummarizer, record_compaction_boundary,
};

// ── retry policy ──
pub use agent::retry_policy::{LlmRetryPolicy, RetryConfigKey, RetryingExecutor};

// ── permission rules ──
pub use agent::permission_rules::{
    PermissionAction, PermissionRule, RulesPermissionChecker, RulesPermissionPlugin,
};

// ── background tasks ──
pub use agent::background_tasks::{
    BackgroundTaskManager, BackgroundTaskPlugin, PersistedTaskMeta, TaskId, TaskResult, TaskStatus,
    TaskSummary,
};

// ── agent tools ──
pub use agent::agent_tools::{
    A2aConfig, AgentBackend, AgentBackendError, AgentTool, DelegateRunResult, DelegateRunStatus,
    LocalBackend,
};

// ── parallel merge ──
pub use agent::loop_runner::parallel_merge::{
    ParallelMergeError, ToolStateBatch, collect_all_touched_keys, validate_parallel_state_batches,
};

// ── builder ──
pub use builder::AgentRuntimeBuilder;

// ── plugins ──
pub use plugins::{Plugin, PluginDescriptor, PluginRegistrar};

// ── runtime ──
pub use runtime::{
    AgentResolver, AgentRuntime, AppRuntime, CancellationToken, DEFAULT_MAX_PHASE_ROUNDS,
    ExecutionEnv, PhaseContext, PhaseHook, PhaseRuntime, ResolvedAgent, RunHandle, RunRequest,
    ToolPermission, ToolPermissionChecker, ToolPermissionResult, TypedEffectHandler,
    TypedScheduledActionHandler, aggregate_tool_permissions,
};

// ── state ──
pub use state::{CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore};

// ── extensions ──
pub use extensions::a2ui::{A2uiPlugin, A2uiRenderTool, validate_a2ui_messages};
pub use extensions::handoff::{
    ActiveAgentKey, AgentOverlay, HandoffAction, HandoffPlugin, HandoffState,
};
