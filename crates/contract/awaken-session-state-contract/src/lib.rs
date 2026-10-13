//! Durable Session aggregate state and pure transition laws.

mod aggregate;
pub use aggregate::{
    SessionActivityAdmissionPlan, SessionActivityOperations, SessionAggregate,
    SessionAggregateCommandError, SessionAssignmentOperations, SessionBudgetOperations,
    SessionCleanupOperations, SessionConfigurationOperations, SessionEnvironmentOperations,
    SessionEventsOperations, SessionLifecycleOperations, SessionMcpOperations,
    SessionResourcesOperations, session_activity_open_plan, session_root_update_observation,
};
mod archive_intent;
mod environment;
mod event_history;
pub use event_history::SessionEventHistory;
mod realization_progress;
mod resource_activation;
mod session_repo;
mod terminal_cleanup;

pub(crate) use awaken_agent_contract::stable_fingerprint;
#[cfg(test)]
pub(crate) use awaken_session_budget_contract::ManagedModelUsageCursor;
pub(crate) use awaken_session_budget_contract::{ManagedBudgetUsageCursor, SessionBudgetState};
pub(crate) use awaken_session_configuration_contract::resource::{
    ResolvedInput, ResolvedInputSource, ResolvedSessionResources, ResolvedSkillBinding,
    SessionRealizationLease, SessionResourceManifest, SessionResourceTransition,
    realization_lease_authorizes, realization_lease_generation_authorizes,
};
#[cfg(test)]
pub(crate) use awaken_session_configuration_contract::resource::{
    ResolvedRepositoryCredential, SessionMutationPolicy, repository_transport_credential_target,
    repository_transport_credential_usage,
};
pub(crate) use awaken_session_configuration_contract::{
    EnvironmentIdleRetentionMode, EnvironmentIdleRetentionPolicy, SessionBaseline,
    SessionBaselineState, SessionToolConfiguration,
};
#[cfg(test)]
pub(crate) use awaken_session_event_contract::SessionEventInput;
pub(crate) use awaken_session_event_contract::{
    SessionAgentRunBoundary, SessionEventBatch, SessionEventBatchError, SessionEventCommand,
    SessionEventDispatchSettlementPlan, SessionEventDispatchSettlementReceipt,
    SessionEventProjectionAnchor, SessionInitialEventPlan, session_event_execution_may_close,
    session_inputs_allow_idle,
};
pub(crate) use awaken_session_types::{SessionId, SessionRevision};
pub(crate) use {
    awaken_agent_contract::McpTarget,
    awaken_session_configuration_contract::mcp::{McpGenerationRef, SessionMcpAttachmentSet},
};
#[cfg(test)]
pub(crate) use {
    awaken_session_configuration_contract::{
        ControlSessionCreationInputs, EnvironmentFingerprint, EnvironmentSnapshot,
        SessionCreationIntent, SessionNetworkPolicy, SessionSystemPromptSelection,
    },
    awaken_session_types::SessionRuntimePlacement,
};

pub use archive_intent::{
    SessionArchiveAdmission, SessionArchiveBusinessIntent, SessionTerminalCleanupAffinity,
    session_archive_idempotency_record, session_has_terminal_no_create_evidence,
    session_terminal_cleanup_affinity_for, session_unattempted_profiled_archive_record,
};
pub use environment::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
    SandboxCheckpointRequest, SandboxGeneration, SandboxRestoreRequest,
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind, SessionEnvironmentMaterialization, SessionEnvironmentOperation,
    SessionEnvironmentPhase, SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentReceiptRecoveryAction, SessionEnvironmentState,
    SessionEnvironmentTransitionError, SourceDisposedReceipt, SourceReleaseDisposal,
    SourceReleasePreparationEffect, SourceReleasePreparedReceipt, SuspendPhase,
    checkpoint_source_disposal_authorized, checkpoint_source_preparation_authorized,
};
// These values occur in the root persistence surface; their owner is Event admission.
pub use awaken_session_event_contract::{DeletedSessionEventAdmissions, RetainedEventAdmission};
pub use awaken_session_event_contract::{
    ManagedLifecycleFact, SessionRuntimeInterval, SessionRuntimeIntervalObservation,
    SessionRuntimeIntervalStart,
};
pub use realization_progress::SessionRealizationProgress;
pub use resource_activation::{
    ActivationState, ResourceActivationError, SessionResourceActivation, SessionResourceReferences,
    SessionResourceState,
};
pub use session_repo::{
    IdempotencyRecord, PersistedSession, ScopedPersistedSession,
    SessionArchiveWithRepositoryPublicationError, SessionCreateResult, SessionDisposition,
    SessionDispositionTransitionError, SessionEventDispatchSettlementEffect, SessionExecutionState,
    SessionExecutionStateError, SessionExecutionTransitionError, SessionIdempotencyReceipt,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionMutationValidationError,
    SessionPlacementClass, SessionPublicStatus, SessionRepositoryConflict, SessionRepositoryError,
    SessionRepositoryRecoveryAction, SessionRunActivityAdmissionRecord, SessionScan,
    SessionScanCursor, SessionScanQuery, SessionTombstone, SessionWorkerObligation,
    VisibleMcpServer, session_is_terminal, session_is_unattempted_profiled_preparation,
};
pub use terminal_cleanup::{
    SessionCleanupCommand, SessionCleanupDisposalCommand, SessionCleanupDisposalReceipt,
    SessionCleanupError, SessionCleanupOperation, SessionCleanupPreparation,
    SessionCleanupRepositoryPreparation, SessionMemoryReconciliationError,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection, SessionRepositoryPublicationRejectionReason,
    SessionTerminalCleanupAction, SessionTerminalCleanupDisposalEffect,
    SessionTerminalCleanupEffect, SessionTerminalMemoryIntent, SessionTerminalMemoryTarget,
    terminal_memory_reconciliation_intent, terminal_memory_reconciliation_intents,
    terminal_memory_reconciliation_intents_from_materializations,
    validate_continuation_memory_materializations, validate_continuation_memory_reconciliation,
};
