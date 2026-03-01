//! Runtime contracts grouped by run/tool_call/llm lifecycle boundaries.

pub mod action;
pub mod activity;
pub mod behavior;
pub mod extensions;
pub mod inference;
pub mod llm;
pub mod plugin;
pub mod run;
pub mod state;
pub mod state_paths;
pub mod tool_call;

pub use activity::{ActivityManager, NoOpActivityManager};
pub use llm::{StreamResult, TokenUsage};
pub use plugin::{
    build_read_only_context_from_step, reduce_state_actions, Action, AfterInferenceContext,
    AfterToolExecuteContext, AgentBehavior, AnyStateAction, BeforeInferenceContext,
    BeforeToolExecuteContext, CommutativeAction, Extensions, NoOpBehavior, Phase, PhaseContext,
    PhasePolicy, ReadOnlyContext, RunAction, RunEndContext, RunStartContext, StateScope, StateSpec,
    StepContext, StepEndContext, StepOutcome, StepStartContext, SuspendTicket, ToolCallAction,
    ToolGate,
};
pub use run::{
    run_lifecycle_from_state, InferenceError, InferenceErrorState, RunContext, RunDelta,
    RunLifecycleAction, RunLifecycleState, RunStatus, StoppedReason, TerminationReason,
};
pub use tool_call::{
    suspended_calls_from_state, tool_call_states_from_state, ActivityContext, DecisionReplayPolicy,
    PendingToolCall, SuspendedCall, SuspendedToolCallsAction, SuspendedToolCallsState, Suspension,
    SuspensionResponse, ToolCallContext, ToolCallOutcome, ToolCallResume, ToolCallResumeMode,
    ToolCallState, ToolCallStatesAction, ToolCallStatesMap, ToolCallStatus, ToolExecution,
    ToolExecutionEffect, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError, ToolProgressState, TOOL_PROGRESS_ACTIVITY_TYPE,
};
