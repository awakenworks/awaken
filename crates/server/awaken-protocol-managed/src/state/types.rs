//! The neutral session vocabulary + the [`SessionRuntime`] port now live in
//! `awaken-session-contract` (a contract/ leaf); this module re-exports them so
//! existing `crate::state::…` / `awaken_protocol_managed::…` paths keep resolving
//! until consumers flip to the contract directly. The Managed wire DTOs + the
//! neutral→wire encoder stay in this adapter (`crate::types`, `crate::project`).

pub use awaken_session_contract::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, McpServerBinding, OutcomeIteration, OutcomeReport, Pending, RunError,
    RunErrorKind, SessionInit, SessionRuntime, SessionUsage, StepFailure, StepOutcome, Terminus,
};
