#![allow(missing_docs)]

pub mod agent;
pub mod builder;
pub mod context;
pub mod engine;
mod error;
pub mod execution;
pub mod extensions;
pub mod plugins;
pub mod policies;
pub mod registry;
pub mod runtime;
pub mod state;

// ── error ──
pub use error::RuntimeError;

// ── agent internals re-exported for extensions and tests ──
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
pub use execution::AllowAllToolsPlugin;
pub use runtime::loop_runner::{
    AgentLoopError, AgentLoopParams, AgentRunResult, LoopStatePlugin, build_agent_env,
    prepare_resume, run_agent_loop,
};

// ── compaction ──
pub use context::{
    CompactionConfig, CompactionConfigKey, CompactionPlugin, CompactionState, CompactionStateKey,
    ContextSummarizer, ContextTransform, DefaultSummarizer, record_compaction_boundary,
};

// ── retry policy ──
pub use execution::{LlmRetryPolicy, RetryConfigKey, RetryingExecutor};

// ── background tasks ──
pub use extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, PersistedTaskMeta, TaskId, TaskResult, TaskStatus,
    TaskSummary,
};

// ── agent tools ──
pub use extensions::a2a::{
    A2aConfig, AgentBackend, AgentBackendError, AgentTool, DelegateRunResult, DelegateRunStatus,
    LocalBackend,
};

// ── parallel merge ──
pub use runtime::loop_runner::parallel_merge::{
    ParallelMergeError, ToolStateBatch, collect_all_touched_keys, validate_parallel_state_batches,
};

// ── composite registry ──
pub use registry::composite::{CompositeAgentSpecRegistry, DiscoveryError, RemoteAgentSource};

// ── builder ──
pub use builder::{AgentRuntimeBuilder, BuildError};

// ── plugins ──
pub use plugins::{Plugin, PluginDescriptor, PluginRegistrar};

// ── runtime ──
pub use runtime::{
    AgentResolver, AgentRuntime, AppRuntime, CancellationToken, DEFAULT_MAX_PHASE_ROUNDS,
    ExecutionEnv, PhaseContext, PhaseHook, PhaseRuntime, ResolvedAgent, RunRequest, ToolPermission,
    ToolPermissionChecker, ToolPermissionResult, TypedEffectHandler, TypedScheduledActionHandler,
    aggregate_tool_permissions,
};

// ── state ──
pub use state::{CommitEvent, CommitHook, MutationBatch, StateCommand, StateStore};

// ── extensions ──
pub use extensions::a2ui::{A2uiPlugin, A2uiRenderTool, validate_a2ui_messages};
pub use extensions::handoff::{
    ActiveAgentKey, AgentOverlay, HandoffAction, HandoffPlugin, HandoffState,
};
