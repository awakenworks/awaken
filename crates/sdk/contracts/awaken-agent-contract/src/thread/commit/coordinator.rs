// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint run.commit_shape_atomicity
use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("commit rejected: {0}")]
    Rejected(String),
    /// The durable store has no room for the commit (disk or quota full).
    /// Nothing was applied; unlike `Rejected`, the same commit may succeed
    /// once capacity is restored.
    #[error("commit storage capacity exhausted: {0}")]
    CapacityExhausted(String),
}

/// The single durable write boundary (G1/G13). Async because a real store
/// commits over IO; `Send + Sync` so it can be shared as `Arc<dyn Coordinator>`
/// through `RuntimeRunContext`.
#[async_trait]
pub trait Coordinator: Send + Sync {
    async fn commit(
        &self,
        commit: crate::thread::commit::staged::ThreadCommit,
    ) -> Result<crate::thread::commit::staged::CommitRecord, Error>;
}

/// Durable idempotent/CAS extension used by recoverable remote Workers.
///
/// Implementations must persist the commit and its receipt atomically, retaining
/// the original committed Thread alongside the operation identity and declared
/// payload hash. A duplicate can succeed only for that same Thread and returns
/// the original receipt without appending facts or advancing the Thread version.
/// This persisted receipt binding precedes new-operation CAS; an absent receipt
/// continues through the existing append admission. Receipt replay preserves
/// declared-hash semantics; body validation and authorization stay at their
/// existing boundaries.
#[async_trait]
pub trait OperationCoordinator: Coordinator {
    async fn commit_operation(
        &self,
        operation: crate::thread::commit::operation::CommitOperation,
    ) -> Result<crate::thread::commit::operation::CommitReceipt, Error>;
}
