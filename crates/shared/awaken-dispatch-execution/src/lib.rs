//! Protocol-neutral, claim-bound dispatch execution wiring.
//!
//! This shared owner carries the one Worker/Pool execution path, its claim-bound
//! attempt context, and process-local capacity. Durable queue/storage and
//! transport factories remain with their Coordinator and Worker owners.

mod admission_error;
mod attempt_termination;
pub mod claimed_stream;
mod commit_fence;
#[cfg(test)]
mod composition_tests;
mod control_dispatch;
mod coordination_activation;
mod error;
pub mod execution_capacity;
mod execution_topology;
mod fenced_checkpoint;
mod live_control;
pub mod pool;
mod process_dispatch;
mod reconciliation;
mod recovery_projection;
mod service;
mod session_admission;
pub mod wake;
mod worker;
pub mod worker_context;
mod worker_evidence;

// Existing execution modules retain their contract vocabulary while the
// Coordinator adapter depends inward on this one implementation.
mod dispatch {
    pub use awaken_dispatch_contract::dispatch::*;
}

pub use admission_error::AdmissionError;
pub use attempt_termination::{
    NativeAttemptFootprint, NativeAttemptTermination, NativeAttemptTerminationError,
    classify_native_attempt_termination,
};
pub use awaken_dispatch_contract::{
    ClaimedStreamPublisher, DispatchError, DispatchSettlementObserver, DispatchState,
    DispatchSummary, RunClaim, RunDispatch, SettleOutcome,
};
pub use commit_fence::{
    ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit, validate_commit_dispatch_binding,
};
pub use control_dispatch::{ControlDispatch, LocalWorkerDispatch, TerminalRecoveryDispatch};
pub use coordination_activation::{coordinated_activation_matches, coordinated_run_activation};
pub use error::{Error, NotReady};
pub use execution_topology::{
    DispatchPoolSizing, ExecutionTopology, LeaseOwnerId, LocalExecution, QueueDepthSource,
    RunIngress,
};
pub use fenced_checkpoint::FencedStreamCheckpointStore;
pub use live_control::{Error as LiveRunControlError, LiveRunControlService};
pub use pool::{CompletionSink, DispatchPool, DispatchReconciler, PoolDrainFence, WorkerResolver};
pub use process_dispatch::{ClaimedRunsDrained, ProcessDispatch};
pub use reconciliation::TerminalReconciliationScan;
pub use recovery_projection::{RecoveryProjection, RecoveryProjectionError};
pub use service::{DispatchService, DispatchServiceConfig};
pub use session_admission::SessionRunAdmissionObserver;
pub use wake::{DispatchHint, LocalWakeSignal, publish_dispatch_hint};
pub use worker::{DEFAULT_LEASE_MS, DispatchWorker, claim_bound_ownership_verifier};
pub use worker_context::{ClaimedModelRequestGateFactory, InferenceMaterializerFn};
pub use worker_evidence::{
    PhysicalAttemptReads, PhysicalAttemptRecoveryRead, WorkerEvidenceRelease,
    WorkerTerminalReconciliation, committed_terminal_ready,
};

pub use awaken_dispatch_erasure::{
    AnyCoordinatorDispatch, AnyDispatchStore, Dispatch, DispatchBinding, DispatchTopology,
    DurableDispatch, RemoteDispatch,
};

mod execution_recovery;
pub use execution_recovery::{
    exact_expired_slot_remains, read_execution_recovery, read_expired_physical_attempt,
    read_source_dispatch_observation,
};

pub mod model_capability;
pub mod placement;
pub mod session_envelope;
pub mod session_execution;
pub mod session_reconciliation;
pub mod session_worker_resolver;

pub mod session_dispatch;

pub mod durable_control;
