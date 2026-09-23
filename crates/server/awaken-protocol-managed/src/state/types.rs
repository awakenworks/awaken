//! Private imports of neutral Session vocabulary consumed by adapter state.
//! Public consumers import these types from their authoritative Session contract;
//! the protocol crate publishes no compatibility path.

#[cfg(test)]
pub(crate) use awaken_session_application_contract::DelegatedRun;
#[cfg(test)]
pub(crate) use awaken_session_application_contract::StepOutcome;
pub(crate) use awaken_session_budget_contract::SessionUsage;
pub(crate) use awaken_session_configuration_contract::{AgentCapabilities, CustomTool};
pub(crate) use awaken_session_error_contract::{RunError, RunErrorKind};
#[cfg(test)]
pub(crate) use awaken_session_execution_contract::SessionRuntime;
pub(crate) use awaken_session_execution_contract::{CommittedOutcomeProjection, OutcomeIteration};
#[cfg(test)]
pub(crate) use awaken_session_execution_contract::{OutcomeDrive, OutcomeFailure, OutcomeReport};
