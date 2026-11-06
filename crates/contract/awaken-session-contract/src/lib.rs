// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Publishes the existing Session aggregate values, transition laws and persistence roles from one authority; no parallel model or effect owner is introduced.
//! The Session Core: aggregate state, Event admission, exact budgets and persistence roles.
//!
//! Ownership compile table C1: the original public vocabulary is available from
//! this one root, while identity/error primitives keep their SessionTypes owner.
//! C2: a repository still upcasts to its SAME reader and maintenance roles;
//! these type-only examples grant no new mutation, lease or effect authority.
//!
//! ```no_run
//! use std::sync::Arc;
//! use awaken_session_contract::{SessionAggregate, SessionBudgetState, SessionEventBatch,
//!     SessionRepository, SessionRepositoryReads, SessionReconciliation};
//! use awaken_session_types::SessionId;
//! fn roles(store: Arc<dyn SessionRepository>, _: SessionId) {
//!     let _: Arc<dyn SessionRepositoryReads> = store.clone();
//!     let _: Arc<dyn SessionReconciliation> = store;
//! }
//! let _: Option<(SessionAggregate, SessionBudgetState, SessionEventBatch)> = None;
//! ```
//!
//! C3: each retired package spelling is independently unavailable. No alias,
//! wrapper crate or compatibility re-export can satisfy the old imports.
//!
//! ```compile_fail
//! use awaken_session_state_contract::SessionAggregate;
//! ```
//!
//! ```compile_fail
//! use awaken_session_event_contract::SessionEventBatch;
//! ```
//!
//! ```compile_fail
//! use awaken_session_budget_contract::SessionBudgetState;
//! ```
//!
//! ```compile_fail
//! use awaken_session_repository_contract::SessionRepository;
//! ```
//!
//!
//! # Boundary
//!
//! Between the Session application and its store (aggregate, Event admission, budgets, persistence
//! roles).
//!
//! Implemented by: `awaken-coordinator`, `awaken-protocol-managed`, `awaken-scenario-host`,
//! `awaken-session-application`, `awaken-session-store`, `awaken-webhook`.
//!
//! Called by: `awaken-coordinator`, `awaken-coordinator-client`, `awaken-dispatch`,
//! `awaken-dispatch-execution`, `awaken-dispatch-http`, `awaken-managed-session-events`,
//! `awaken-process-startup`, `awaken-protocol-managed`, `awaken-protocol-managed-testkit`,
//! `awaken-resource-worker-http`, `awaken-scenario-host`, `awaken-session-application`,
//! `awaken-session-execution-contract`, `awaken-session-executor`, `awaken-session-mcp`,
//! `awaken-session-realization`, `awaken-session-realization-driver`,
//! `awaken-session-run-delivery`, `awaken-session-store`, `awaken-store-conformance`,
//! `awaken-webhook`.

#![warn(unreachable_pub)]

mod budget;
mod event;
mod repository;
mod state;

pub use budget::{
    BudgetReachTransition, ManagedBudgetUsageCursor, ManagedListPriceError,
    ManagedListPriceProvider, ManagedListPriceRequest, ManagedListPriceSnapshot,
    ManagedModelUsageCursor, ManagedRuntimeListRates, ManagedTokenListRates, SessionBudgetState,
    SessionModelUsage, SessionUsage,
};
pub use event::{
    CompositeLifecycleFactDelivery, DeletedSessionEventAdmissions, LifecycleFactDelivery,
    MAX_SESSION_INITIAL_EVENTS, ManagedLifecycleFact, OUTCOME_BUSY_CODE, RetainedEventAdmission,
    SESSION_TERMINATED, SessionAgentRunBoundary, SessionEventAdmission, SessionEventBatch,
    SessionEventBatchAdmissionAction, SessionEventBatchAdmissionClass,
    SessionEventBatchAdmissionPlan, SessionEventBatchError, SessionEventBatchOperation,
    SessionEventCommand, SessionEventDispatchSettlementPlan, SessionEventDispatchSettlementReceipt,
    SessionEventEntry, SessionEventInput, SessionEventInterrupt, SessionEventProjectionAnchor,
    SessionEventToolReply, SessionEventToolReplyKind, SessionInitialEventPlan,
    SessionOutcomeRubric, SessionRuntimeInterval, SessionRuntimeIntervalObservation,
    SessionRuntimeIntervalStart, SessionThreadTarget, SessionThreadToolReply,
    SessionThreadToolReplyActivity, SessionThreadToolReplyCommand, SessionThreadToolReplyDelivery,
    SessionThreadToolReplyFence, SessionUserRunCommand, SessionUserRunSystemInput,
    classify_session_event_batch_admission, decode_session_event_batch_operation,
    event_admission_closed, event_admission_conflict, resolve_deleted_event_admission,
    resolve_session_event_batch_admission, session_event_batch_id, session_event_batch_operation,
    session_event_dispatch_settlement_plan, session_event_execution_may_close,
    session_event_outcome_id, session_event_user_run_id, session_inputs_allow_idle,
    session_outcome_convenience_id, session_terminated_fact_id,
};
pub use repository::{
    LifecycleOutbox, PreparedParticipantIntent, PreparedParticipantIntentClaim,
    PreparedParticipantIntentPhase, PreparedParticipantIntentRecord, PreparedParticipantIntents,
    SessionReconciliation, SessionRepository, SessionRepositoryReads,
};
pub use state::{
    ActivationState, CheckpointReceipt, IdempotencyRecord, MaterializationKind, PersistedSession,
    QuiescenceReceipt, RealizationLeaseStanding, RebuildSourceAccess, RebuildSourceEvidence,
    ResourceActivationError, RestoreReceipt, SandboxCheckpointRequest, SandboxGeneration,
    ScopedPersistedSession, SessionActivityAdmissionPlan, SessionActivityOperations,
    SessionActivitySettlementPlan, SessionAggregate, SessionAggregateCommandError,
    SessionArchiveAdmission, SessionArchiveBusinessIntent,
    SessionArchiveWithRepositoryPublicationError, SessionAssignmentOperations,
    SessionBudgetOperations, SessionCleanupCommand, SessionCleanupDisposalCommand,
    SessionCleanupDisposalReceipt, SessionCleanupError, SessionCleanupOperation,
    SessionCleanupOperations, SessionCleanupPreparation, SessionCleanupRepositoryPreparation,
    SessionCommandReceiptKind, SessionConfigurationOperations, SessionCreateResult,
    SessionDisposition, SessionDispositionTransitionError, SessionEnvironmentDecision,
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind, SessionEnvironmentEvent, SessionEnvironmentFacts,
    SessionEnvironmentInstruction, SessionEnvironmentMaterialization, SessionEnvironmentOperation,
    SessionEnvironmentOperations, SessionEnvironmentPhase, SessionEnvironmentReceipt,
    SessionEnvironmentReceiptError, SessionEnvironmentReceiptRecoveryAction,
    SessionEnvironmentRejection, SessionEnvironmentState, SessionEnvironmentStep,
    SessionEnvironmentTransitionError, SessionEventDispatchSettlementEffect, SessionEventHistory,
    SessionEventsOperations, SessionExecutionState, SessionExecutionStateError,
    SessionExecutionTransitionError, SessionFieldUpdate, SessionIdempotencyReceipt,
    SessionLifecycleOperations, SessionMcpOperations, SessionMemoryReconciliationError,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionMutationValidationError,
    SessionPlacementClass, SessionPreparationFailure, SessionPreparationObservation,
    SessionPreparationStatus, SessionPublicStatus, SessionRealizationAssignment,
    SessionRealizationBegin, SessionRealizationBeginRejection, SessionRealizationBeginRequest,
    SessionRealizationProgress, SessionRepositoryConflict, SessionRepositoryError,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection, SessionRepositoryPublicationRejectionReason,
    SessionRepositoryRecoveryAction, SessionResourceActivation, SessionResourceLocalApply,
    SessionResourceReconciliationInstruction, SessionResourceReferences, SessionResourceState,
    SessionResourcesOperations, SessionRunActivityAdmissionRecord, SessionScan, SessionScanCursor,
    SessionScanQuery, SessionTerminalCleanupAction, SessionTerminalCleanupAffinity,
    SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect,
    SessionTerminalMemoryIntent, SessionTerminalMemoryTarget, SessionTerminality, SessionTombstone,
    SessionUpdateAgentConfiguration, SessionUpdateChanges, SessionUpdateFact, SessionUpdateHistory,
    SessionWorkerObligation, SourceDisposedReceipt, SourceReleaseDisposal,
    SourceReleasePreparationEffect, SourceReleasePreparedReceipt, SuspendPhase,
    checkpoint_source_disposal_authorized, checkpoint_source_preparation_authorized,
    failed_profiled_activity_matches, failed_profiled_run_for_archive, session_activity_open_plan,
    session_activity_settlement_plan, session_archive_idempotency_record,
    session_budget_pause_absorbs_interrupt, session_budget_pause_may_absorb_interrupt,
    session_can_adopt_mcp_credential_lifecycle, session_has_no_create_evidence,
    session_is_budget_paused, session_is_terminal, session_is_unattempted_profiled_preparation,
    session_realization_assignment, session_root_update_observation,
    session_terminal_cleanup_affinity_for, session_unattempted_profiled_archive_record,
    terminal_memory_reconciliation_intent, terminal_memory_reconciliation_intents,
    terminal_memory_reconciliation_intents_from_materializations, transition_environment,
    validate_complete_run_activity, validate_continuation_memory_materializations,
    validate_continuation_memory_reconciliation,
};
