//! Managed protocol Session event semantics: what committed Session, Thread,
//! Run and Outcome facts publish, and in what order.
//!
//! Pure and synchronous over borrowed evidence: no I/O, clock or cache.
//! The Managed protocol adapter reads the evidence through the Session
//! application, asks this crate which facts exist and where each one sits,
//! mints its own public ids from [`EventIdentity`]s and encodes each fact on its
//! wire. The evidence itself is the Session's (`CommittedSessionFacts` in
//! `awaken-session-execution-contract`). Consumer: the Managed protocol adapter
//! (identity, ordering and projection).

mod updates;
pub use updates::derive_session_update_events;
mod acceptance;
mod accepted_prefix;
mod activation;
mod aggregate;
mod budget;
mod coordination;
mod coordination_events;
mod error;
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
mod permission;
mod position;
mod prefix;
mod role;
mod stop;
mod transcript;
mod transcript_fold;
mod transcript_plan;
mod usage;

pub use acceptance::{AcceptanceFence, LifecycleAcceptance, accept_lifecycle};
pub use accepted_prefix::{
    AcceptedLifecycle, ChildThreadStates, PendingTicketAdmission, PendingTicketObservation,
    accept_committed_lifecycle, advance_answered_pending_commit, answered_pending_commit_cursor,
    child_thread_states, committed_pause_health, current_recovery_run, pending_ticket_admission,
};
pub use activation::{ActivationFailureProjection, activation_failure_projection};
pub use aggregate::{
    AggregateFallbackClose, AggregateIntervalClose, AggregatePlan, AggregateStop, IntervalHold,
    IntervalUsage, interval_hold, plan_aggregate,
};
pub use budget::BudgetReach;
pub use coordination::{
    CoordinationReceipts, IssuedToolEvent, SEND_MESSAGE_TOOL_ID, coordination_orders, cross_posts,
    is_coordination_send, link_facts, project_child_activity,
};
pub use coordination_events::{
    CoordinationEvents, CoordinationEvidence, derive_coordination_events,
};
pub use error::ProjectionError;
pub use fact::{DerivedSessionEvent, OutcomeVerdict, SessionEventKind};
pub use fault::{RunFault, run_fault, run_state_fault};
pub use historical::{
    SyntheticAwaitingTool, audited_await_targets, historical_message_await_name,
    historical_message_boundaries, historical_pending_by_lifecycle, synthetic_awaiting_tools,
};
pub use index::{
    BudgetReachAnchor, OrderEvidence, OrderIndex, ParentTerminalEvidence,
    message_projection_ordinal_bound, report_commit_cursor,
};
pub use interval::{
    budget_reach_close_cursor, budget_reach_projection_close_cursor, interval_close_cursor,
    interval_lifecycle_open_event, is_opening, period_opening,
    running_interval_lifecycle_open_event,
};
pub use key::{EventIdentity, FactProvenance};
pub use lifecycle::{
    ChildLifecycle, CoordinatedThread, RootTerminalFacts, ThreadStatus, archive_children,
    initial_coordinated_threads, project_child_lifecycle, root_lifecycle_opening_facts,
    root_run_boundaries, root_terminal_facts, root_terminal_idle_key, terminate_session,
};
pub use observation::run_observation_facts;
pub use order::{EventPosition, Ordinal, historical_pending_projection_order};
pub use outcome::{AnchoredOutcome, CommittedOutcome, OutcomeAnchor};
pub use position::{AnchoredEvent, EventAnchor, position_events};
pub use prefix::same_thread_projection_prefix;
pub use role::{ManagedEventType, TranscriptRole, phase};
pub use stop::{StopFact, terminal_stop};
pub use transcript::{
    AdvisorConsultationBlocks, CommittedMessageProjection, CommittedTranscriptEvidence,
    RuntimeToolFollowupAdmission, RuntimeToolProjectionPhase, ToolResultSource,
    advisor_consultation_blocks, assistant_response_coordinates, committed_transcript_evidence,
    fold_transcript, validate_committed_message_identities,
};
pub use transcript_fold::{
    ToolFamilies, ToolUseFamily, TranscriptFold, is_public_user_input, transcript_fact_identity,
};
pub use transcript_plan::{
    ChildTranscript, ChildTranscriptPlan, ChildTranscriptStep, MessageFold, plan_child_transcript,
    plan_folds,
};
pub use usage::{committed_thread_usage, session_usage_as_of};

pub use permission::{
    RecordedToolPermission, RecordedToolPermissions, ScopedToolPermissionKey,
    recorded_tool_permissions,
};
