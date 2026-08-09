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

    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        let operation = command.operation;
        let thread_version = operation
            .expected_thread_version
            .checked_add(1)
            .ok_or_else(|| CommitError::Rejected("Thread version overflow".to_string()))?;
        let record = self.commit(&command.claim, operation.commit).await?;
        Ok(CommitReceipt {
            operation_id: operation.operation_id,
            commit_sequence: record.sequence,
            thread_version,
            payload_hash: operation.payload_hash,
            duplicate: false,
        })
    }
}
