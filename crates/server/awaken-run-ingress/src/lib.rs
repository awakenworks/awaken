//! Durable run ingress: the dispatch/server host above the runtime core.
//!
//! `RunIngress` has exactly two delivery semantics (G5): direct, shipped by the
//! runtime as `DirectRunIngress`, and durable, shipped here as
//! [`DurableRunIngress`]. This crate owns the durable half — the dispatch queue,
//! pending input, claim/lease/recovery, and the worker that turns a durable
//! dispatch into a runtime attempt. It sits *above* the runtime (it depends on
//! the kernel; the kernel never depends on it) and adds durability over runtime
//! control without owning the loop, agent truth, or a second commit mechanism
//! (G6). Committed facts remain the single authority (G1/G13).
//!
//! The aggregates follow the run-ingress design's DDD split: [`DispatchQueue`] owns
//! delivery opportunity (claim/lease/recovery), [`Inbox`] owns the
//! thread's pending input, and run outcome stays in committed facts, read back
//! through the commit boundary's `ThreadReader`/`RunStore` ports.

mod any;
mod application;
mod capability;
mod claimed_stream;
mod clock;
mod commit_fence;
mod commit_operation;
mod dispatch;
mod dispatch_schema;
mod durable;
mod fenced_checkpoint;
mod live_control;
#[cfg(any(test, feature = "test-support"))]
pub mod memory;
mod pool;
#[cfg(feature = "durable")]
mod postgres;
mod recovery_projection;
mod send_message;
mod service;
#[cfg(feature = "durable")]
mod sqlite;
mod wake;
mod worker;
mod worker_context;

pub use any::AnyDispatchStore;
pub use application::{
    ApplicationError, ApplicationErrorKind, ClaimedCommitApplier, ClaimedCommitService,
    DurableDispatchStatus, DurableRunOperations, DurableSupersedeResult,
};
pub use capability::RunIngressCapabilities;
// The database-less worker's HTTP dispatch client (drives claim/settle over the wire
// to the Coordinator's registered Worker router), extracted from awaken-runtime-host.
pub use awaken_run_ingress_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
pub use awaken_run_ingress_contract::{
    AssignmentRejection, ClaimedCommitCommand, ExecutionLocation, ExecutionScopeRef,
    HOST_EXECUTOR_CAPABILITY, LeastLoadedPolicy, PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE,
    PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, PlacementContext, PlacementError, PlacementPolicy,
    PlacementRequirements, REPOSITORY_CREDENTIALS_CAPABILITY, RankedWorker, RegisteredWorker,
    RegistryError, RegistryMutation, RunDispatch, SESSION_RESOURCES_CAPABILITY,
    SessionResourceEnvelope, SessionRuntimeEnvelope, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerAcpCapabilityObservation, WorkerAcpCapabilityRequirement, WorkerAssignment,
    WorkerCredentialObservation, WorkerCredentialRevision, WorkerCredentialState, WorkerDirectory,
    WorkerHeartbeat, WorkerIdentity, WorkerManifest, WorkerObservationSource, WorkerRecoveryMode,
    WorkerRegistration, WorkerSnapshot, WorkerState, can_assign, can_claim, can_claim_locally,
    place_assignment, worker_credential_realization_capabilities,
};
pub use awaken_run_ingress_contract::{
    BindSandboxRequest, CheckpointRequest, ClaimNewRunRequest, ClaimRunRequest, ClaimWorkerRequest,
    ClaimedCommitRequest, CredentialRealizationRequest, DeliverAndClaimRequest, EnqueueRequest,
    HeartbeatWorkerRequest, RecoveryRequest, RegisterWorkerRequest, RenewRequest, SettleRequest,
    StreamEventRequest, WorkerIdentityRequest,
};
pub use claimed_stream::ClaimedStreamPublisher;
pub use clock::{Clock, ManualClock, SystemClock};
pub use commit_fence::{ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit};
pub use commit_operation::{CommitHashError, commit_payload_hash};
pub use dispatch::{
    AttemptCredentialBinding, CandidateFingerprint, CasOutcome, Claimed, CommitEpochGuard,
    CredentialRealizationReceipt, Dispatch, DispatchCompletion, DispatchError, DispatchOutcome,
    DispatchQueue, DispatchState, DispatchSummary, Inbox, Lease, Outbox, PendingInput,
    PendingRecord, RunClaim, SettleOutcome, SubmitOptions,
};
pub use dispatch_schema::dispatch_bundle;
pub use durable::DurableRunIngress;
pub use fenced_checkpoint::FencedStreamCheckpointStore;
pub use live_control::{Error as LiveRunControlError, LiveRunControlService};
#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryDispatchStore;
pub use pool::{CompletionSink, DispatchPool, WorkerResolver};
#[cfg(feature = "durable")]
pub use postgres::{
    PostgresDispatchStore, PostgresStreamCheckpointStore, StoreError as PostgresStoreError,
};
pub use recovery_projection::{RecoveryProjection, RecoveryProjectionError};
pub use send_message::OutboxMessageSender;
pub use service::{DispatchService, DispatchServiceConfig};
#[cfg(feature = "durable")]
pub use sqlite::{SqliteDispatchStore, StoreError as SqliteStoreError};
#[cfg(feature = "nats")]
pub use wake::NatsWakeSignal;
#[cfg(feature = "durable")]
pub use wake::PgNotifyWake;
pub use wake::{LocalWakeSignal, WakeSignal};
pub use worker::{DEFAULT_LEASE_MS, DispatchWorker, claim_bound_ownership_verifier};
pub use worker_context::InferenceMaterializerFn;

/// Canonical renewal cadence for a ten-second claim lease. Both Coordinator
/// and remote Worker pools use this policy; wake transport selection does not
/// change lease ownership.
pub const DEFAULT_LEASE_RENEWAL: std::time::Duration = std::time::Duration::from_secs(10);

/// A durable-ingress failure: either the dispatch store rejected an operation or
/// a runtime attempt failed. Kept as two arms so a queue-storage failure never
/// masquerades as a run execution failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Dispatch(#[from] dispatch::DispatchError),
    #[error(transparent)]
    Execution(#[from] awaken_runtime_contract::execution::Error),
    /// Session realization already committed its absorbing application failure;
    /// the still-current Run claim must now commit/settle the corresponding Run
    /// failure instead of pretending the Worker crashed.
    #[error(transparent)]
    TerminalResolution(awaken_runtime_contract::execution::Error),
}

impl Error {
    #[must_use]
    pub fn is_terminal_resolution(&self) -> bool {
        matches!(self, Self::TerminalResolution(_))
    }
}

/// Shared policy adapter used by every durable backend. Eligibility and
/// replacement authority remain in the worker-contract kernel; stores only use
/// the boolean result while holding their backend-specific claim lock.
#[cfg(feature = "durable")]
pub(crate) struct DispatchPlacement<'a> {
    pub recovered: bool,
    pub previous: Option<&'a WorkerAssignment>,
    pub sandbox_bound: bool,
    pub requester: &'a WorkerIdentity,
    pub workers: &'a [WorkerSnapshot],
    pub now_ms: u64,
}

#[cfg(feature = "durable")]
pub(crate) fn policy_selects_requester(
    request: &RunDispatch,
    policy: &dyn PlacementPolicy,
    attempt: DispatchPlacement<'_>,
) -> Result<bool, DispatchError> {
    let context = PlacementContext {
        run_id: request.run_id().0.clone(),
        workspace_id: request
            .execution_scope
            .as_ref()
            .map_or_else(String::new, |scope| scope.0.0.clone()),
        requirements: request.placement.clone(),
        recovered: attempt.recovered,
        previous_worker: attempt
            .previous
            .map(|assignment| assignment.identity.clone()),
        attributes: request
            .preferred_environment_shape
            .as_ref()
            .map(|shape| {
                [(
                    PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE.to_string(),
                    shape.to_string(),
                )]
                .into_iter()
                .collect()
            })
            .unwrap_or_default(),
    };
    match place_assignment(
        policy,
        &context,
        attempt.workers,
        attempt.previous,
        attempt.sandbox_bound,
        attempt.now_ms,
    ) {
        Ok(selected) => Ok(&selected.identity == attempt.requester),
        Err(PlacementError::NoEligibleWorker) => Ok(false),
        Err(error) => Err(DispatchError::Rejected(error.to_string())),
    }
}
