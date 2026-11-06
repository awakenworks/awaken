// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: This module publishes the one neutral Runs vocabulary and its
// @formal reason: projections; all lifecycle and authorization remain with implementers.
//! Conversation Runs, their committed projections and request-local identities.
//!
//! The SDK and hosted compositions implement the same Run vocabulary. This
//! contract owns no Session, Sandbox, scheduling or authorization decisions.

pub mod error;
mod execution;
mod pending;
pub mod request;
pub mod scope;
mod step_outcome;

pub use error::{RunError, RunErrorClassification, RunErrorKind};
pub use execution::{
    EventForwardingSink, OwnedRuns, RunMode, RunResume, Runs, StopReceipt, blocks_text,
    paginate_history,
};
pub use pending::Pending;
pub use step_outcome::{DelegatedRun, StepOutcome};
