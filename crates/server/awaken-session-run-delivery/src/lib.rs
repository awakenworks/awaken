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
//! The same process-local [`BackgroundRuns`] registry tracks auxiliary work
//! through drain and exact Environment-generation quiescence. Durable intent and
//! claim state remain with their original controllers and repositories.
//!
//! These components are not durable truth: committed Run truth decides execution
//! outcomes; process-local state tracks lifetimes, notifications and previews.
#![warn(unreachable_pub)]

mod authority_commits;
mod auxiliary_snapshot;
mod background;
mod child_dispatch;
mod commit_boundary;
mod committed_reads;
mod committed_step;
mod completion;
mod delivery;
mod hub;
mod lock;
mod memory_binding;
mod memory_binding_compiler;
mod memory_content;
mod memory_extractions;
mod memory_proposals;
mod outcome_projection;
mod run_publications;
mod session_activation;
mod session_budgets;
mod session_capabilities;
mod session_inputs;
mod session_projections;
mod session_run_activation;
mod session_run_reservation;
mod session_skills;
mod settled_step;
#[cfg(test)]
mod test_support;
mod thread_coordination;
mod thread_resume;

pub use authority_commits::{AuthorityCommits, runtime_authority_error};
pub use auxiliary_snapshot::resolve_auxiliary_snapshot;
pub use background::{BackgroundRuns, BackgroundWorkClass};
pub use child_dispatch::{ChildDispatchRefusal, child_dispatch_request};
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
pub use delivery::{ResourceBindingVerifiers, WorkerPlacedSessions, WorkerPlacement};
pub use hub::{ThreadEvent, ThreadEventHub};
pub use lock::{DeliveryLock, LOCK_POISONED_CODE};
pub use memory_binding::MemoryBinding;
pub use memory_binding_compiler::MemoryBindingCompiler;
pub use memory_content::PlatformMemoryHandle;
pub use memory_extractions::MemoryExtractions;
pub use memory_proposals::{
    MemoryProposalError, MemoryWriteProposals, committed_memory_writes, memory_extraction_input,
};
pub use outcome_projection::{
    committed_outcome_projection, outcome_controller_error, outcome_report,
};
pub use run_publications::RUN_PUBLICATIONS;
pub use session_activation::{
    ManagedCoordinationRole, SessionToolsetProjection, project_managed_coordination_surface,
    project_session_tool_override, project_session_tools, session_activation_snapshot,
    session_client_tool_descriptor,
};
pub use session_budgets::SessionBudgets;
pub use session_capabilities::{
    builtin_hand_tools, hand_tool_descriptors, supports_mid_conversation_system,
};
pub use session_inputs::{
    client_result_for_ticket, resume_activation, session_system_message,
    session_thread_reply_result,
};
pub use session_projections::{DispatchProjection, SessionProjections};
pub use session_run_activation::{activate_session_run_reservation, read_settled_phase};
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
pub use thread_coordination::{
    ChildRunInputs, CoordinatedThreads, CoordinationAuthorities, ResidentAttempt, SessionLowering,
    check_pending, dispatch_admission_error, validate_awaiting_tool_authority,
};
pub use thread_resume::{
    decision_input, durable_resume_input, manual_resume_input, resume_result, resume_wants_client,
};

/// Poison `lock` the way production can: a holder panics while holding it.
#[cfg(test)]
pub(crate) fn poison_for_test<T>(lock: &std::sync::Mutex<T>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = lock.lock();
        panic!("poison the lock under test");
    }));
    assert!(lock.is_poisoned());
}
