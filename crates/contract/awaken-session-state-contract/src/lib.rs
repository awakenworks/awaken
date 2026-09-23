//! Durable Session aggregate state and pure transition laws.

mod environment;
mod event_retention;
mod lifecycle;
mod realization_progress;
mod resource_activation;
mod session_repo;
mod terminal_cleanup;

pub(crate) use awaken_agent_contract::stable_fingerprint;
#[cfg(test)]
pub(crate) use awaken_session_budget_contract::ManagedModelUsageCursor;
pub(crate) use awaken_session_budget_contract::{ManagedBudgetUsageCursor, SessionBudgetState};
#[cfg(test)]
pub(crate) use awaken_session_configuration_contract::{
    ControlSessionCreationInputs, EnvironmentFingerprint, EnvironmentSnapshot,
    SessionCreationIntent, SessionNetworkPolicy, SessionRuntimePlacement,
    SessionSystemPromptSelection,
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
pub(crate) use awaken_session_mcp_contract::{
    McpGenerationRef, McpTarget, SessionMcpAttachmentSet,
};
pub(crate) use awaken_session_resource_contract::{
    ResolvedInput, ResolvedInputSource, ResolvedSessionResources, ResolvedSkillBinding,
    SessionRealizationLease, SessionResourceManifest, SessionResourceTransition,
    realization_lease_authorizes, realization_lease_generation_authorizes,
};
#[cfg(test)]
pub(crate) use awaken_session_resource_contract::{
    ResolvedRepositoryCredential, SessionMutationPolicy, repository_transport_credential_target,
    repository_transport_credential_usage,
};
pub(crate) use awaken_session_types::{SessionId, SessionRevision};

pub use environment::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
    SandboxCheckpointRequest, SandboxGeneration, SandboxRestoreRequest,
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind, SessionEnvironmentOperation, SessionEnvironmentPhase,
    SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentReceiptRecoveryAction, SessionEnvironmentState,
    SessionEnvironmentTransitionError, SourceDisposedReceipt, SourceReleaseDisposal,
    SourceReleasePreparationEffect, SourceReleasePreparedReceipt, SuspendPhase,
    checkpoint_source_disposal_authorized, checkpoint_source_preparation_authorized,
};
pub use event_retention::{DeletedSessionEventAdmissions, RetainedEventAdmission};
pub use lifecycle::{
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
    SessionRecoveryQuarantine, SessionRepositoryConflict, SessionRepositoryError,
    SessionRepositoryRecoveryAction, SessionScan, SessionScanCursor, SessionScanQuery,
    SessionTombstone, VisibleMcpServer,
};
pub use terminal_cleanup::{
    SessionCleanupCommand, SessionCleanupDisposalCommand, SessionCleanupDisposalReceipt,
    SessionCleanupError, SessionCleanupOperation, SessionCleanupPreparation,
    SessionCleanupRepositoryPreparation, SessionMemoryReconciliationError,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection, SessionTerminalCleanupAction,
    SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect,
    SessionTerminalMemoryIntent, SessionTerminalMemoryTarget,
    terminal_memory_reconciliation_intent, terminal_memory_reconciliation_intents,
    terminal_memory_reconciliation_intents_from_materializations,
    validate_continuation_memory_materializations, validate_continuation_memory_reconciliation,
};
