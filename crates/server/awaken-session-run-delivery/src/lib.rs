//! `awaken-session-run-delivery` — how a Coordinator delivers a Session's Runs
//! to the Workers that execute them and observes them settle.
//!
//! A Coordinator admits a Run into dispatch, a registered Worker claims and
//! executes it, and the Worker's settlement and live progress come back over
//! the Worker transport. This crate owns the Coordinator half of that loop
//! that needs no execution machinery: the per-Thread live observation point
//! ([`ThreadEventHub`]) every protocol adapter subscribes to, and the
//! foreground completion relay ([`CompletionRegistry`]) a caller registers
//! with before its Run is activated. It links no Session realization, no
//! Sandbox and no credential realization, so the Coordinator process composes
//! it without the Worker-side executor; the executor builds on it for the
//! Runs a co-located pool executes.
//!
//! Neither component is durable truth: committed Run truth decides every
//! outcome, these only notify and preview.
#![warn(unreachable_pub)]

mod commit_boundary;
mod committed_reads;
mod committed_step;
mod completion;
mod delivery;
mod hub;
mod lock;
mod outcome_projection;
mod run_publications;
mod session_activation;
mod session_projections;
mod session_run_reservation;
mod session_skills;
mod settled_step;
#[cfg(test)]
mod test_support;

pub use commit_boundary::{
    CommittedAuthority, SessionCommit, SessionCommitBoundary, WorkerRecoveryCommit,
};
pub use committed_reads::{
    AdvisorModelSource, CommittedCoordinationProjection, CommittedReads, CoordinationReadPurpose,
    SessionCommitSource, application_error, authoritative_child_ticket,
    delegation_registry_from_snapshot, project_committed_coordination, project_delegated_runs,
    recovery_ticket, visible_pending_tool,
};
pub use committed_step::{
    CommittedStepReceipt, StepCommitExpectation, VerifiedStepProjection,
    authoritative_step_snapshot, message_prefix_before_exact_inputs, project_committed_step,
    verify_committed_step,
};
pub use completion::{
    CompletionRegistry, CompletionSignalClosed, WaiterGuard, activate_and_await_session_run,
    await_completion_state,
};
pub use delivery::{ResourceBindingVerifiers, SessionRunDelivery};
pub use hub::{ThreadEvent, ThreadEventHub};
pub use lock::{DeliveryLock, LOCK_POISONED_CODE};
pub use outcome_projection::{
    committed_outcome_projection, outcome_controller_error, outcome_report,
};
pub use run_publications::RUN_PUBLICATIONS;
pub use session_activation::{
    ManagedCoordinationRole, SessionToolsetProjection, project_managed_coordination_surface,
    project_session_tool_override, project_session_tools, session_activation_snapshot,
    session_client_tool_descriptor,
};
pub use session_projections::{DispatchProjection, SessionProjections};
pub use session_run_reservation::{
    SESSION_RUN_RESERVATION_TTL_MS, SessionRunInputs, reserve_session_run,
    validate_session_run_command,
};
pub use session_skills::{
    SKILLS_REQUIRE_FILESYSTEM, SkillSnapshot, delivered_skill_file, managed_skill_registry,
    published_skill_ids, requires_filesystem, session_allows_agent_tool,
    session_allows_filesystem_tools,
};
pub use settled_step::settled_step;

/// Poison `lock` the way production can: a holder panics while holding it.
#[cfg(test)]
pub(crate) fn poison_for_test<T>(lock: &std::sync::Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = lock.lock();
        panic!("poison the lock under test");
    }));
    assert!(lock.is_poisoned());
}
