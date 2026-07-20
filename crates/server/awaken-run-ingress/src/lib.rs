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
mod capability;
mod clock;
mod commit_fence;
mod dispatch;
mod dispatch_schema;
mod durable;
mod fenced_checkpoint;
mod live_control;
pub mod memory;
mod pool;
mod postgres;
mod send_message;
mod service;
mod sqlite;
mod transport_client;
mod wake;
mod worker;
mod worker_context;

pub use any::AnyDispatchStore;
pub use capability::RunIngressCapabilities;
// The database-less worker's HTTP dispatch client (drives claim/settle over the wire
// to a cell server's dispatch_transport_router), extracted from awaken-runtime-host.
pub use awaken_run_ingress_contract::{
    AssignmentRejection, ExecutionScopeRef, LeastLoadedPolicy, PlacementContext, PlacementError,
    PlacementPolicy, PlacementRequirements, RankedWorker, RegisteredWorker, RegistryError,
    RegistryMutation, RunDispatch, WorkerAssignment, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerRecoveryMode, WorkerRegistration, WorkerSnapshot,
    WorkerState, can_assign, can_claim, place_assignment,
};
pub use awaken_runtime_contract::{InferenceAccess, InferenceAccessCandidate};
pub use clock::{Clock, ManualClock, SystemClock};
pub use commit_fence::{ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit};
pub use dispatch::{
    CasOutcome, Claimed, CommitEpochGuard, Dispatch, DispatchCompletion, DispatchError,
    DispatchOutcome, DispatchQueue, DispatchState, DispatchSummary, Inbox, Lease, Outbox,
    PendingInput, PendingRecord, RunClaim, SettleOutcome, SubmitOptions,
};
pub use dispatch_schema::dispatch_bundle;
pub use durable::DurableRunIngress;
pub use fenced_checkpoint::FencedStreamCheckpointStore;
pub use live_control::{Error as LiveRunControlError, LiveRunControlService};
pub use memory::MemoryDispatchStore;
pub use pool::{CompletionSink, DispatchPool, WorkerResolver};
pub use postgres::{
    PostgresDispatchStore, PostgresStreamCheckpointStore, StoreError as PostgresStoreError,
};
pub use send_message::OutboxMessageSender;
pub use service::{DispatchService, DispatchServiceConfig};
pub use sqlite::{SqliteDispatchStore, StoreError as SqliteStoreError};
pub use transport_client::{HttpDispatchQueue, worker_dispatch_store};
#[cfg(feature = "nats")]
pub use wake::NatsWakeSignal;
pub use wake::{LocalWakeSignal, PgNotifyWake, WakeSignal};
pub use worker::{DEFAULT_LEASE_MS, DispatchWorker};
pub use worker_context::InferenceMaterializerFn;

/// A durable-ingress failure: either the dispatch store rejected an operation or
/// a runtime attempt failed. Kept as two arms so a queue-storage failure never
/// masquerades as a run execution failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Dispatch(#[from] dispatch::DispatchError),
    #[error(transparent)]
    Execution(#[from] awaken_runtime_contract::execution::Error),
}

/// Shared policy adapter used by every durable backend. Eligibility and
/// replacement authority remain in the worker-contract kernel; stores only use
/// the boolean result while holding their backend-specific claim lock.
pub(crate) struct DispatchPlacement<'a> {
    pub recovered: bool,
    pub previous: Option<&'a WorkerAssignment>,
    pub sandbox_bound: bool,
    pub requester: &'a WorkerIdentity,
    pub workers: &'a [WorkerSnapshot],
    pub now_ms: u64,
}

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
        attributes: Default::default(),
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
