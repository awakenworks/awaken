// @formal entrypoint store_history.receipt_refines_one_exact_durable_record
//! Claim-fenced committed-truth application port.

use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::CommitReceipt;
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use crate::ClaimedCommitCommand;
use awaken_worker_contract::RunClaim;

/// Atomically apply a [`ThreadCommit`] under one durable [`RunClaim`].
#[async_trait::async_trait]
pub trait ClaimedRunCommit<OperationError = CommitError>: Send + Sync {
    /// Compatibility for an explicitly embedded Worker with a plain SDK store.
    async fn commit(
        &self,
        _claim: &RunClaim,
        _commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        Err(CommitError::Rejected(
            "Worker commits require a versioned CommitOperation".to_owned(),
        ))
    }

    /// Return the original atomically persisted operation receipt. Every
    /// implementation validates the exact current claim and its Dispatch
    /// binding in the same consistency boundary as publishing the result.
    /// The commit, version and receipt become visible together. An exact
    /// retry under a valid claim returns the original receipt.
    ///
    /// Coordinator ingress can preserve classified failures with its
    /// operation error type while execution adapters retain the SDK error.
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, OperationError>;
}
