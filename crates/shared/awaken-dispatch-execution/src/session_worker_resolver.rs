// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Routes claimed work through the canonical Session preparation and Run factory.
//! The resolver holds the roles its construction was given and nothing else.
use crate::session_execution::*;
use crate::{DispatchWorker, Error, WorkerResolver};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_dispatch_erasure::AnyDispatchStore;
use std::sync::Arc;
pub mod claim_failure;
pub mod claimed_dispatch;
pub mod claimed_session;
mod resolver;

pub(crate) use claim_failure::deferral_retry_after;
pub use claim_failure::{claim_outcome, map_admission_error, map_session_realization_drive_error};
pub use resolver::{
    ClaimedExecution, CommittedTerminalScan, ProjectionAuthority, SessionWorkerResolver,
};
