//! Claim-fenced committed-truth application port.

use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::{
    CommitReceipt, applied_commit_receipt, next_commit_thread_version,
};
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
        let thread_version = next_commit_thread_version(operation.expected_thread_version)
            .map_err(|error| CommitError::Rejected(error.to_string()))?;
        let record = self.commit(&command.claim, operation.commit).await?;
        Ok(applied_commit_receipt(
            operation.operation_id,
            operation.payload_hash,
            record.sequence,
            thread_version,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::operation::{
        CommitOperation, CommitOperationId, CommitPayloadHash,
    };
    use awaken_agent_contract::thread::commit::staged::RunDisposition;

    use super::*;

    #[derive(Default)]
    struct RecordingCommit {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ClaimedRunCommit for RecordingCommit {
        async fn commit(
            &self,
            _claim: &RunClaim,
            _commit: ThreadCommit,
        ) -> Result<CommitRecord, CommitError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CommitRecord { sequence: 7 })
        }
    }

    fn command(expected_thread_version: u64) -> ClaimedCommitCommand {
        let run_id = RunId("run".into());
        ClaimedCommitCommand {
            claim: RunClaim {
                run_id: run_id.clone(),
                owner: "worker".into(),
                epoch: 3,
            },
            operation: CommitOperation {
                operation_id: CommitOperationId::new(run_id.clone(), 2),
                expected_thread_version,
                payload_hash: CommitPayloadHash("hash".into()),
                commit: ThreadCommit {
                    thread_id: ThreadId("thread".into()),
                    run: RunDisposition::running(run_id),
                    messages: Vec::new(),
                    state: Vec::new(),
                    events: Vec::new(),
                },
            },
        }
    }

    #[tokio::test]
    async fn default_commit_operation_rejects_overflow_before_durable_commit() {
        // Cause/effect graph: C1 expected Thread version has a successor; C2
        // the underlying commit succeeds. Effects: E1 C1+C2 calls the durable
        // commit once and returns its exact sequence with the successor; E2
        // !C1 rejects before any durable call. Decision rules R1=C1+C2=>E1 and
        // R2=!C1=>E2. A commit failure is propagated by the trait and is outside
        // this receipt-mapping test.
        let accepted = RecordingCommit::default();
        let receipt = accepted.commit_operation(command(4)).await.expect("R1/E1");
        assert_eq!(accepted.calls.load(Ordering::SeqCst), 1, "R1/E1");
        assert_eq!(receipt.commit_sequence, 7, "R1/E1");
        assert_eq!(receipt.thread_version, 5, "R1/E1");

        let rejected = RecordingCommit::default();
        assert!(
            rejected.commit_operation(command(u64::MAX)).await.is_err(),
            "R2/E2"
        );
        assert_eq!(rejected.calls.load(Ordering::SeqCst), 0, "R2/E2");
    }
}
