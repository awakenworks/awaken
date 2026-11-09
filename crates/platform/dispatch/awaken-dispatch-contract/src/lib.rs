// @formal surface exclude=module_facade
// @formal owner crates/platform/dispatch/awaken-dispatch-contract/src/lib.rs
// @formal reason: Dispatch contract declarations and public re-exports only; dispatch, claim,
// @formal reason: placement, and settlement decisions remain inventoried in their defining modules.
//! The dispatch contract (ADR-0039 slice 2.1).
//!
//! This is the durable-dispatch bounded context's port surface, factored out of
//! the `awaken-dispatch` host so store backends and adapters can depend on the
//! contract without the host crate (G2). It carries the neutral dispatch ports
//! (caller-specific dispatch roles / `Inbox` / `Outbox`) and the
//! serializable [`RunDispatch`] a durable queue persists and replays — no live
//! handles (G3). Worker wiring stays private to the host.
//!
//! Worker placement, registry and claim vocabulary is named from
//! `awaken-worker-contract`, credential receipts from `awaken-credential-contract`
//! and the execution scope from `awaken-agent-contract`; this contract publishes
//! none of them:
//!
//! ```compile_fail
//! use awaken_dispatch_contract::RunClaim;
//! ```
//!
//! ```compile_fail
//! use awaken_dispatch_contract::ExecutionLocation;
//! ```
//!
//! # Boundary
//!
//! Context: `dispatch`.
//!
//! Implemented by: `awaken-coordinator-client`, `awaken-dispatch`, `awaken-dispatch-erasure`,
//! `awaken-dispatch-execution`, `awaken-session-application`, `awaken-session-executor`,
//! `awaken-session-run-delivery`.
//!
//! Called by: `awaken-coordinator`, `awaken-coordinator-client`, `awaken-dispatch`,
//! `awaken-dispatch-erasure`, `awaken-dispatch-execution`, `awaken-dispatch-http`,
//! `awaken-dispatch-testkit`, `awaken-protocol-managed`, `awaken-resource-worker-http`,
//! `awaken-session-application`, `awaken-session-executor`, `awaken-session-realization`,
//! `awaken-session-run-delivery`.

mod claimed_commit;
mod claimed_stream;
mod completion_sink;
pub mod dispatch;
mod dispatch_transition;
mod execution_observation;
pub mod operational;
pub mod run_dispatch;
mod session_run_events;
mod wake;
mod worker_transport;
pub use completion_sink::CompletionSink;
pub use execution_observation::DispatchExecutionReads;
pub use wake::WakeSignal;

pub use session_run_events::SessionRunEventPage;

pub use claimed_commit::{
    COMMIT_CONFLICT, COMMIT_FENCED, COMMIT_IDENTITY_MISMATCH, COMMIT_OUTCOME_UNKNOWN,
    ClaimedRunCommit,
};
pub use claimed_stream::ClaimedStreamPublisher;
pub use dispatch::{
    AttemptAcknowledgementRecord, AttemptAdmission, AttemptClosureEvent, AttemptClosureKind,
    AttemptClosurePage, AttemptClosureRecord, AttemptPhysicalFact, AttemptReturnCause, CasOutcome,
    ClaimCredentialOperation, Claimed, ClaimedCheckpointTransport, ClaimedCommitCommand,
    ClaimedCredentialDeliveryTransport, CommitEpochAdmission, CommitEpochBoundary,
    CommitEpochCompletion, CommitEpochGuard, ContinuationAdmission, DispatchCompletion,
    DispatchCredentialAdmissionError, DispatchDepths, DispatchError, DispatchLease,
    DispatchOutcome, DispatchSettlementError, DispatchSettlementObserver, DispatchState,
    DispatchSummary, Inbox, LeaseRenewalOutcome, Outbox, PendingInput, PendingRecord,
    PreparedRunRecovery, SessionChildAdmission, SessionContinuationOutbox,
    SessionRunAdmissionObservation, SessionRunReservationActivation, SessionRunReservationOutcome,
    SessionRunReservationResolution, SettleOutcome, SubmitOptions, can_admit_claim_credentials,
    claim_credential_operation, classify_commit_epoch_admission,
    compile_attempt_credential_bindings, compile_claim_credential_bindings, deadline_has_passed,
    decide_lease_renewal, session_run_replacement_candidate_is_safe,
    verify_attempt_credential_bindings, verify_claim_credential_bindings,
    worker_credential_realization_capabilities,
};
pub use dispatch::{
    DispatchAdministration, DispatchAttemptControl, DispatchClaimOwnership, DispatchClaims,
    DispatchCommitAuthority, DispatchControlClaims, DispatchControlReads, DispatchEnqueue,
    DispatchExecution, DispatchMaintenance, DispatchReads, DispatchSettlement,
    SessionReservationAuthority, SessionRunAdmissionReads, SessionRunReservations,
};
pub use dispatch_transition::{
    BLOCKED_RECLAIM_LIMIT, CancelTransition, DispatchTransition, DispatchTransitionError,
    GuardedTransition, PhysicalAttemptSlot, PhysicalAttemptTransition, ReleasedAttempt,
    RelinquishTransition, attempt_epoch_is_open, begin_physical_attempt,
    blocked_reclaim_attempt_count, can_close_physical_attempt, finish_physical_attempt,
    handoff_physical_attempt, retry_exhaustion_eligible,
};
pub use operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
pub use run_dispatch::{
    DispatchAdmissionShape, DispatchIdentityScope, RunDispatch, SessionResourceEnvelope,
    SessionRuntimeEnvelope,
};
pub use worker_transport::{
    AttemptExecutionRequest, BindSandboxRequest, CheckpointRequest, ClaimNewRunRequest,
    ClaimRunRequest, ClaimWorkerRequest, ClaimedCommitRequest, CredentialDeliveryRefreshRequest,
    CredentialRealizationRequest, DeliverAndClaimRequest, EnqueueRequest, HeartbeatWorkerRequest,
    MAX_CLAIMED_COMMIT_BODY_BYTES, RecoveryRequest, RegisterWorkerRequest, RelinquishRequest,
    RenewRequest, SessionRunReservationResolutionRequest, SettleRequest, StreamEventRequest,
    StreamObservationRequest, WorkerHeartbeatReceipt, WorkerIdentityRequest,
    WorkerRegistrationReceipt,
};

pub use dispatch::InboxRead;

pub use dispatch::DispatchAttemptRecordReads;
