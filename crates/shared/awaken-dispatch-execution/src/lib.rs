//! Protocol-neutral, claim-bound dispatch execution wiring.
//!
//! This shared owner carries the one Worker/Pool execution path, its claim-bound
//! attempt context, and process-local capacity. Durable queue/storage and
//! transport factories remain with their Coordinator and Worker owners.

pub mod claimed_stream;
mod commit_fence;
mod control_dispatch;
mod error;
pub mod execution_capacity;
mod fenced_checkpoint;
mod live_control;
pub mod pool;
mod reconciliation;
mod recovery_projection;
mod service;
pub mod wake;
mod worker;
pub mod worker_context;
mod worker_evidence;

// Existing execution modules retain their contract vocabulary while the
// Coordinator adapter depends inward on this one implementation.
mod dispatch {
    pub use awaken_dispatch_contract::dispatch::*;
}

pub use awaken_dispatch_contract::{
    ClaimedStreamPublisher, DispatchError, DispatchQueue, DispatchSettlementObserver,
    DispatchState, DispatchSummary, RunClaim, RunDispatch, SessionRunAdmissionObserver,
    SettleOutcome,
};
pub use commit_fence::{
    ClaimedCommitCoordinator, ClaimedRunCommit, GuardedRunCommit, validate_commit_dispatch_binding,
};
pub use control_dispatch::ControlDispatch;
pub use error::Error;
pub use fenced_checkpoint::FencedStreamCheckpointStore;
pub use live_control::{Error as LiveRunControlError, LiveRunControlService};
pub use pool::{CompletionSink, DispatchPool, DispatchReconciler, PoolDrainFence, WorkerResolver};
pub use reconciliation::TerminalReconciliationScan;
pub use recovery_projection::{RecoveryProjection, RecoveryProjectionError};
pub use service::{DispatchService, DispatchServiceConfig};
pub use wake::{LocalWakeSignal, WakeSignal, publish_dispatch_hint};
pub use worker::{DEFAULT_LEASE_MS, DispatchWorker, claim_bound_ownership_verifier};
pub use worker_context::{ClaimedModelRequestGateFactory, InferenceMaterializerFn};
pub use worker_evidence::WorkerEvidenceRelease;
