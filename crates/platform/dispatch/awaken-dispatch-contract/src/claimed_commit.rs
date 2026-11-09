// @formal entrypoint store_history.receipt_refines_one_exact_durable_record
//! Claim-fenced committed-truth application port.

use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::CommitReceipt;

use crate::ClaimedCommitCommand;

pub const COMMIT_FENCED: &str = "commit_fenced";
pub const COMMIT_CONFLICT: &str = "commit_conflict";
pub const COMMIT_IDENTITY_MISMATCH: &str = "commit_identity_mismatch";
pub const COMMIT_OUTCOME_UNKNOWN: &str = "commit_outcome_unknown";

/// Atomically publish a versioned Thread commit under a durable Run claim.
#[async_trait::async_trait]
pub trait ClaimedRunCommit<OperationError = CommitError>: Send + Sync {
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
