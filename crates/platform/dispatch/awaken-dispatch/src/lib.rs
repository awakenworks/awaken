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
mod worker_registration;
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
    record_occupied_worker_ends, release_closed_idle_worker_evidence,
    stop_committed_terminal_attempt, stop_expired_predecessor,
};
pub use awaken_agent_contract::ExecutionScopeRef;
pub use awaken_agent_contract::credential::CredentialRef;
pub use awaken_credential_contract::CredentialObservationState;
pub use awaken_credential_contract::{AttemptCredentialBinding, CredentialRealizationReceipt};
pub use awaken_dispatch_contract::DispatchAttemptRecordReads;
pub use awaken_dispatch_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalEvent, DispatchOperationalFeed,
    DispatchPage, LeaseLossReason,
};
pub use awaken_dispatch_contract::{
    AttemptExecutionRequest, BindSandboxRequest, CheckpointRequest, ClaimNewRunRequest,
    ClaimRunRequest, ClaimWorkerRequest, ClaimedCommitRequest, CredentialDeliveryRefreshRequest,
    CredentialRealizationRequest, DeliverAndClaimRequest, EnqueueRequest, HeartbeatWorkerRequest,
    RecoveryRequest, RegisterWorkerRequest, RelinquishRequest, RenewRequest,
    SessionRunReservationResolutionRequest, SettleRequest, StreamEventRequest,
    StreamObservationRequest, WorkerIdentityRequest,
};
pub use awaken_dispatch_contract::{
    ClaimedCommitCommand, RunDispatch, SessionResourceEnvelope, SessionRuntimeEnvelope,
    retry_exhaustion_eligible, worker_credential_realization_capabilities,
};
pub use awaken_dispatch_contract::{
    ClaimedStreamPublisher, CompletionSink, SessionContinuationOutbox,
};
pub use awaken_dispatch_execution::{AnyDispatchStore, Dispatch};
pub use awaken_dispatch_execution::{
    ClaimedCommitCoordinator, ClaimedModelRequestGateFactory, ClaimedRunCommit, ControlDispatch,
    DEFAULT_LEASE_MS, DispatchPool, DispatchReconciler, DispatchService, DispatchServiceConfig,
    DispatchWorker, FencedStreamCheckpointStore, GuardedRunCommit, InferenceMaterializerFn,
    LiveRunControlError, LiveRunControlService, LocalWakeSignal, PoolDrainFence, PoolShutdown,
    RecoveryProjection, RecoveryProjectionError, TerminalReconciliationScan, WorkerResolver,
    claim_bound_ownership_verifier, publish_dispatch_hint,
};
pub use awaken_worker_contract::RunClaim;
pub use awaken_worker_contract::{
    AssignmentRejection, CURRENT_RUNTIME_PROTOCOL_VERSION, ExecutionLocation,
    HOST_EXECUTOR_CAPABILITY, LeastLoadedPolicy, PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE,
    PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, PlacementContext, PlacementError, PlacementPolicy,
    PlacementRequirements, REPOSITORY_CREDENTIALS_CAPABILITY, RankedWorker, RegisteredWorker,
    RegistryError, RegistryMutation, SESSION_RESOURCES_CAPABILITY, TRUSTED_HOST_SANDBOX_BACKEND,
    WORKER_LOCAL_CREDENTIALS_CAPABILITY, WorkerAcpCapabilityObservation,
    WorkerAcpCapabilityRequirement, WorkerAssignment, WorkerCreateLedger,
    WorkerCredentialObservation, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerObservationSource, WorkerProcessExit, WorkerRecoveryMode, WorkerRegistration,
    WorkerSnapshot, WorkerState, can_assign, can_claim, can_claim_locally, place_assignment,
};
pub use dispatch::{
    AttemptAcknowledgementRecord, AttemptAdmission, AttemptClosureEvent, AttemptClosurePage,
    AttemptClosureRecord, AttemptExecutionBinding, AttemptPhysicalFact, AttemptReturnCause,
    CasOutcome, Claimed, ClaimedCheckpointTransport, ClaimedCredentialDeliveryTransport,
    CommitEpochGuard, ContinuationAdmission, DispatchCompletion, DispatchDepths, DispatchError,
    DispatchLease, DispatchOutcome, DispatchSettlementError, DispatchSettlementObserver,
    DispatchState, DispatchSummary, Inbox, LeaseRenewalOutcome, Outbox, PendingInput,
    PendingRecord, PhysicalAttempt, SessionChildAdmission, SessionRunReservationActivation,
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
    lock_running_claim_for_commit,
};
#[cfg(feature = "durable")]
pub use sqlite::{SqliteDispatchStore, StoreError as SqliteStoreError};
#[cfg(feature = "nats")]
pub use wake::NatsWakeSignal;
#[cfg(feature = "durable")]
pub use wake::PgNotifyWake;
pub use worker_registration::{RegisterWorker, WorkerRegistryCommands};

#[cfg(any(feature = "durable", test))]
fn next_supersession_epoch(max_epoch: i64) -> Result<i64, DispatchError> {
    max_epoch.checked_add(1).ok_or_else(|| {
        DispatchError::Rejected(
            "dispatch supersession epoch exhausted; refusing to wrap ordering authority"
                .to_string(),
        )
    })
}

/// A claim epoch past the signed column's range is exhausted storage, and is
/// refused through the same lease-generation class as the kernel's; a negative
/// one is the same permanent refusal ([`ClaimGenerationViolation`]).
#[cfg(any(feature = "durable", test))]
fn next_claim_epoch(previous_epoch: i64) -> Result<u64, DispatchError> {
    match sql_claim_generation_violation(previous_epoch) {
        Some(violation) => Err(claim_generation_integrity(violation)),
        None => Ok(previous_epoch.unsigned_abs() + 1), // nonnegative, below the last
    }
}

/// Why a persisted SQL claim generation cannot be trusted, if it cannot: the
/// signed column's last value has no successor (the exhausted class), and a
/// negative one is corrupt. Pure, so a claim scan classifies each candidate
/// once, at its own decision site.
#[cfg(any(feature = "durable", test))]
const fn sql_claim_generation_violation(previous_epoch: i64) -> Option<ClaimGenerationViolation> {
    if previous_epoch == i64::MAX {
        Some(ClaimGenerationViolation::Exhausted)
    } else if previous_epoch < 0 {
        Some(ClaimGenerationViolation::Negative)
    } else {
        None
    }
}

/// Why one dispatch's persisted claim generation is untrustworthy. Both are
/// the same permanent refusal of that one row, carrying
/// [`EpochIntegrityViolation::CODE`](awaken_lease_contract::fence::EpochIntegrityViolation::CODE).
/// Consumers: [`claim_generation_integrity`], the claim scans' observation and
/// their decision-table tests.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimGenerationViolation {
    /// The lease contract's class: the generation has no successor.
    Exhausted,
    /// A signed column below zero. No grant writes one (every generation
    /// counts up from the column's `DEFAULT 0`), so only corruption can.
    Negative,
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl std::fmt::Display for ClaimGenerationViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use awaken_lease_contract::fence::EpochIntegrityViolation;
        match self {
            Self::Exhausted => EpochIntegrityViolation::Exhausted.fmt(formatter),
            Self::Negative => write!(
                formatter,
                "{}: the persisted lease epoch is negative",
                EpochIntegrityViolation::CODE
            ),
        }
    }
}

/// The claim scans' admission of one candidate's SQL generation, observed
/// through the store's `reported` record: an untrustworthy generation is never
/// selected, and is reported once. An expired lease at the exhausted generation
/// never reaches a scan: each adapter dead-letters it first.
#[cfg(feature = "durable")]
fn sql_claim_generation_admits(
    reported: &ReportedClaimGenerations,
    run_id: &str,
    lease_epoch: i64,
) -> bool {
    match sql_claim_generation_violation(lease_epoch) {
        Some(violation) => {
            reported.observe_skipped(run_id, violation);
            false
        }
        None => true,
    }
}

/// What a claim scan did with one dispatch whose lease generation is
/// untrustworthy. Consumers: the claim scans of every adapter (in-memory,
/// SQLite, PostgreSQL), the PostgreSQL depth read, and their decision tables.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UntrustworthyClaimGeneration {
    /// An expired lease at the exhausted generation: dead-lettered through the
    /// kernel's one retry exhaustion transition, with `LeaseLost` naming the
    /// integrity code.
    DeadLettered,
    /// Not under an expired, settleable lease: never claimed by any scan,
    /// which moves on to the next candidate.
    Skipped(ClaimGenerationViolation),
}

/// The one error-level observation of a dispatch whose lease generation is
/// untrustworthy, carrying the stable integrity code. A dead-lettered row is
/// observed once, by the scan that settled it; a skipped row once per store,
/// through [`ReportedClaimGenerations`].
#[cfg(any(feature = "durable", test, feature = "test-support"))]
fn observe_untrustworthy_claim_generation(run_id: &str, disposition: UntrustworthyClaimGeneration) {
    match disposition {
        UntrustworthyClaimGeneration::DeadLettered => tracing::error!(
            target: "awaken::lease_generation_integrity",
            code = awaken_lease_contract::fence::EpochIntegrityViolation::CODE,
            lease = "dispatch_claim",
            run_id,
            disposition = "dead_lettered",
            violation = %ClaimGenerationViolation::Exhausted,
            "lease generation integrity violated; dead-lettered this dispatch permanently"
        ),
        UntrustworthyClaimGeneration::Skipped(violation) => tracing::error!(
            target: "awaken::lease_generation_integrity",
            code = awaken_lease_contract::fence::EpochIntegrityViolation::CODE,
            lease = "dispatch_claim",
            run_id,
            disposition = "skipped",
            %violation,
            "lease generation integrity violated; this dispatch is never claimed"
        ),
    }
}

/// The dispatches whose skipped, untrustworthy claim generation one store has
/// already reported. Every claim scan that reaches such a row skips it again;
/// this record makes the error-level report once per row, not once per scan.
///
/// No durable record of a report exists, so this one is process-local: a
/// restarted store reports each row once more. Past `capacity` distinct rows it
/// records no more and reports that saturation once, so neither memory nor the
/// log grows with the number of corrupt rows; nothing is evicted, so no row is
/// reported twice. Consumers: the adapters' claim scans and the PostgreSQL
/// depth read.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Debug)]
struct ReportedClaimGenerations {
    capacity: usize,
    rows: awaken_sync::LeafLock<ReportedRows>,
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Debug, Default)]
struct ReportedRows {
    run_ids: std::collections::BTreeSet<String>,
    saturated: bool,
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl Default for ReportedClaimGenerations {
    fn default() -> Self {
        Self::with_capacity(Self::CAPACITY)
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl ReportedClaimGenerations {
    /// Distinct rows reported individually by one store.
    const CAPACITY: usize = 1024;

    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            rows: awaken_sync::LeafLock::new(ReportedRows::default()),
        }
    }

    /// Report one skipped dispatch at error level the first time this store
    /// skips it; later skips of the same row are silent.
    fn observe_skipped(&self, run_id: &str, violation: ClaimGenerationViolation) {
        let mut rows = self.rows.lock();
        if rows.run_ids.contains(run_id) {
            return;
        }
        if rows.run_ids.len() >= self.capacity {
            if !std::mem::replace(&mut rows.saturated, true) {
                tracing::error!(
                    target: "awaken::lease_generation_integrity",
                    code = awaken_lease_contract::fence::EpochIntegrityViolation::CODE,
                    lease = "dispatch_claim",
                    capacity = self.capacity,
                    disposition = "unreported",
                    "lease generation integrity violated on more dispatches than are \
                     reported individually; further skipped dispatches are not reported"
                );
            }
            return;
        }
        rows.run_ids.insert(run_id.to_owned());
        drop(rows);
        observe_untrustworthy_claim_generation(
            run_id,
            UntrustworthyClaimGeneration::Skipped(violation),
        );
    }
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
        lease_epoch: stored_claim_epoch(lease_epoch)?,
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

/// The one projection of a transition-kernel refusal onto the store error,
/// shared by the in-memory and SQL adapters: [`claim_generation_integrity`] of
/// the exhausted class.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
fn transition_error(error: awaken_dispatch_contract::DispatchTransitionError) -> DispatchError {
    match error {
        awaken_dispatch_contract::DispatchTransitionError::LeaseEpochExhausted => {
            claim_generation_integrity(ClaimGenerationViolation::Exhausted)
        }
    }
}

/// The one projection of an untrustworthy dispatch claim generation onto the
/// store error: the kernel's exhausted refusal, a minted generation the signed
/// column cannot hold, and a negative persisted one all reach it. It is a
/// lease-generation integrity failure of that one dispatch row: `Rejected`
/// (this command did not commit, and retrying it cannot), carrying the stable
/// integrity code, observed once at error level. Nothing resets the epoch.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
fn claim_generation_integrity(violation: ClaimGenerationViolation) -> DispatchError {
    tracing::error!(
        target: "awaken::lease_generation_integrity",
        code = awaken_lease_contract::fence::EpochIntegrityViolation::CODE,
        lease = "dispatch_claim",
        %violation,
        "lease generation integrity violated; refusing this dispatch claim permanently"
    );
    let refusal = match violation {
        ClaimGenerationViolation::Exhausted => "wrap",
        ClaimGenerationViolation::Negative => "normalize",
    };
    DispatchError::Rejected(format!(
        "dispatch claim: {violation}; refusing to {refusal} fencing authority"
    ))
}

/// Decode one persisted SQL claim generation (a lease epoch, or the claim
/// epoch a physical attempt slot recorded). Negative is corruption of fencing
/// authority, refused through [`claim_generation_integrity`].
#[cfg(feature = "durable")]
fn stored_claim_epoch(value: i64) -> Result<u64, DispatchError> {
    u64::try_from(value).map_err(|_| claim_generation_integrity(ClaimGenerationViolation::Negative))
}

/// Encode a claim generation the kernel minted from a persisted one. The
/// signed column's last value has no successor there: the exhausted class,
/// refused through [`claim_generation_integrity`].
#[cfg(feature = "durable")]
fn minted_claim_epoch_column(minted: u64) -> Result<i64, DispatchError> {
    i64::try_from(minted)
        .map_err(|_| claim_generation_integrity(ClaimGenerationViolation::Exhausted))
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
    fn dispatch_claim_epoch_integrity_is_one_permanent_rejection() {
        // The kernel owns the successor law (`DispatchTransition::claim` and
        // its siblings); `transition_error` is the adapters' one projection.
        // | Rule | persisted lease_epoch                         | effect                                   |
        // |------|-----------------------------------------------|------------------------------------------|
        // | D1   | u64::MAX, any claimable state                 | LeaseEpochExhausted, every attempt       |
        // | D2   | signed column at i64::MAX                     | the same Rejected, integrity code        |
        // | D3   | 0 (the V0013 `DEFAULT 0` column / new row)    | never claimed: the claim mints epoch 1   |
        // | D4   | D1/D2 outward                                 | Rejected (did not commit), Terminal for  |
        // |      |                                               | the claim; never Unavailable or Conflict |
        use awaken_dispatch_contract::{DispatchTransition, DispatchTransitionError};
        use awaken_lease_contract::fence::EpochIntegrityViolation;
        for state in [
            DispatchState::Pending,
            DispatchState::Leased,
            DispatchState::Awaiting,
        ] {
            let exhausted = DispatchTransition {
                state,
                lease_epoch: u64::MAX,
                cancellation_requested: false,
            };
            for attempt in 0..3 {
                assert_eq!(
                    exhausted.claim(),
                    Err(DispatchTransitionError::LeaseEpochExhausted),
                    "D1 {state:?} attempt {attempt}"
                );
            }
            let fresh = DispatchTransition {
                lease_epoch: 0,
                ..exhausted
            };
            assert_eq!(
                fresh.claim().map(|next| next.map(|next| next.lease_epoch)),
                Ok(Some(1)),
                "D3 {state:?}"
            );
        }
        let kernel = transition_error(DispatchTransitionError::LeaseEpochExhausted);
        let column = next_claim_epoch(i64::MAX).unwrap_err();
        assert_eq!(kernel, column, "D2 one projection");
        let DispatchError::Rejected(message) = &kernel else {
            panic!("D4 Rejected, got {kernel:?}");
        };
        assert!(
            message.contains(EpochIntegrityViolation::CODE),
            "D4 greppable"
        );
        assert_eq!(
            awaken_dispatch_execution::AdmissionError::Dispatch(kernel).claim_disposition(),
            awaken_session_execution_contract::SessionRealizationControlDisposition::Terminal,
            "D4 the claim is never retried as a disposition"
        );
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

    /// Every untrustworthy SQL claim generation reaches the one projection.
    ///
    /// | Rule | stored / minted generation                     | effect                                       |
    /// |------|------------------------------------------------|----------------------------------------------|
    /// | E1   | minted successor of the column's last value    | the kernel's exhausted refusal, exactly       |
    /// | E2   | the column's last value, as a scan reads it    | the same refusal                              |
    /// | E3   | stored negative (any epoch column)             | Rejected, integrity code, Terminal claim      |
    /// | E4   | a negative claim base                          | the same refusal as E3                        |
    /// | E5   | 0, 7, and the column's last value as minted    | decoded / encoded unchanged (control)         |
    /// | E6   | a caller-supplied epoch past the column        | the ordinary range refusal, no integrity code |
    #[cfg(feature = "durable")]
    #[test]
    fn untrustworthy_sql_claim_generations_share_the_one_integrity_projection() {
        use awaken_dispatch_contract::DispatchTransitionError;
        use awaken_lease_contract::fence::EpochIntegrityViolation;
        let kernel = transition_error(DispatchTransitionError::LeaseEpochExhausted);
        let minted = minted_claim_epoch_column(i64::MAX.unsigned_abs() + 1).unwrap_err();
        assert_eq!(minted, kernel, "E1");
        assert_eq!(next_claim_epoch(i64::MAX).unwrap_err(), kernel, "E2");
        let negative = stored_claim_epoch(-1).unwrap_err();
        let DispatchError::Rejected(message) = &negative else {
            panic!("E3 Rejected, got {negative:?}");
        };
        assert!(
            message.contains(EpochIntegrityViolation::CODE) && message.contains("negative"),
            "E3 {message}"
        );
        assert_eq!(stored_claim_epoch(i64::MIN).unwrap_err(), negative, "E3");
        assert_eq!(
            awaken_dispatch_execution::AdmissionError::Dispatch(negative.clone())
                .claim_disposition(),
            awaken_session_execution_contract::SessionRealizationControlDisposition::Terminal,
            "E3 never retried"
        );
        assert_eq!(next_claim_epoch(-2).unwrap_err(), negative, "E4");
        assert_eq!(stored_claim_epoch(0).unwrap(), 0, "E5");
        assert_eq!(stored_claim_epoch(7).unwrap(), 7, "E5");
        assert_eq!(
            minted_claim_epoch_column(i64::MAX.unsigned_abs()).unwrap(),
            i64::MAX,
            "E5"
        );
        assert_eq!(
            next_claim_epoch(i64::MAX - 1).unwrap(),
            i64::MAX.unsigned_abs(),
            "E5"
        );
        let DispatchError::Rejected(range) =
            durable_i64("dispatch lease epoch", u64::MAX).unwrap_err()
        else {
            panic!("E6 Rejected");
        };
        assert!(
            !range.contains(EpochIntegrityViolation::CODE),
            "E6 a caller's epoch is not stored corruption: {range}"
        );
    }

    /// One store reports each skipped untrustworthy row once, within a bound.
    ///
    /// Record capacity 2.
    /// | Rule | skip                                   | error-level events                              |
    /// |------|----------------------------------------|-------------------------------------------------|
    /// | R1   | row a, first time (exhausted)          | one: skipped, integrity code, its violation     |
    /// | R2   | row a, three more scans                | none                                            |
    /// | R3   | row b, first time (negative)           | one of its own, naming the negative cause       |
    /// | R4   | row c, the record full                 | no row event; one saturation event              |
    /// | R5   | row d, and c again                     | none                                            |
    /// | R6   | another store, row a                   | one (the record is per store)                   |
    #[test]
    fn a_skipped_untrustworthy_generation_is_reported_once_per_row_within_a_bound() {
        use awaken_lease_contract::fence::EpochIntegrityViolation;
        let captured = awaken_reliability_testkit::events::CapturedEvents::default();
        let integrity = |captured: &awaken_reliability_testkit::events::CapturedEvents| {
            captured
                .snapshot()
                .into_iter()
                .filter(|event| {
                    event.get("target").map(String::as_str)
                        == Some("awaken::lease_generation_integrity")
                })
                .collect::<Vec<_>>()
        };
        let reported = ReportedClaimGenerations::with_capacity(2);
        tracing::subscriber::with_default(captured.clone(), || {
            reported.observe_skipped("a", ClaimGenerationViolation::Exhausted);
        });
        let events = integrity(&captured);
        assert_eq!(events.len(), 1, "R1 {events:?}");
        assert_eq!(events[0]["run_id"], "a", "R1");
        assert_eq!(events[0]["disposition"], "skipped", "R1");
        assert_eq!(events[0]["code"], EpochIntegrityViolation::CODE, "R1");
        assert_eq!(
            events[0]["violation"],
            EpochIntegrityViolation::Exhausted.to_string(),
            "R1"
        );
        tracing::subscriber::with_default(captured.clone(), || {
            for _ in 0..3 {
                reported.observe_skipped("a", ClaimGenerationViolation::Exhausted);
            }
        });
        assert_eq!(integrity(&captured).len(), 1, "R2");
        tracing::subscriber::with_default(captured.clone(), || {
            reported.observe_skipped("b", ClaimGenerationViolation::Negative);
        });
        let events = integrity(&captured);
        assert_eq!(events.len(), 2, "R3 {events:?}");
        assert_eq!(events[1]["run_id"], "b", "R3");
        assert!(events[1]["violation"].contains("negative"), "R3");
        tracing::subscriber::with_default(captured.clone(), || {
            reported.observe_skipped("c", ClaimGenerationViolation::Exhausted);
        });
        let events = integrity(&captured);
        assert_eq!(events.len(), 3, "R4 {events:?}");
        assert!(!events[2].contains_key("run_id"), "R4 no row event");
        assert_eq!(events[2]["disposition"], "unreported", "R4");
        assert_eq!(events[2]["capacity"], "2", "R4");
        assert_eq!(events[2]["code"], EpochIntegrityViolation::CODE, "R4");
        tracing::subscriber::with_default(captured.clone(), || {
            reported.observe_skipped("d", ClaimGenerationViolation::Negative);
            reported.observe_skipped("c", ClaimGenerationViolation::Exhausted);
        });
        assert_eq!(integrity(&captured).len(), 3, "R5");
        let other = ReportedClaimGenerations::default();
        tracing::subscriber::with_default(captured.clone(), || {
            other.observe_skipped("a", ClaimGenerationViolation::Exhausted);
        });
        assert_eq!(integrity(&captured).len(), 4, "R6");
    }
}
