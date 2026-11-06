// @formal surface obligation=store_history.recovery_preserves_committed_time_and_receipt_identity
// @formal surface obligation=store_history.exact_retry_and_every_rejection_are_atomic_stutters
// @formal reason: This consumed construction role forwards existing records and
// @formal reason: times to the one original state/receipt/append kernel; it creates
// @formal reason: no parallel store, clock sampler, or live replay capability.
//! Construction-only restoration of the existing reference commit state.

use std::sync::Arc;

use awaken_agent_contract::thread::commit::coordinator::Error;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use super::{MemoryCommitCoordinator, OperationOrigin};

/// Restore existing records into one new, empty reference state before it is
/// exposed as a live Coordinator. The durable owner must supply the original
/// records and optional times; this role does not certify historical bodies.
/// It cannot bind an existing Coordinator, clone the restoration authority or
/// expose read/write ports while construction is in progress.
///
/// C1 construction has finished -> E1 restoration authority is consumed; there
/// is no transition from the returned live owner back into this role.
/// ```compile_fail
/// let replay = awaken_store_inmem::MemoryCommitReplay::new(awaken_time::system_clock());
/// let coordinator = replay.finish();
/// drop(replay); // C1/E1: the construction role has been consumed.
/// ```
/// C2 an existing Coordinator is offered as the initial state -> E2 its private
/// storage cannot be installed in a new replay role.
/// ```compile_fail
/// let coordinator = awaken_store_inmem::MemoryCommitCoordinator::new(awaken_time::system_clock());
/// let replay = awaken_store_inmem::MemoryCommitReplay { inner: coordinator };
/// ```
/// C3 an attempted clone would retain restoration authority after finish -> E3
/// no Clone implementation exists.
/// ```compile_fail
/// let replay = awaken_store_inmem::MemoryCommitReplay::new(awaken_time::system_clock());
/// let duplicate = replay.clone();
/// ```
#[must_use]
pub struct MemoryCommitReplay {
    inner: MemoryCommitCoordinator,
}

impl MemoryCommitReplay {
    pub fn new(clock: Arc<dyn awaken_time::WallClock>) -> Self {
        Self {
            inner: MemoryCommitCoordinator::new(clock),
        }
    }

    /// Restore one ordinary entry with its unchanged original optional time.
    pub async fn replay_commit_at(
        &mut self,
        commit: ThreadCommit,
        committed_at_unix_ms: Option<u64>,
    ) -> Result<CommitRecord, Error> {
        self.inner.commit_at(commit, committed_at_unix_ms).await
    }

    /// Restore one existing operation and declared hash through the same private
    /// state/receipt/CAS/append kernel as the live owner. No clock is sampled.
    pub async fn replay_operation_at(
        &mut self,
        operation: CommitOperation,
        committed_at_unix_ms: Option<u64>,
    ) -> Result<CommitReceipt, Error> {
        self.inner
            .apply_operation_at(
                operation,
                committed_at_unix_ms,
                OperationOrigin::StoredRecord,
            )
            .await
    }

    /// Publish the same reconstructed owner and consume the only replay role.
    #[must_use]
    pub fn finish(self) -> MemoryCommitCoordinator {
        self.inner
    }
}
