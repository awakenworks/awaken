// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: This source implements the repository-controlled ingress product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Durable run dispatch: the dispatch/server host above the runtime core.
//!
//! Direct attempts and durable delivery are intentionally different shapes
//! (G5). The runtime ships `DirectAttemptDriver`; the dispatch contract owns
//! `RunDispatch` and the queue ports, this crate owns the Coordinator's
//! SQLite/PostgreSQL dispatch adapters and claimed-commit application, and
//! `awaken-dispatch-execution` owns the sole `DispatchWorker`/`DispatchPool`.
//! The split adds durability without owning the runtime loop, agent truth, or
//! a second commit mechanism (G6). Committed facts remain the single authority
//! (G1/G13). Historical reexports preserve caller source compatibility without
//! retaining another execution implementation here.
//!
//! Source-owner rule W4: the old host WakeSignal export is removed; callers
//! import its existing two-method port from dispatch-contract. No new wake
//! implementation or delivery law is introduced by this source control.
//! ```compile_fail,E0432
//! use awaken_dispatch::WakeSignal;
//! ```
//!
//! The aggregates follow the dispatch design's DDD split: [`DispatchEnqueue`] and [`DispatchClaims`] owns
//! delivery opportunity (claim/lease/recovery), [`Inbox`] owns the
//! thread's pending input, and run outcome stays in committed facts, read back
//! through the commit boundary's single `CommittedThreadView` port.

pub use awaken_dispatch_execution::{
    NativeAttemptFootprint, NativeAttemptTermination, NativeAttemptTerminationError,
    classify_native_attempt_termination,
};
#[cfg(feature = "durable")]
mod any_factory;
mod application;
mod clock;
mod dispatch;
mod dispatch_schema;
// Synchronous composition convenience for conformance and embedded fixtures.
// Production Hosts compose DispatchWorker/DispatchPool directly and therefore
// cannot depend on the former parallel ingress façade.
#[cfg(feature = "test-support")]
mod durable;
#[cfg(any(feature = "durable", test, kani))]
mod inbox_mutation;
#[cfg(any(test, feature = "test-support"))]
pub mod memory;
#[cfg(feature = "durable")]
mod postgres;
#[cfg(feature = "durable")]
mod postgres_checkpoint;
#[cfg(feature = "durable")]
mod postgres_helpers;
#[cfg(feature = "durable")]
mod postgres_identity;
#[cfg(feature = "durable")]
mod sqlite;
mod wake;

#[cfg(all(feature = "durable", any(test, feature = "test-support")))]
pub use any_factory::open_sqlite_in_memory as open_sqlite_dispatch_store_in_memory;
#[cfg(feature = "durable")]
pub use any_factory::{
    connect_postgres as connect_postgres_dispatch_store,
    connect_postgres_existing as connect_postgres_existing_dispatch_store,
    connect_postgres_existing_with_wake as connect_postgres_existing_dispatch_store_with_wake,
    connect_postgres_with_wake as connect_postgres_dispatch_store_with_wake,
    open_sqlite as open_sqlite_dispatch_store,
    with_existing_postgres_pool as with_existing_postgres_dispatch_pool,
    with_existing_postgres_pool_and_wake as with_existing_postgres_dispatch_pool_and_wake,
    with_postgres_pool as with_postgres_dispatch_pool,
    with_postgres_pool_and_wake as with_postgres_dispatch_pool_and_wake,
};
#[cfg(feature = "nats")]
pub use any_factory::{
    connect_postgres_existing_with_nats_wake as connect_postgres_existing_dispatch_store_with_nats_wake,
    connect_postgres_with_nats_wake as connect_postgres_dispatch_store_with_nats_wake,
};
pub use application::{
    AttemptClosureKind, AttemptStopStatus, ClaimedCommitService,
    ClaimedOperationCoordinatorResolver, ClosedAttemptExecution, ClosedWorkerExecution,
    ObservedAttemptStop, ObservedWorkerProcessExit, WorkerEvidenceReleaseAdapter,
    WorkerExecutionBindingSource, WorkerExecutionStopSource, WorkerProcessStopStatus,
    has_active_physical_attempt_for_owner, reconcile_closed_idle_worker_evidence,
    release_closed_idle_worker_evidence, stop_committed_terminal_attempt, stop_expired_predecessor,
};
pub use awaken_dispatch_contract::DispatchAttemptRecordReads;
pub use awaken_dispatch_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
pub use awaken_dispatch_contract::{
    AssignmentRejection, CURRENT_RUNTIME_PROTOCOL_VERSION, ClaimedCommitCommand, ExecutionLocation,
    ExecutionScopeRef, HOST_EXECUTOR_CAPABILITY, LeastLoadedPolicy,
    PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, PlacementContext,
    PlacementError, PlacementPolicy, PlacementRequirements, REPOSITORY_CREDENTIALS_CAPABILITY,
    RankedWorker, RegisteredWorker, RegistryError, RegistryMutation, RunDispatch,
    SESSION_RESOURCES_CAPABILITY, SessionResourceEnvelope, SessionRuntimeEnvelope,
    TRUSTED_HOST_SANDBOX_BACKEND, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerAcpCapabilityObservation, WorkerAcpCapabilityRequirement, WorkerAssignment,
    WorkerCreateLedger, WorkerCredentialObservation, WorkerCredentialRevision,
    WorkerCredentialState, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerObservationSource, WorkerProcessExit, WorkerRecoveryMode, WorkerRegistration,
    WorkerSnapshot, WorkerState, can_assign, can_claim, can_claim_locally, place_assignment,
    retry_exhaustion_eligible, worker_credential_realization_capabilities,
};
pub use awaken_dispatch_contract::{
    AttemptExecutionRequest, BindSandboxRequest, CheckpointRequest, ClaimNewRunRequest,
    ClaimRunRequest, ClaimWorkerRequest, ClaimedCommitRequest, CredentialDeliveryRefreshRequest,
    CredentialRealizationRequest, DeliverAndClaimRequest, EnqueueRequest, HeartbeatWorkerRequest,
    RecoveryRequest, RegisterWorkerRequest, RelinquishRequest, RenewRequest,
    SessionRunReservationResolutionRequest, SettleRequest, StreamEventRequest,
    StreamObservationRequest, WorkerIdentityRequest,
};
pub use awaken_dispatch_contract::{ClaimedStreamPublisher, SessionContinuationOutbox};
pub use awaken_dispatch_execution::{AnyDispatchStore, Dispatch};
pub use awaken_dispatch_execution::{
    ClaimedCommitCoordinator, ClaimedModelRequestGateFactory, ClaimedRunCommit, CompletionSink,
    ControlDispatch, DEFAULT_LEASE_MS, DispatchPool, DispatchReconciler, DispatchService,
    DispatchServiceConfig, DispatchWorker, FencedStreamCheckpointStore, GuardedRunCommit,
    InferenceMaterializerFn, LiveRunControlError, LiveRunControlService, LocalWakeSignal,
    PoolDrainFence, RecoveryProjection, RecoveryProjectionError, TerminalReconciliationScan,
    WorkerResolver, claim_bound_ownership_verifier, publish_dispatch_hint,
};
pub use dispatch::{
    AttemptAcknowledgementRecord, AttemptAdmission, AttemptClosureEvent, AttemptClosurePage,
    AttemptClosureRecord, AttemptCredentialBinding, AttemptExecutionBinding, AttemptPhysicalFact,
    AttemptReturnCause, CasOutcome, Claimed, ClaimedCheckpointTransport,
    ClaimedCredentialDeliveryTransport, CommitEpochGuard, ContinuationAdmission,
    CredentialRealizationReceipt, DispatchCompletion, DispatchDepths, DispatchError, DispatchLease,
    DispatchOutcome, DispatchSettlementError, DispatchSettlementObserver, DispatchState,
    DispatchSummary, Inbox, LeaseRenewalOutcome, Outbox, PendingInput, PendingRecord,
    PhysicalAttempt, RunClaim, SessionChildAdmission, SessionRunReservationActivation,
    SessionRunReservationOutcome, SessionRunReservationResolution, SettleOutcome, SubmitOptions,
};
pub use dispatch_schema::dispatch_bundle;
#[cfg(feature = "test-support")]
pub use durable::DispatchTestHarness;
#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryDispatchStore;
#[cfg(feature = "durable")]
pub use postgres::{
    PostgresDispatchStore, PostgresStreamCheckpointStore, StoreError as PostgresStoreError,
};
#[cfg(feature = "durable")]
pub use sqlite::{SqliteDispatchStore, StoreError as SqliteStoreError};
#[cfg(feature = "nats")]
pub use wake::NatsWakeSignal;
#[cfg(feature = "durable")]
pub use wake::PgNotifyWake;

#[cfg(any(feature = "durable", test))]
fn next_supersession_epoch(max_epoch: i64) -> Result<i64, DispatchError> {
    max_epoch.checked_add(1).ok_or_else(|| {
        DispatchError::Rejected(
            "dispatch supersession epoch exhausted; refusing to wrap ordering authority"
                .to_string(),
        )
    })
}

#[cfg(any(feature = "durable", test))]
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

/// Decode the authority-bearing SQL columns into the one transition kernel.
/// Every SQL adapter uses this boundary before choosing a next state; unknown
/// vocabulary and negative epochs fail closed instead of creating local policy.
#[cfg(feature = "durable")]
fn persisted_dispatch_transition(
    status: &str,
    lease_epoch: i64,
    cancellation_requested: bool,
) -> Result<awaken_dispatch_contract::DispatchTransition, DispatchError> {
    let state = DispatchState::from_db(status).ok_or_else(|| {
        DispatchError::Rejected(format!("unknown persisted dispatch state `{status}`"))
    })?;
    Ok(awaken_dispatch_contract::DispatchTransition {
        state,
        lease_epoch: durable_u64("dispatch lease epoch", lease_epoch)?,
        cancellation_requested,
    })
}

#[cfg(feature = "durable")]
fn dispatch_state_db(state: DispatchState) -> &'static str {
    match state {
        DispatchState::Reserved => "reserved",
        DispatchState::ReservationLeased => "reservation_running",
        DispatchState::Pending => "pending",
        DispatchState::Leased => "running",
        DispatchState::Awaiting => "awaiting",
        DispatchState::DeadLetter => "dead_letter",
        DispatchState::Superseded => "superseded",
    }
}

#[cfg(feature = "durable")]
fn transition_error(error: awaken_dispatch_contract::DispatchTransitionError) -> DispatchError {
    match error {
        awaken_dispatch_contract::DispatchTransitionError::LeaseEpochExhausted => {
            DispatchError::Rejected(
                "dispatch claim epoch exhausted; refusing to wrap fencing authority".to_string(),
            )
        }
    }
}

/// Decode one non-negative SQL integer used as durable ordering authority.
///
/// SQL backends expose signed integers while the domain uses unsigned values.
/// Treating a corrupt negative value as zero would silently mint fresh authority;
/// this boundary therefore fails closed instead of normalizing it.
#[cfg(any(feature = "durable", test))]
fn durable_u64(field: &str, value: i64) -> Result<u64, DispatchError> {
    u64::try_from(value).map_err(|_| {
        DispatchError::Rejected(format!(
            "persisted {field} is negative; refusing to normalize durable authority"
        ))
    })
}

/// Encode one unsigned domain counter for SQL without wrapping at `i64::MAX`.
#[cfg(any(feature = "durable", test))]
fn durable_i64(field: &str, value: u64) -> Result<i64, DispatchError> {
    i64::try_from(value).map_err(|_| {
        DispatchError::Rejected(format!(
            "{field} exceeds the durable SQL integer range; refusing to wrap authority"
        ))
    })
}

pub use awaken_dispatch_execution::Error;

/// Shared policy adapter used by every durable backend. Eligibility and
/// replacement authority remain in the worker-contract kernel; stores only use
/// the boolean result while holding their backend-specific claim lock.
#[cfg(any(feature = "durable", test))]
pub(crate) struct DispatchPlacement<'a> {
    pub recovered: bool,
    pub previous: Option<&'a WorkerAssignment>,
    pub sandbox_bound: bool,
    pub requester: &'a WorkerIdentity,
    pub workers: &'a [WorkerSnapshot],
    pub now_ms: u64,
}

#[cfg(any(feature = "durable", test))]
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

pub use application::AttemptControlAuthority;
pub use awaken_dispatch_contract::{
    DispatchAdministration, DispatchAttemptControl, DispatchClaimOwnership, DispatchClaims,
    DispatchCommitAuthority, DispatchControlClaims, DispatchControlReads, DispatchEnqueue,
    DispatchExecution, DispatchMaintenance, DispatchReads, DispatchSettlement, InboxRead,
    SessionReservationAuthority, SessionRunReservations,
};
pub use awaken_dispatch_execution::{
    AnyCoordinatorDispatch, DispatchBinding, DispatchTopology, DurableDispatch, RemoteDispatch,
};

pub use awaken_dispatch_execution::{PhysicalAttemptReads, PhysicalAttemptRecoveryRead};

#[cfg(test)]
mod supersession_epoch_tests {
    use super::*;

    #[test]
    fn supersession_epoch_overflow_fails_closed() {
        // S1 a representable successor advances once; S2 the SQL maximum has
        // no successor and rejects without wrapping. One case owns both bounds
        // and the exact diagnostic, including in the database-less unit graph.
        assert_eq!(next_supersession_epoch(0).unwrap(), 1);
        assert!(matches!(
            next_supersession_epoch(i64::MAX),
            Err(DispatchError::Rejected(message))
                if message == "dispatch supersession epoch exhausted; refusing to wrap ordering authority"
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
}
