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

/// Contract-owned rejection reasons for the logical operation protocol.
///
/// Storage adapters own locking and atomic persistence, but all adapters use
/// these exact identity, replay, and compare-and-swap decisions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitOperationAdmissionError {
    #[error("commit operation run_id does not match ThreadCommit run_id")]
    RunIdentityMismatch,
    #[error("stored commit receipt identity does not match operation identity")]
    ReceiptIdentityMismatch,
    #[error("commit operation {run_id}:{ordinal} was reused with another payload")]
    PayloadMismatch { run_id: String, ordinal: u64 },
    #[error("thread version conflict: expected {expected}, current {current}")]
    ThreadVersionConflict { expected: u64, current: u64 },
    #[error("commit operation ordinal conflict: expected {expected}, current {current}")]
    OrdinalConflict { expected: u64, current: u64 },
    #[error("Thread version overflow")]
    ThreadVersionOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationIdentityDisposition {
    New,
    Replay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationIdentityError {
    Run,
    Receipt,
    Payload,
}

fn classify_operation_identity(
    run_identity_matches: bool,
    existing_receipt_matches: Option<(bool, bool)>,
) -> Result<OperationIdentityDisposition, OperationIdentityError> {
    if !run_identity_matches {
        return Err(OperationIdentityError::Run);
    }
    match existing_receipt_matches {
        Some((false, _)) => Err(OperationIdentityError::Receipt),
        Some((true, false)) => Err(OperationIdentityError::Payload),
        Some((true, true)) => Ok(OperationIdentityDisposition::Replay),
        None => Ok(OperationIdentityDisposition::New),
    }
}

/// Reject an operation whose durable identity names a different Run than its
/// commit payload.
pub fn require_commit_operation_identity(
    operation: &CommitOperation,
) -> Result<(), CommitOperationAdmissionError> {
    classify_operation_identity(
        operation.operation_id.run_id == *operation.commit.run_id(),
        None,
    )
    .map(|_| ())
    .map_err(|error| match error {
        OperationIdentityError::Run => CommitOperationAdmissionError::RunIdentityMismatch,
        OperationIdentityError::Receipt => CommitOperationAdmissionError::ReceiptIdentityMismatch,
        OperationIdentityError::Payload => {
            unreachable!("no receipt was supplied for identity validation")
        }
    })
}

/// Require a durable receipt to name the submitted operation and payload.
pub fn require_commit_operation_receipt(
    operation: &CommitOperation,
    receipt: &CommitReceipt,
) -> Result<(), CommitOperationAdmissionError> {
    classify_operation_identity(
        operation.operation_id.run_id == *operation.commit.run_id(),
        Some((
            receipt.operation_id == operation.operation_id,
            receipt.payload_hash == operation.payload_hash,
        )),
    )
    .map(|_| ())
    .map_err(|error| match error {
        OperationIdentityError::Run => CommitOperationAdmissionError::RunIdentityMismatch,
        OperationIdentityError::Receipt => CommitOperationAdmissionError::ReceiptIdentityMismatch,
        OperationIdentityError::Payload => CommitOperationAdmissionError::PayloadMismatch {
            run_id: operation.operation_id.run_id.0.clone(),
            ordinal: operation.operation_id.ordinal,
        },
    })
}

/// Return the exact durable receipt on replay, or `None` for a new operation.
pub fn replay_commit_operation_receipt(
    operation: &CommitOperation,
    existing: Option<&CommitReceipt>,
) -> Result<Option<CommitReceipt>, CommitOperationAdmissionError> {
    require_commit_operation_identity(operation)?;
    match existing {
        Some(receipt) => {
            require_commit_operation_receipt(operation, receipt)?;
            let mut duplicate = receipt.clone();
            duplicate.duplicate = true;
            Ok(Some(duplicate))
        }
        None => Ok(None),
    }
}

/// Admit a new operation only at the exact committed Thread and Run prefix.
/// The returned version is the only version an adapter may persist.
pub fn admit_commit_operation_append(
    operation: &CommitOperation,
    current_thread_version: u64,
    current_run_ordinal: u64,
) -> Result<u64, CommitOperationAdmissionError> {
    if current_thread_version != operation.expected_thread_version {
        return Err(CommitOperationAdmissionError::ThreadVersionConflict {
            expected: operation.expected_thread_version,
            current: current_thread_version,
        });
    }
    if current_run_ordinal != operation.operation_id.ordinal {
        return Err(CommitOperationAdmissionError::OrdinalConflict {
            expected: operation.operation_id.ordinal,
            current: current_run_ordinal,
        });
    }
    current_thread_version
        .checked_add(1)
        .ok_or(CommitOperationAdmissionError::ThreadVersionOverflow)
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

    #[test]
    fn operation_admission_covers_replay_conflict_and_fresh_append_rules() {
        // Cause/effect graph: C1 Run identity matches; C2 a receipt exists; C3
        // its operation identity matches; C4 its payload hash matches; C5
        // Thread version matches; C6 Run ordinal matches; C7 version can
        // increment. Effects: E1 exact replay returns
        // the old receipt marked duplicate; E2 conflicting identity/hash is
        // rejected; E3 a fresh exact prefix yields version+1; E4 stale, gapped,
        // or overflowing prefixes are rejected. Decision rules:
        // A1=C1+C2+C3+C4=>E1; A2=!C1 or C2+(!C3 or !C4)=>E2;
        // A3=C1+!C2+C5+C6+C7=>E3; A4=otherwise=>E4. Storage adapters share
        // these decisions and own only their transaction and append mechanics.
        let operation = CommitOperation {
            operation_id: CommitOperationId::new(RunId("run".into()), 0),
            expected_thread_version: 0,
            payload_hash: CommitPayloadHash("hash".into()),
            commit: commit(serde_json::json!({"value": 1})),
        };
        let receipt = CommitReceipt {
            operation_id: operation.operation_id.clone(),
            commit_sequence: 7,
            thread_version: 1,
            payload_hash: operation.payload_hash.clone(),
            duplicate: false,
        };
        assert!(require_commit_operation_identity(&operation).is_ok());
        let replay = replay_commit_operation_receipt(&operation, Some(&receipt)).unwrap();
        assert!(replay.is_some_and(|receipt| receipt.duplicate), "A1/E1");
        assert_eq!(
            admit_commit_operation_append(&operation, 0, 0),
            Ok(1),
            "A3/E3"
        );

        let mut wrong_run = operation.clone();
        wrong_run.operation_id.run_id = RunId("other".into());
        assert!(
            require_commit_operation_identity(&wrong_run).is_err(),
            "A2/E2"
        );
        let mut wrong_receipt = receipt.clone();
        wrong_receipt.operation_id.ordinal = 1;
        assert!(
            replay_commit_operation_receipt(&operation, Some(&wrong_receipt)).is_err(),
            "A2/E2"
        );
        let mut wrong_hash = receipt;
        wrong_hash.payload_hash = CommitPayloadHash("other".into());
        assert!(
            replay_commit_operation_receipt(&operation, Some(&wrong_hash)).is_err(),
            "A2/E2"
        );
        assert!(
            admit_commit_operation_append(&operation, 1, 0).is_err(),
            "A4/E4 stale Thread"
        );
        assert!(
            admit_commit_operation_append(&operation, 0, 1).is_err(),
            "A4/E4 ordinal gap"
        );
        let mut overflow = operation;
        overflow.expected_thread_version = u64::MAX;
        overflow.operation_id.ordinal = u64::MAX;
        assert!(
            admit_commit_operation_append(&overflow, u64::MAX, u64::MAX).is_err(),
            "A4/E4 overflow"
        );
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn operation_identity_replays_only_the_same_run_and_payload() {
        let run_identity_matches: bool = kani::any();
        let receipt_tag: u8 = kani::any();
        let existing_receipt_matches = match receipt_tag % 4 {
            0 => None,
            1 => Some((false, kani::any())),
            2 => Some((true, false)),
            _ => Some((true, true)),
        };
        let result = classify_operation_identity(run_identity_matches, existing_receipt_matches);
        assert_eq!(
            result == Ok(OperationIdentityDisposition::Replay),
            run_identity_matches && existing_receipt_matches == Some((true, true))
        );
        assert_eq!(
            result == Ok(OperationIdentityDisposition::New),
            run_identity_matches && existing_receipt_matches.is_none()
        );
    }

    #[kani::proof]
    fn operation_append_requires_exact_prefix_and_nonwrapping_version() {
        let expected_version: u64 = kani::any();
        let current_version: u64 = kani::any();
        let expected_ordinal: u64 = kani::any();
        let current_ordinal: u64 = kani::any();
        let operation = CommitOperation {
            operation_id: CommitOperationId::new(RunId("run".into()), expected_ordinal),
            expected_thread_version: expected_version,
            payload_hash: CommitPayloadHash(String::new()),
            commit: ThreadCommit {
                thread_id: crate::agent::thread::Id("thread".into()),
                run: crate::thread::commit::staged::RunDisposition::running(RunId("run".into())),
                messages: Vec::new(),
                state: Vec::new(),
                events: Vec::new(),
            },
        };
        let result = admit_commit_operation_append(&operation, current_version, current_ordinal);
        let expected = current_version == expected_version
            && current_ordinal == expected_ordinal
            && current_version < u64::MAX;
        assert_eq!(result.is_ok(), expected);
        if let Ok(next) = result {
            assert_eq!(Some(next), current_version.checked_add(1));
        }
    }
}
