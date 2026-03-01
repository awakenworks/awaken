pub use crate::runtime::run::flow::RunAction;
pub use crate::runtime::tool_call::gate::{SuspendTicket, ToolCallAction};

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
