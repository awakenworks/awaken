//! Runtime contracts grouped by domain boundaries.

pub mod action;
pub mod activity;
pub mod behavior;
pub mod extensions;
pub mod inference;
pub mod phase;
pub mod run;
pub mod state;
pub mod tool_call;

pub use action::Action;
pub use activity::{ActivityManager, NoOpActivityManager};
pub use behavior::{
    build_read_only_context_from_step, AgentBehavior, NoOpBehavior, ReadOnlyContext,
};
pub use extensions::Extensions;
pub use inference::{StreamResult, TokenUsage};
pub use phase::{
    AfterInferenceContext, AfterToolExecuteContext, BeforeInferenceContext,
    BeforeToolExecuteContext, Phase, PhaseContext, PhasePolicy, RunAction, RunEndContext,
    RunStartContext, StepContext, StepEndContext, StepOutcome, StepStartContext, SuspendTicket,
    ToolCallAction,
};
pub use run::{
    run_lifecycle_from_state, FlowControl, InferenceError, InferenceErrorState, RunContext,
    RunDelta, RunLifecycleAction, RunLifecycleState, RunStatus, StoppedReason, TerminationReason,
};
pub use state::{reduce_state_actions, AnyStateAction, CommutativeAction, StateScope, StateSpec};
pub use tool_call::{
    suspended_calls_from_state, tool_call_states_from_state, ActivityContext, DecisionReplayPolicy,
    PendingToolCall, SuspendedCall, SuspendedToolCallsAction, SuspendedToolCallsState, Suspension,
    SuspensionResponse, ToolCallContext, ToolCallOutcome, ToolCallResume, ToolCallResumeMode,
    ToolCallState, ToolCallStatesAction, ToolCallStatesMap, ToolCallStatus, ToolExecution,
    ToolExecutionEffect, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError, ToolGate, ToolProgressState, TOOL_PROGRESS_ACTIVITY_TYPE,
};
