//! Private imports of neutral Session vocabulary consumed by adapter state.
//! Public consumers import these types from `awaken-session-contract`; the
//! protocol crate publishes no compatibility path.

#[cfg(test)]
pub(crate) use awaken_session_contract::DelegatedRun;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use awaken_session_contract::SessionRuntime;
pub(crate) use awaken_session_contract::{
    AgentCapabilities, CommittedOutcomeProjection, CustomTool, OutcomeIteration, RunError,
    RunErrorKind, SessionUsage,
};
#[cfg(test)]
pub(crate) use awaken_session_contract::{
    OutcomeDrive, OutcomeFailure, OutcomeReport, StepOutcome,
};
