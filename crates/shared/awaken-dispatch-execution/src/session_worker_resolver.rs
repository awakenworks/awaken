// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! Routes claimed work through the canonical Session preparation and Run factory.
//! Weak execution wiring keeps pool tasks from retaining their composing process.
use crate::session_execution::*;
use crate::{DispatchWorker, Error, WorkerResolver};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_session_realization::worker_projection::SessionProjectionExecution;
use std::sync::Arc;
pub mod claim_failure;
pub mod claimed_dispatch;
pub mod claimed_session;
mod resolver;

pub use claim_failure::{claim_outcome, map_admission_error, map_session_realization_drive_error};
use claimed_session::install_claimed_session_projection;
pub use resolver::SessionWorkerResolver;
