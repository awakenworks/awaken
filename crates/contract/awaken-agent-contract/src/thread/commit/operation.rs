//! Stable logical identities and durable receipts for retryable Thread commits.
//!
//! A dispatch claim authorizes one delivery attempt; it is deliberately absent
//! from these types. The operation id survives HTTP response loss and Worker
//! reclaim, while the expected Thread version prevents an attempt built from a
//! stale recovery prefix from appending new truth.

use crate::agent::run::Id as RunId;
use crate::thread::commit::staged::{CommitRecord, ThreadCommit};
use serde::{Deserialize, Serialize};

const CANONICAL_COMMIT_VERSION: &str = "awaken.thread-commit.v1";

#[derive(Debug, thiserror::Error)]
pub enum CommitHashError {
    #[error("serialize ThreadCommit for canonical hashing: {0}")]
    Serialize(String),
}

/// Hash one retryable commit with the contract-owned canonical encoding.
///
/// Every producer of [`CommitOperation`] uses this function so a durable
/// receipt cannot depend on adapter JSON field order or a second hash format.
pub fn commit_payload_hash(commit: &ThreadCommit) -> Result<CommitPayloadHash, CommitHashError> {
    crate::canonical_json_sha256(CANONICAL_COMMIT_VERSION, commit)
        .map(CommitPayloadHash)
        .map_err(|error| CommitHashError::Serialize(error.to_string()))
}

/// Stable identity of one logical commit within a Run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommitOperationId {
    pub run_id: RunId,
    pub ordinal: u64,
}

impl CommitOperationId {
    #[must_use]
    pub fn new(run_id: RunId, ordinal: u64) -> Self {
        Self { run_id, ordinal }
    }
}

/// SHA-256 of the versioned canonical encoding of a [`ThreadCommit`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitPayloadHash(pub String);

/// One retryable optimistic commit submitted to the authoritative coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitOperation {
    pub operation_id: CommitOperationId,
    pub expected_thread_version: u64,
    pub payload_hash: CommitPayloadHash,
    pub commit: ThreadCommit,
}

/// Durable acknowledgement of an applied logical commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub operation_id: CommitOperationId,
    pub commit_sequence: u64,
    pub thread_version: u64,
    pub payload_hash: CommitPayloadHash,
    /// `true` only on a replay response; the durable row stores the original
    /// receipt and remains unchanged.
    pub duplicate: bool,
}

impl CommitReceipt {
    #[must_use]
    pub fn commit_record(&self) -> CommitRecord {
        CommitRecord {
            sequence: self.commit_sequence,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::run::Id as RunId;
    use crate::agent::thread::Id as ThreadId;
    use crate::audit::draft::Draft;
    use crate::audit::kind::Kind;
    use crate::thread::commit::staged::RunDisposition;

    use super::*;

    fn commit(payload: serde_json::Value) -> ThreadCommit {
        ThreadCommit {
            thread_id: ThreadId("thread".into()),
            run: RunDisposition::running(RunId("run".into())),
            messages: Vec::new(),
            state: Vec::new(),
            events: vec![Draft {
                kind: Kind::RunStateChanged,
                payload,
            }],
        }
    }

    #[test]
    fn canonical_commit_hash_ignores_object_field_order_but_not_payload_changes() {
        // Cause/effect graph: C1 JSON object insertion order changes; C2 a
        // semantic payload value changes. Effects: E1 C1 retains one durable
        // receipt identity; E2 C2 changes it. Decision rules H1=C1=>E1 and
        // H2=C2=>E2; every CommitOperation producer shares this owner.
        // Constraints/invariants: canonicalization ignores representation order
        // only; it never erases a semantic payload difference.
        let left = commit(serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap());
        let reordered = commit(serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap());
        let changed = commit(serde_json::json!({"a": 3, "b": 2}));
        assert_eq!(
            commit_payload_hash(&left).unwrap(),
            commit_payload_hash(&reordered).unwrap(),
            "H1/E1"
        );
        assert_ne!(
            commit_payload_hash(&left).unwrap(),
            commit_payload_hash(&changed).unwrap(),
            "H2/E2"
        );
    }
}
