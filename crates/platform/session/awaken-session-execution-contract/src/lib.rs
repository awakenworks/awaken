//! The Session ↔ execution boundary: the Session roles execution calls, coordination
//! and Run admission, the Session realization phase protocol and its Control ports,
//! and the one late binding from execution back to the Session application.
//!
//! Realization and execution cross the same Coordinator ↔ Worker boundary between
//! the same parties, so they are one contract. The retired realization package is
//! unavailable:
//!
//! ```compile_fail
//! use awaken_session_realization_contract::SessionRealizationControl;
//! ```
//!
//! Protocol adapters drive the live inbox through the application contract,
//! not through execution:
//!
//! ```compile_fail
//! use awaken_session_execution_contract::LiveInboxApplication;
//! ```
//!
//! The contract publishes only its own items. Session Core, agent and
//! configuration vocabulary is named from its owner, and `neutral_role!` names
//! those owners by absolute path, so there is no macro-support module:
//!
//! ```compile_fail
//! use awaken_session_execution_contract::__macro_support::RunError;
//! ```
//!
//! ```compile_fail
//! use awaken_session_execution_contract::ToolPermissionDecision;
//! ```
//!
//! ```compile_fail
//! use awaken_session_execution_contract::SessionThreadTarget;
//! ```
//!
//! # Boundary
//!
//! Between the Session and its Worker-side executors (execution, realization and coordination
//! roles).
//!
//! Implemented by: `awaken-coordinator-client`, `awaken-dispatch-execution`,
//! `awaken-protocol-managed`, `awaken-protocol-managed-testkit`, `awaken-scenario-host`,
//! `awaken-session-application`, `awaken-session-executor`, `awaken-session-mcp`,
//! `awaken-session-realization`, `awaken-session-realization-driver`,
//! `awaken-session-run-delivery`.
//!
//! Called by: `awaken-cli`, `awaken-coordinator`, `awaken-coordinator-client`, `awaken-dispatch`,
//! `awaken-dispatch-execution`, `awaken-dispatch-http`, `awaken-dispatch-testkit`,
//! `awaken-managed-session-events`, `awaken-process-startup`, `awaken-protocol-managed`,
//! `awaken-protocol-managed-testkit`, `awaken-resource-worker-http`, `awaken-scenario-host`,
//! `awaken-session-application`, `awaken-session-executor`, `awaken-session-mcp`,
//! `awaken-session-realization`, `awaken-session-realization-driver`,
//! `awaken-session-run-delivery`, `awaken-worker`.

mod application_binding;
mod bound_source_end;
mod claimed_control;
mod claimed_coordination;
mod coordination;
mod durable_run_operations;
#[cfg(feature = "test-support")]
pub mod realization_observation;
mod resource_operation_fence;
mod run_admission;
mod session;
mod session_realization;
mod source_retention;

#[cfg(test)]
pub(crate) use awaken_agent_contract::McpTarget;
pub(crate) use awaken_agent_contract::stable_fingerprint;
#[cfg(test)]
pub(crate) use awaken_environment_contract::EnvironmentRevision;
pub(crate) use awaken_runtime_contract::runs::RunError;
#[cfg(test)]
pub(crate) use awaken_runtime_contract::runs::RunErrorKind;
pub(crate) use awaken_runtime_contract::runs::{DelegatedRun, Pending, StepOutcome};
pub(crate) use awaken_session_application_contract::{
    DelegatedRunSnapshot, SessionBudgetResumeDelivery, SessionBudgetResumeDisposition,
    SessionBudgetResumeTicket,
};
#[cfg(feature = "test-support")]
pub(crate) use awaken_session_configuration_contract::mcp::McpGeneration;
pub(crate) use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, McpProjectionEffectKind, McpProjectionReceipt, McpRealizationReceipt,
    SessionMcpAttachment, StageMcpAttachment,
};
pub(crate) use awaken_session_configuration_contract::resource::{
    ResolvedInput, ResolvedSessionResources, ResolvedSkillBinding, SessionRealizationLease,
    SessionResourceManifest, SessionResourceTransition,
};
pub(crate) use awaken_session_configuration_contract::{
    AgentCapabilities, SessionBaseline, SessionInit, SessionModelOverride, SessionSandboxLayout,
    SessionSystemPromptSelection, SessionToolConfiguration,
};
#[cfg(test)]
pub(crate) use awaken_session_configuration_contract::{
    EnvironmentFingerprint, EnvironmentSnapshot, SessionBaselineInputs, SessionNetworkPolicy,
};
#[cfg(test)]
pub(crate) use awaken_session_contract::{SessionCleanupCommand, SessionUserRunSystemInput};
pub(crate) use awaken_session_contract::{
    SessionRepositoryPublicationReceipt, SessionRepositoryPublicationRejection,
    SessionTerminalCleanupAction, SessionTerminalMemoryIntent, SessionTerminalMemoryTarget,
};
pub(crate) use awaken_session_types::{SessionId, SessionRevision, SessionRuntimePlacement};
pub(crate) use {
    awaken_provisioning_contract::SandboxCheckpointRef,
    awaken_provisioning_contract::SandboxRestoreRequest,
    awaken_session_contract::CheckpointReceipt, awaken_session_contract::QuiescenceReceipt,
    awaken_session_contract::RestoreReceipt, awaken_session_contract::SandboxCheckpointRequest,
    awaken_session_contract::SandboxGeneration,
    awaken_session_contract::SessionCleanupDisposalReceipt,
    awaken_session_contract::SessionCleanupPreparation,
    awaken_session_contract::SessionEnvironmentEffectAuthorization,
    awaken_session_contract::SessionEnvironmentEffectIntent,
    awaken_session_contract::SessionEnvironmentOperation,
    awaken_session_contract::SessionEnvironmentReceipt,
    awaken_session_contract::SessionEnvironmentState,
    awaken_session_contract::SessionRepositoryPublicationCommand,
    awaken_session_contract::SessionRepositoryPublicationEffect,
    awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    awaken_session_contract::SessionTerminalCleanupEffect,
    awaken_session_contract::SourceDisposedReceipt, awaken_session_contract::SourceReleaseDisposal,
    awaken_session_contract::SourceReleasePreparationEffect,
    awaken_session_contract::SourceReleasePreparedReceipt,
};

pub use application_binding::{
    SessionApplicationBinding, SessionApplicationBindingError, SessionApplicationLifetime,
    SessionApplicationMode, SessionApplications,
};
pub use bound_source_end::{BoundSourceEnd, RecordedProcessEnd};
pub use claimed_control::{ClaimedSessionControlError, ClaimedSessionRealizationControl};
pub use claimed_coordination::ClaimedSessionCoordinationControl;
pub use coordination::{
    CoordinatedAdmissionRef, CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadKind,
    CoordinatedThreadLink, CoordinatedThreadTarget, PreparedSessionRunActivity,
    SessionAgentBoundaryAction, SessionAgentBoundaryCommand, SessionAgentBoundaryRole,
    SessionAgentBoundaryVisibility, SessionAgentCommittedReport, SessionAgentCoordination,
    SessionAgentMessageCommand, SessionAgentMessageReceipt, SessionAgentReportContinuation,
    SessionAgentRosterEntry, SessionAgentTarget, SessionRunActivityAdmission,
    SessionRunActivityAdmissionMode, admit_event_batch_topology, coordinated_activation_input,
    coordinated_run_id, coordinated_thread_closed, coordinated_thread_id,
    coordinated_thread_is_interruptible, interrupt_child_targets, session_agent_boundary_action,
    session_agent_report_messages, session_agent_report_text, session_agent_run_boundary,
    session_run_activity_operation_id, validate_interrupt_children,
    validate_tool_reply_coordinates,
};
pub use durable_run_operations::{
    ApplicationError, ApplicationErrorKind, DurableDispatchStatus, DurableRunOperations,
};
pub use resource_operation_fence::ResourceOperationFence;
pub use run_admission::{
    AdmitPreparedSessionRun, AdmitSessionRun, AdmittedSessionRun, PreparedSessionRunAdmission,
    SessionRunActivation, SessionRunActivityAdmitter, SessionRunActivityReceiptPlan,
    SessionRunDelivery, SessionRunExecutionRequirements, SessionRunReservation,
    session_run_activity_receipt_plan, session_run_id,
};
pub use session::{
    BudgetAccounting, CapabilityRuntime, ChildRunCoordination, ColocatedRealizationRoles,
    CommittedEvidenceError, CommittedOutcomeProjection, CommittedSessionFacts,
    CommittedSessionProjection, CompleteSessionRuntime, EnvironmentRuntime, LiveInboxAccess,
    McpAttachmentRealizer, NO_COLOCATED_WORKER, NeutralRuntime, OutcomeDrive, OutcomeFailure,
    OutcomeIteration, OutcomeReport, Outcomes, ProjectionThreadSnapshots, RealizationEffects,
    RealizationRuntime, SessionCoordinationReads, SessionEnvironmentBindingSink,
    SessionExecutionClosureReference, SessionExecutionEvidenceSelection,
    SessionExecutionObservation, SessionExecutionOperationFootprint,
    SessionExecutionOperationInventory, SessionExecutionRecoveryGap,
    SessionExecutionRecoveryObservation, SessionExecutionRoles, SessionExecutionToolOperation,
    SessionExecutor, SessionInputAdmission, SessionModelRequestObservation, SessionPlacementRoles,
    SessionProjectionPrefix, SessionProjectionReadTimings, SessionProjectionSource,
    SessionRecoveryPreparationReference, SessionRunExecutionObservation, SessionRuns,
    SessionSourceDispatchObservation, SessionThreadLiveSubscription, SessionToolUseObservation,
    StreamingRuns, TerminalCleanup, ThreadReads, committed_lifecycle_prefix,
    drain_mcp_generation_receipt, outcome_terminal_commit_cursor,
    projection_validation_needs_trailing_root, publish_mcp_generation_receipt,
    same_thread_projection_prefix, select_recovery_snapshot_run,
};

pub use session_realization::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    CanonicalPublicationError, CanonicalSessionProjection, CanonicalSessionPublication,
    ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, ENVIRONMENT_PROVISIONING_CODE,
    ENVIRONMENT_REJECTED_CODE, ENVIRONMENT_UNAVAILABLE_CODE, FailSessionRealization,
    FrozenAgentPublicationDecision, FrozenProjectionReceipt, FrozenResourceTransitionUse,
    FrozenSessionProjection, ObservedSessionRealizationLease, ProjectionReceiptRefusal,
    RealizationFailureOutcome, RenewSessionRealization,
    SESSION_ENVIRONMENT_REBUILD_SOURCE_UNAVAILABLE_CODE, SESSION_REALIZATION_NOT_READY_CODE,
    STALE_PROJECTION_CODE, SessionCheckpointArtifactControl, SessionFileReadAuthorization,
    SessionPreparationAssignment, SessionPreparationControl, SessionPreparationPlacement,
    SessionProjectionInstallMode, SessionProjectionSynchronizer, SessionRealizationAction,
    SessionRealizationControl, SessionRealizationControlDisposition,
    SessionRealizationControlFailure, SessionRealizationDirective, SessionRealizationDriveError,
    SessionRealizationEffectFailure, SessionRealizationExclusionDecision,
    SessionRealizationLeaseReceipt, SessionRealizationPermit, SessionRealizationTarget,
    SessionRepositoryPublicationProjection, SessionResourceMaterializationControl,
    SessionResourceMaterializationIntent, SessionResourceMaterializationTarget,
    SessionTerminalCleanupAssignment, SessionTerminalCleanupControl,
    SessionTerminalCleanupPreparationAuthorization, SessionTerminalCleanupQueue,
    SessionTerminalCleanupWork, SessionTerminalDisposalControl, SessionTerminalMemoryControl,
    SessionTerminalPreparationControl, SessionTerminalRepositoryPublicationControl,
    SessionTerminalSourceDispatch, TERMINAL_CLEANUP_SWEEP_CURSOR_VERSION,
    WorkerSessionRealizationControl, effect_failure_disposition, environment_effect_error,
    frozen_agent_publication_decision, frozen_sandbox_layout_publication_decision,
    frozen_session_requires_opaque_process, is_environment_effect_code, is_not_ready_code,
    is_permanent_conflict_code, legacy_agent_publication_decision,
    model_candidates_require_opaque_process, project_effective_agent_publication,
    provisioning_disposition, realization_generation_authorizes, requires_local_environment,
    resolve_canonical_session_projection, session_realization_exclusion_admits,
    synchronize_or_supersede, terminal_cleanup_worker_requirements,
    validate_terminal_no_create_witness,
};
pub use source_retention::{
    SourceRetentionSubject, WORKER_SOURCE_RETENTION_SUBJECTS_PATH, WorkerSourceRetentionSubjects,
};

#[cfg(test)]
mod mcp_attachment {
    pub(crate) fn test_generation(
        attachment: &str,
        version: u64,
        url: &str,
    ) -> awaken_session_configuration_contract::mcp::McpGeneration {
        awaken_session_configuration_contract::mcp::McpGeneration::of_bare_target(
            awaken_session_configuration_contract::mcp::McpAttachmentId(attachment.into()),
            version,
            &awaken_agent_contract::McpTarget::parse_http(url).unwrap(),
        )
    }
}
