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
mod activation;
mod aggregate;
mod coordination;
mod error;
mod evidence;
mod fact;
mod fault;
mod historical;
mod index;
mod interval;
mod key;
mod lifecycle;
mod observation;
mod order;
mod outcome;
mod prefix;
mod role;
mod stop;
mod transcript;
mod transcript_plan;
mod usage;

pub use acceptance::{AcceptanceFence, LifecycleAcceptance, accept_lifecycle};
pub use activation::activation_failure_fact;
pub use aggregate::{IntervalAggregatePlan, IntervalStep, IntervalUsage, plan_interval_aggregate};
pub use coordination::{
    CoordinationReceipts, IssuedToolEvent, SEND_MESSAGE_TOOL_ID, coordination_orders, cross_posts,
    is_coordination_send, link_facts,
};
pub use error::ProjectionError;
pub use evidence::{
    ProjectionThreadSnapshots, SessionProjectionEvidence, SessionProjectionReadTimings,
    committed_lifecycle_prefix,
};
pub use fact::{FactKind, TimelineFact};
pub use fault::{RunFault, run_fault, run_state_fault};
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
pub use lifecycle::{
    ChildLifecycle, CoordinatedThread, ThreadStatus, archive_children, project_child_lifecycle,
    terminate_session,
};
pub use observation::run_observation_facts;
pub use order::{CanonicalOrder, historical_pending_projection_order};
pub use outcome::{
    AnchoredOutcome, OutcomeAnchor, anchor_outcome, outcome_owns_failure_run, validate_outcome,
};
pub use prefix::same_thread_projection_prefix;
pub use role::{FactRole, TranscriptRole, phase};
pub use stop::{StopFact, terminal_stop};
pub use transcript::{
    AdvisorConsultationBlocks, CommittedMessageProjection, CommittedTranscriptEvidence,
    RuntimeToolFollowupAdmission, RuntimeToolProjectionPhase, advisor_consultation_blocks,
    assistant_response_coordinates, committed_transcript_evidence, fold_transcript,
};
pub use transcript_plan::{
    ChildTranscript, ChildTranscriptPlan, ChildTranscriptStep, MessageFold, plan_child_transcript,
    plan_folds,
};
pub use usage::committed_thread_usage;
