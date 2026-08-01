//! Private imports of neutral Session vocabulary consumed by adapter state.
//! Public consumers import these types from `awaken-session-contract`; the
//! protocol crate publishes no compatibility path.

pub(crate) use awaken_session_contract::{
    AgentCapabilities, CustomTool, DelegatedRun, LiveInboxEntry, LiveInboxError, LiveInboxSnapshot,
    OutcomeIteration, OutcomeReport, RunError, RunErrorKind, SessionInit, SessionRuntime,
    SessionUsage, StepOutcome, ToolPermissionDecision,
};
