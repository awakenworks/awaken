//! Stable logical identities and durable receipts for retryable Thread commits.
//!
//! A dispatch claim authorizes one delivery attempt; it is deliberately absent
//! from these types. The operation id survives HTTP response loss and Worker
//! reclaim, while the expected Thread version prevents an attempt built from a
//! stale recovery prefix from appending new truth.

use serde::{Deserialize, Serialize};

use crate::agent::run::Id as RunId;
use crate::thread::commit::staged::{CommitRecord, ThreadCommit};

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
