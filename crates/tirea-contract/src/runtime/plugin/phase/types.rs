use crate::runtime::run::TerminationReason;
use crate::runtime::tool_call::Suspension;
use crate::runtime::{PendingToolCall, ToolCallResumeMode};
use serde::{Deserialize, Serialize};

/// Execution phase in the agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Thread started (called once).
    RunStart,
    /// Step started - prepare context.
    StepStart,
    /// Before LLM inference - build messages, filter tools.
    BeforeInference,
    /// After LLM inference - process response.
    AfterInference,
    /// Before tool execution.
    BeforeToolExecute,
    /// After tool execution.
    AfterToolExecute,
    /// Step ended.
    StepEnd,
    /// Thread ended (called once).
    RunEnd,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RunStart => write!(f, "RunStart"),
            Self::StepStart => write!(f, "StepStart"),
            Self::BeforeInference => write!(f, "BeforeInference"),
            Self::AfterInference => write!(f, "AfterInference"),
            Self::BeforeToolExecute => write!(f, "BeforeToolExecute"),
            Self::AfterToolExecute => write!(f, "AfterToolExecute"),
            Self::StepEnd => write!(f, "StepEnd"),
            Self::RunEnd => write!(f, "RunEnd"),
        }
    }
}

/// Mutation policy enforced for each phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhasePolicy {
    /// Whether tool filtering (`StepContext::tools`) can be mutated.
    pub allow_tool_filter_mutation: bool,
    /// Whether `StepContext::run_action` can be mutated.
    pub allow_run_action_mutation: bool,
    /// Whether tool execution gate (`blocked/pending`) can be mutated.
    pub allow_tool_gate_mutation: bool,
}

impl PhasePolicy {
    pub const fn read_only() -> Self {
        Self {
            allow_tool_filter_mutation: false,
            allow_run_action_mutation: false,
            allow_tool_gate_mutation: false,
        }
    }
}

impl Phase {
    /// Return mutation policy for this phase.
    pub const fn policy(self) -> PhasePolicy {
        match self {
            Self::BeforeInference => PhasePolicy {
                allow_tool_filter_mutation: true,
                allow_run_action_mutation: true,
                allow_tool_gate_mutation: false,
            },
            Self::AfterInference => PhasePolicy {
                allow_tool_filter_mutation: false,
                allow_run_action_mutation: true,
                allow_tool_gate_mutation: false,
            },
            Self::BeforeToolExecute => PhasePolicy {
                allow_tool_filter_mutation: false,
                allow_run_action_mutation: false,
                allow_tool_gate_mutation: true,
            },
            Self::RunStart
            | Self::StepStart
            | Self::AfterToolExecute
            | Self::StepEnd
            | Self::RunEnd => PhasePolicy::read_only(),
        }
    }
}

/// Result of a step execution.
#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    /// Continue to next step.
    Continue,
    /// Thread complete.
    Complete,
    /// Pending external suspension.
    Pending(SuspendTicket),
}

/// Run-level control action emitted by plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAction {
    /// Continue normal execution.
    Continue,
    /// Terminate run with specific reason.
    Terminate(TerminationReason),
}

/// Suspension payload for `ToolCallAction::Suspend`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuspendTicket {
    /// External suspension payload.
    pub suspension: Suspension,
    /// Pending call projection emitted to event stream.
    pub pending: PendingToolCall,
    /// Resume mapping strategy.
    pub resume_mode: ToolCallResumeMode,
}

impl SuspendTicket {
    pub fn new(
        suspension: Suspension,
        pending: PendingToolCall,
        resume_mode: ToolCallResumeMode,
    ) -> Self {
        Self {
            suspension,
            pending,
            resume_mode,
        }
    }

    pub fn use_decision_as_tool_result(suspension: Suspension, pending: PendingToolCall) -> Self {
        Self::new(
            suspension,
            pending,
            ToolCallResumeMode::UseDecisionAsToolResult,
        )
    }

    pub fn with_resume_mode(mut self, resume_mode: ToolCallResumeMode) -> Self {
        self.resume_mode = resume_mode;
        self
    }

    pub fn with_pending(mut self, pending: PendingToolCall) -> Self {
        self.pending = pending;
        self
    }
}

/// Tool-call level control action emitted by plugins.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallAction {
    Proceed,
    Suspend(Box<SuspendTicket>),
    Block { reason: String },
}

impl ToolCallAction {
    pub fn suspend(ticket: SuspendTicket) -> Self {
        Self::Suspend(Box::new(ticket))
    }
}
