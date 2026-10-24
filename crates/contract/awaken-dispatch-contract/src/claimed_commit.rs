// @formal entrypoint store_history.receipt_refines_one_exact_durable_record
//! Claim-fenced committed-truth application port.

use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::CommitReceipt;
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

use crate::{ClaimedCommitCommand, RunClaim};

/// Atomically apply a [`ThreadCommit`] under one durable [`RunClaim`].
#[async_trait::async_trait]
pub trait ClaimedRunCommit: Send + Sync {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError>;

    /// Return the original atomically persisted operation receipt. Every
    /// implementation must provide this authority explicitly; an ordinary
    /// commit cannot establish operation replay identity or Thread CAS.
    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError>;
}
