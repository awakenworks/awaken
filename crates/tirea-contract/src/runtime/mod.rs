//! Runtime contracts grouped by run/tool_call/llm lifecycle boundaries.

pub mod activity;
pub mod llm;
pub mod plugin;
pub mod run;
pub mod state_paths;
pub mod tool_call;

pub use activity::{ActivityManager, NoOpActivityManager};
pub use llm::{StreamResult, TokenUsage};
pub use plugin::{
    reduce_state_actions, validate_effect, AfterInferenceContext, AfterToolExecuteContext,
    AgentBehavior, AnyStateAction, BeforeInferenceContext, BeforeToolExecuteContext, NoOpBehavior,
    Phase, PhaseContext, PhaseEffect, PhaseOutput, PhasePolicy, ReadOnlyContext, RunAction,
    RunEndContext, RunStartContext, StateScope, StateSpec, StepContext, StepEndContext,
    StepOutcome, StepStartContext, SuspendTicket, ToolCallAction, ToolContext,
};
pub use run::{
    run_lifecycle_from_state, InferenceError, InferenceErrorState, RunContext, RunDelta, RunState,
    RunStatus, StoppedReason, TerminationReason,
};
pub use tool_call::{
    suspended_calls_from_state, tool_call_states_from_state, ActivityContext, DecisionReplayPolicy,
    PendingToolCall, SuspendedCall, SuspendedToolCallsAction, SuspendedToolCallsState, Suspension,
    SuspensionResponse, ToolCallContext, ToolCallOutcome, ToolCallResume, ToolCallResumeMode,
    ToolCallState, ToolCallStatesAction, ToolCallStatesMap, ToolCallStatus, ToolExecution,
    ToolExecutionEffect, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError, ToolProgressState, TOOL_PROGRESS_ACTIVITY_TYPE,
};
