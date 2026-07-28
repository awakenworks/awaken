//! The dispatch / run-ingress contract (ADR-0039 slice 2.1).
//!
//! This is the durable-dispatch bounded context's port surface, factored out of
//! the `awaken-run-ingress` host so store backends and adapters can depend on the
//! contract without the host crate (G2). It carries the neutral dispatch ports
//! (`DispatchQueue` / `Inbox` / `Outbox`, bundled as `Dispatch`) and the
//! serializable [`RunDispatch`] a durable queue persists and replays — no live
//! handles (G3). Worker wiring stays private to the host.

pub mod dispatch;
pub mod operational;
pub mod run_dispatch;

pub use awaken_runtime_contract::{
    AttemptCredentialBinding, CandidateFingerprint, CredentialRealizationReceipt,
    CredentialReceiptError, verify_credential_realization_receipt,
};
pub use awaken_worker_contract::{
    AssignmentRejection, ExecutionLocation, HOST_EXECUTOR_CAPABILITY, LeastLoadedPolicy,
    PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, PlacementContext, PlacementError, PlacementPolicy,
    REPOSITORY_CREDENTIALS_CAPABILITY, RankedWorker, RegisteredWorker, RegistryError,
    RegistryMutation, SESSION_RESOURCES_CAPABILITY, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerAcpCapabilityObservation, WorkerAcpCapabilityRequirement, WorkerAssignment,
    WorkerCredentialObservation, WorkerCredentialRevision, WorkerCredentialState, WorkerDirectory,
    WorkerHeartbeat, WorkerIdentity, WorkerManifest, WorkerRecoveryMode, WorkerRegistration,
    WorkerSnapshot, WorkerState, can_assign, can_claim, can_claim_locally, place_assignment,
};
pub use dispatch::{
    AttemptCredentialBindingError, CasOutcome, Claimed, ClaimedCommitCommand, CommitEpochGuard,
    Dispatch, DispatchCompletion, DispatchError, DispatchOutcome, DispatchQueue, DispatchState,
    DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim, SettleOutcome,
    SubmitOptions, compile_attempt_credential_bindings, worker_credential_realization_capabilities,
};
pub use operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
pub use run_dispatch::{
    ExecutionScopeRef, PlacementRequirements, RunDispatch, SessionResourceEnvelope,
};
