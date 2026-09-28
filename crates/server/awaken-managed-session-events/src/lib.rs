//! The Session timeline domain: what a protocol projection of committed
//! Session, Thread, Run and Outcome truth publishes, and in what order.
//!
//! Pure and synchronous over borrowed evidence: no I/O, clock, cache or wire
//! vocabulary. A protocol adapter reads the evidence through the Session
//! application, asks this crate which facts exist and where each one sits,
//! mints its own public ids from [`FactKey`]s and encodes each fact on its
//! wire. Consumers: `awaken-session-application` (the evidence it reads) and
//! the Managed protocol adapter (identity, ordering and projection).

mod acceptance;
mod error;
mod evidence;
mod historical;
mod index;
mod interval;
mod key;
mod order;
mod outcome;
mod prefix;
mod role;
mod transcript;

pub use acceptance::{AcceptanceFence, LifecycleAcceptance, accept_lifecycle};
pub use error::ProjectionError;
pub use evidence::{
    ProjectionThreadSnapshots, SessionProjectionEvidence, SessionProjectionReadTimings,
    committed_lifecycle_prefix,
};
pub use historical::{historical_message_boundaries, historical_pending_by_lifecycle};
pub use index::{
    BudgetReachAnchor, OrderEvidence, OrderIndex, ParentTerminalEvidence,
    message_projection_ordinal_bound, report_commit_cursor,
};
pub use interval::{
    budget_reach_close_cursor, budget_reach_projection_close_cursor, interval_close_cursor,
    interval_lifecycle_open_event, is_opening, running_interval_lifecycle_open_event,
};
pub use key::{FactKey, FactProvenance};
pub use order::{CanonicalOrder, historical_pending_projection_order};
pub use outcome::{
    AnchoredOutcome, OutcomeAnchor, anchor_outcome, outcome_owns_failure_run, validate_outcome,
};
pub use prefix::same_thread_projection_prefix;
pub use role::{FactRole, TranscriptRole, phase};
pub use transcript::assistant_response_coordinates;
