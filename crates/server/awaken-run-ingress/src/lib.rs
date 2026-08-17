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
//! through the commit boundary's single `CommittedThreadView` port.

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
#[cfg(feature = "durable")]
mod postgres_identity;
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
    HeartbeatWorkerRequest, RecoveryRequest, RegisterWorkerRequest, RelinquishRequest,
    RenewRequest, SettleRequest, StreamEventRequest, WorkerIdentityRequest,
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
pub use pool::{CompletionSink, DispatchMaintenance, DispatchPool, WorkerResolver};
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

#[cfg(feature = "durable")]
fn next_supersession_epoch(max_epoch: i64) -> Result<i64, DispatchError> {
    max_epoch.checked_add(1).ok_or_else(|| {
        DispatchError::Rejected(
            "dispatch supersession epoch exhausted; refusing to wrap ordering authority"
                .to_string(),
        )
    })
}

#[cfg(feature = "durable")]
fn next_claim_epoch(previous_epoch: i64) -> Result<u64, DispatchError> {
    let next = previous_epoch.checked_add(1).ok_or_else(|| {
        DispatchError::Rejected(
            "dispatch claim epoch exhausted; refusing to wrap fencing authority".to_string(),
        )
    })?;
    u64::try_from(next).map_err(|_| {
        DispatchError::Rejected(
            "persisted dispatch claim epoch is negative; refusing to normalize fencing authority"
                .to_string(),
        )
    })
}

/// Decode one non-negative SQL integer used as durable ordering authority.
///
/// SQL backends expose signed integers while the domain uses unsigned values.
/// Treating a corrupt negative value as zero would silently mint fresh authority;
/// this boundary therefore fails closed instead of normalizing it.
#[cfg(feature = "durable")]
fn durable_u64(field: &str, value: i64) -> Result<u64, DispatchError> {
    u64::try_from(value).map_err(|_| {
        DispatchError::Rejected(format!(
            "persisted {field} is negative; refusing to normalize durable authority"
        ))
    })
}

/// Encode one unsigned domain counter for SQL without wrapping at `i64::MAX`.
#[cfg(feature = "durable")]
fn durable_i64(field: &str, value: u64) -> Result<i64, DispatchError> {
    i64::try_from(value).map_err(|_| {
        DispatchError::Rejected(format!(
            "{field} exceeds the durable SQL integer range; refusing to wrap authority"
        ))
    })
}

#[cfg(test)]
mod supersession_epoch_tests {
    use super::*;

    #[test]
    fn supersession_epoch_overflow_fails_closed() {
        assert_eq!(next_supersession_epoch(0).unwrap(), 1);
        assert!(matches!(
            next_supersession_epoch(i64::MAX),
            Err(DispatchError::Rejected(message))
                if message.contains("refusing to wrap ordering authority")
        ));
    }

    #[test]
    fn claim_epoch_rejects_negative_and_exhausted_database_values() {
        assert_eq!(next_claim_epoch(0).unwrap(), 1);
        assert!(matches!(
            next_claim_epoch(-2),
            Err(DispatchError::Rejected(message)) if message.contains("negative")
        ));
        assert!(matches!(
            next_claim_epoch(i64::MAX),
            Err(DispatchError::Rejected(message)) if message.contains("refusing to wrap")
        ));
    }

    #[test]
    fn durable_integer_boundary_rejects_negative_and_oversized_authority() {
        assert_eq!(durable_u64("lease epoch", 7).unwrap(), 7);
        assert!(matches!(
            durable_u64("lease epoch", -1),
            Err(DispatchError::Rejected(message)) if message.contains("negative")
        ));
        assert_eq!(durable_i64("lease epoch", 7).unwrap(), 7);
        assert!(matches!(
            durable_i64("lease epoch", u64::MAX),
            Err(DispatchError::Rejected(message)) if message.contains("refusing to wrap")
        ));
    }

    #[test]
    fn supersession_epochs_fail_closed_at_the_sql_boundary() {
        assert!(matches!(
            next_supersession_epoch(i64::MAX),
            Err(DispatchError::Rejected(message))
                if message.contains("dispatch supersession epoch exhausted")
        ));
    }
}

/// A durable-ingress failure: either the dispatch store rejected an operation or
/// a runtime attempt failed. Kept as two arms so a queue-storage failure never
/// masquerades as a run execution failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Dispatch(#[from] dispatch::DispatchError),
    #[error(transparent)]
    Execution(#[from] awaken_runtime_contract::execution::Error),
    /// Session resolution identified an absorbing application failure; the
    /// still-current Run claim must commit/settle the corresponding Run failure
    /// instead of pretending the Worker crashed.
    #[error(transparent)]
    TerminalResolution(awaken_runtime_contract::execution::Error),
    /// The claimed Run is valid, but its Session's Environment Work slot is
    /// still occupied. This is scheduling pressure, not a Worker crash or an
    /// absorbing Session failure.
    #[error("Session resolution is not ready: {0}")]
    ResolutionNotReady(String),
}

impl Error {
    #[must_use]
    pub fn is_terminal_resolution(&self) -> bool {
        matches!(self, Self::TerminalResolution(_))
    }

    #[must_use]
    pub fn is_resolution_not_ready(&self) -> bool {
        matches!(self, Self::ResolutionNotReady(_))
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
