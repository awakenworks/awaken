//! The dispatch / run-ingress contract (ADR-0039 slice 2.1).
//!
//! This is the durable-dispatch bounded context's port surface, factored out of
//! the `awaken-run-ingress` host so store backends and adapters can depend on the
//! contract without the host crate (G2). It carries the neutral dispatch ports
//! (`DispatchQueue` / `Inbox` / `Outbox`, bundled as `Dispatch`) and the
//! serializable [`RunDispatch`] a durable queue persists and replays — no live
//! handles (G3). Worker wiring stays private to the host.

mod claimed_commit;
mod claimed_session;
mod claimed_stream;
pub mod dispatch;
mod dispatch_transition;
pub mod operational;
pub mod run_dispatch;
mod worker_transport;

pub use awaken_runtime_contract::{
    AttemptCredentialBinding, CandidateFingerprint, CredentialRealizationReceipt,
    CredentialReceiptError, verify_credential_realization_receipt,
};
pub use awaken_session_contract::SessionRunReplacement;
pub use awaken_worker_contract::{
    AssignmentRejection, ExecutionLocation, HOST_EXECUTOR_CAPABILITY, LeastLoadedPolicy,
    PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, PlacementContext,
    PlacementError, PlacementPolicy, REPOSITORY_CREDENTIALS_CAPABILITY, RankedWorker,
    RegisteredWorker, RegistryError, RegistryMutation, SESSION_RESOURCES_CAPABILITY,
    WORKER_LOCAL_CREDENTIALS_CAPABILITY, WorkerAcpCapabilityObservation,
    WorkerAcpCapabilityRequirement, WorkerAssignment, WorkerCredentialObservation,
    WorkerCredentialRevision, WorkerCredentialState, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerObservationSource, WorkerRecoveryMode,
    WorkerRegistration, WorkerSnapshot, WorkerState, can_assign, can_claim, can_claim_locally,
    place_assignment,
};
pub use claimed_commit::ClaimedRunCommit;
pub use claimed_session::{ClaimedSessionControl, ClaimedSessionControlError};
pub use claimed_stream::ClaimedStreamPublisher;
pub use dispatch::{
    AttemptAdmission, AttemptCredentialBindingError, CasOutcome, Claimed, ClaimedCommitCommand,
    CommitEpochGuard, ContinuationAdmission, Dispatch, DispatchCompletion,
    DispatchCredentialAdmissionError, DispatchError, DispatchOutcome, DispatchQueue,
    DispatchSettlementError, DispatchSettlementObserver, DispatchState, DispatchSummary, Inbox,
    Lease, Outbox, PendingInput, PendingRecord, RunClaim, SessionChildAdmission,
    SessionRunReservationActivation, SessionRunReservationOutcome, SessionRunReservationResolution,
    SettleOutcome, SubmitOptions, compile_attempt_credential_bindings,
    session_run_replacement_candidate_is_safe, worker_credential_realization_capabilities,
};
pub use dispatch_transition::{
    CancelTransition, DispatchTransition, DispatchTransitionError, GuardedTransition,
    retry_exhaustion_eligible,
};
pub use operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
pub use run_dispatch::{
    DispatchAdmissionShape, DispatchIdentityScope, ExecutionScopeRef, PlacementRequirements,
    RunDispatch, SessionResourceEnvelope, SessionRuntimeEnvelope,
};
pub use worker_transport::{
    AttemptExecutionRequest, BindSandboxRequest, CheckpointRequest, ClaimNewRunRequest,
    ClaimRunRequest, ClaimWorkerRequest, ClaimedCommitRequest, CredentialRealizationRequest,
    DeliverAndClaimRequest, EnqueueRequest, HeartbeatWorkerRequest, RecoveryRequest,
    RegisterWorkerRequest, RelinquishRequest, RenewRequest, SessionRunReservationResolutionRequest,
    SettleRequest, StreamEventRequest, StreamObservationRequest, WorkerHeartbeatReceipt,
    WorkerIdentityRequest, WorkerRegistrationReceipt,
};

/// The one closed fencing vocabulary for Runtime-authored Resource effects.
/// Ordinary Run work retains its dispatch claim; terminal recovery carries the
/// aggregate-derived cleanup command and realization lease; checkpoint source
/// release carries the existing Environment operation whose root state already
/// owns the durable checkpoint. Resources remains generic over this value and
/// owns no execution authority of its own.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "type",
    content = "fence",
    rename_all = "snake_case",
    deny_unknown_fields
)]
// This is the single short-lived, closed transport union for mutually
// exclusive Resource operations. Boxing only the terminal arm would change
// the public Rust contract while preserving the same wire authority.
#[allow(clippy::large_enum_variant)]
pub enum ResourceOperationFence {
    Run(RunClaim),
    CheckpointRelease(awaken_session_contract::SessionEnvironmentOperation),
    Terminal(awaken_session_contract::SessionTerminalCleanupEffect),
}

impl From<RunClaim> for ResourceOperationFence {
    fn from(claim: RunClaim) -> Self {
        Self::Run(claim)
    }
}

/// Source-compatible name retained for Artifact publishers. It is an alias,
/// not a second fencing enum or wire grammar.
pub type ArtifactPublicationFence = ResourceOperationFence;
