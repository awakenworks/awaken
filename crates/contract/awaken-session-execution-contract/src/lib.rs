//! The Session ↔ execution boundary: the Session roles execution calls, coordination
//! and Run admission, and the one late binding from execution back to the Session
//! application.

mod application_binding;
mod claimed_coordination;
mod coordination;
mod durable_run_operations;
mod run_admission;
mod session;

#[cfg(test)]
pub(crate) use awaken_agent_contract::McpTarget;
pub(crate) use awaken_agent_contract::stable_fingerprint;
#[cfg(test)]
pub(crate) use awaken_session_application_contract::session_usage_from_thread_usage;
pub(crate) use awaken_session_application_contract::{
    DelegatedRun, DelegatedRunSnapshot, Pending, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket, StepOutcome,
};
pub(crate) use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, McpProjectionEffectKind, McpProjectionReceipt, McpRealizationReceipt,
    StageMcpAttachment,
};
pub(crate) use awaken_session_configuration_contract::resource::{
    ResolvedSkillBinding, SessionRealizationLease, SessionResourceTransition,
};
pub(crate) use awaken_session_configuration_contract::{
    AgentCapabilities, SessionSandboxLayout, SessionToolConfiguration,
};
#[cfg(test)]
pub(crate) use awaken_session_contract::SessionUserRunSystemInput;
pub(crate) use awaken_session_realization_contract::{
    FrozenSessionProjection, ObservedSessionRealizationLease, SessionProjectionInstallMode,
    SessionRealizationControl, SessionTerminalCleanupAssignment,
    SessionTerminalCleanupPreparationAuthorization,
};
pub(crate) use awaken_session_types::RunError;
#[cfg(test)]
pub(crate) use awaken_session_types::RunErrorKind;
pub(crate) use awaken_session_types::SessionId;
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
pub use claimed_coordination::ClaimedSessionCoordinationControl;
pub use coordination::{
    CoordinatedAdmissionRef, CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadKind,
    CoordinatedThreadLink, CoordinatedThreadTarget, PreparedSessionRunActivity,
    SessionAgentBoundaryAction, SessionAgentBoundaryCommand, SessionAgentBoundaryRole,
    SessionAgentBoundaryVisibility, SessionAgentCommittedReport, SessionAgentCoordination,
    SessionAgentMessageCommand, SessionAgentMessageReceipt, SessionAgentReportContinuation,
    SessionAgentRosterEntry, SessionAgentRunBoundary, SessionAgentTarget,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode, SessionThreadTarget,
    SessionThreadToolReply, SessionThreadToolReplyActivity, SessionThreadToolReplyCommand,
    SessionThreadToolReplyDelivery, SessionThreadToolReplyFence, admit_event_batch_topology,
    coordinated_activation_input, coordinated_run_id, coordinated_thread_closed,
    coordinated_thread_id, coordinated_thread_is_interruptible, interrupt_child_targets,
    session_agent_boundary_action, session_agent_report_messages, session_agent_report_text,
    session_agent_run_boundary, session_run_activity_operation_id, validate_interrupt_children,
    validate_tool_reply_coordinates,
};
pub use durable_run_operations::{
    ApplicationError, ApplicationErrorKind, DurableDispatchStatus, DurableRunOperations,
};
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
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, McpAttachmentRealizer, NO_COLOCATED_WORKER, NeutralRuntime, OutcomeDrive,
    OutcomeFailure, OutcomeIteration, OutcomeReport, Outcomes, ProjectionThreadSnapshots,
    RealizationEffects, RealizationRuntime, SessionCoordinationReads,
    SessionEnvironmentBindingSink, SessionExecutionClosureReference,
    SessionExecutionEvidenceSelection, SessionExecutionObservation,
    SessionExecutionOperationFootprint, SessionExecutionOperationInventory,
    SessionExecutionRecoveryGap, SessionExecutionRecoveryObservation, SessionExecutionRoles,
    SessionExecutionToolOperation, SessionExecutor, SessionInputAdmission,
    SessionModelRequestObservation, SessionPlacementRoles, SessionProjectionReadTimings,
    SessionProjectionSource, SessionRecoveryPreparationReference, SessionRunExecutionObservation,
    SessionRuns, SessionSourceDispatchObservation, SessionThreadLiveSubscription,
    SessionToolUseObservation, StreamingRuns, TerminalCleanup, ThreadReads, ToolPermissionDecision,
    committed_lifecycle_prefix, drain_mcp_generation_receipt, outcome_terminal_commit_cursor,
    publish_mcp_generation_receipt, select_recovery_snapshot_run,
};

#[doc(hidden)]
pub mod __macro_support {
    pub use async_trait;
    pub use awaken_agent_contract;
    pub use awaken_session_application_contract::{
        DelegatedRun, DelegatedRunSnapshot, Pending, SessionBudgetResumeDelivery,
        SessionBudgetResumeDisposition, SessionBudgetResumeTicket, StepOutcome,
    };
    pub use awaken_session_configuration_contract::mcp::McpGenerationRef;
    pub use awaken_session_configuration_contract::resource::{
        ResolvedSkillBinding, SessionRealizationLease, SessionResourceTransition,
    };
    pub use awaken_session_configuration_contract::{
        AgentCapabilities, SessionSandboxLayout, SessionToolConfiguration,
    };
    pub use awaken_session_contract::SessionUsage;
    pub use awaken_session_contract::session_outcome_convenience_id;
    pub use awaken_session_realization_contract::{
        FrozenSessionProjection, ObservedSessionRealizationLease, SessionProjectionInstallMode,
        SessionRealizationControl, SessionTerminalCleanupAssignment,
        SessionTerminalCleanupPreparationAuthorization,
    };
    pub use awaken_session_types::RunError;
    pub use awaken_session_types::SessionId;
    pub use {
        awaken_provisioning_contract::SandboxCheckpointRef,
        awaken_provisioning_contract::SandboxRestoreRequest,
        awaken_session_contract::CheckpointReceipt, awaken_session_contract::QuiescenceReceipt,
        awaken_session_contract::RestoreReceipt, awaken_session_contract::SandboxCheckpointRequest,
        awaken_session_contract::SandboxGeneration,
        awaken_session_contract::SessionCleanupDisposalReceipt,
        awaken_session_contract::SessionCleanupPreparation,
        awaken_session_contract::SessionEnvironmentOperation,
        awaken_session_contract::SessionRepositoryPublicationCommand,
        awaken_session_contract::SessionRepositoryPublicationEffect,
        awaken_session_contract::SessionTerminalCleanupDisposalEffect,
        awaken_session_contract::SessionTerminalCleanupEffect,
        awaken_session_contract::SourceDisposedReceipt,
        awaken_session_contract::SourceReleaseDisposal,
        awaken_session_contract::SourceReleasePreparationEffect,
        awaken_session_contract::SourceReleasePreparedReceipt,
    };
}

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
