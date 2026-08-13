//! Private imports of neutral Session vocabulary consumed by adapter state.
//! Public consumers import these types from `awaken-session-contract`; the
//! protocol crate publishes no compatibility path.

#[cfg(any(test, feature = "test-support"))]
pub(crate) use awaken_session_contract::SessionRuntime;
pub(crate) use awaken_session_contract::{
    AgentCapabilities, CustomTool, DelegatedRun, OutcomeDrive, OutcomeIteration, OutcomeReport,
    RunError, RunErrorKind, SessionUsage, StepOutcome, ToolPermissionDecision,
};
