// @formal entrypoint outcome.stable_run_identity_and_exactly_once_ack
// @formal entrypoint remote_worker.acknowledged_projection_is_monotonic
// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint remote_worker.operation_identity_maps_to_one_hash_and_append
// @formal entrypoint store_history.concurrent_compare_and_swap_has_one_winner_per_thread_version
// @formal entrypoint store_history.exact_retry_and_every_rejection_are_atomic_stutters
// @formal entrypoint store_history.receipt_refines_one_exact_durable_record
// @formal entrypoint store_history.recovery_preserves_committed_time_and_receipt_identity
//! Stable logical identities and durable receipts for retryable Thread commits.
//!
//! A dispatch claim authorizes one delivery attempt; it is deliberately absent
//! from these types. The operation id survives HTTP response loss and Worker
//! reclaim, while the expected Thread version prevents an attempt built from a
//! stale recovery prefix from appending new truth.

use crate::agent::run::Id as RunId;
use crate::agent::thread::Id as ThreadId;
use crate::thread::commit::staged::{CommitRecord, ThreadCommit};
use serde::{Deserialize, Serialize};

const CANONICAL_COMMIT_VERSION: &str = "awaken.thread-commit.v1";

#[derive(Debug, thiserror::Error)]
pub enum CommitHashError {
    #[error("serialize ThreadCommit for canonical hashing: {0}")]
    Serialize(String),
    #[error("commit operation payload hash does not match ThreadCommit")]
    PayloadMismatch,
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

/// Validate a new operation with the same canonical body encoding its producer uses.
///
/// Storage calls this after exact receipt replay and before new append admission.
/// Existing immutable receipts retain their original declared-hash semantics;
/// this check does not certify a historical receipt or grant execution authority.
pub fn require_commit_operation_payload_hash(
    operation: &CommitOperation,
) -> Result<(), CommitHashError> {
    if commit_payload_hash(&operation.commit)? != operation.payload_hash {
        return Err(CommitHashError::PayloadMismatch);
    }
    Ok(())
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
///
/// The stored Thread is the owner persisted with that exact receipt, not a
/// current Run projection or caller-supplied partition. Existing Run, operation
/// identity and declared-hash errors retain precedence over Thread mismatch.
/// An absent receipt requires no stored Thread and leaves append/CAS admission
/// to the existing new-operation boundary.
pub fn replay_commit_operation_receipt(
    operation: &CommitOperation,
    existing: Option<(&ThreadId, &CommitReceipt)>,
) -> Result<Option<CommitReceipt>, CommitOperationAdmissionError> {
    require_commit_operation_identity(operation)?;
    match existing {
        Some((thread_id, receipt)) => {
            require_commit_operation_receipt(operation, receipt)?;
            if thread_id != &operation.commit.thread_id {
                return Err(CommitOperationAdmissionError::ReceiptIdentityMismatch);
            }
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
    next_commit_thread_version(current_thread_version)
}

/// Return the only admissible successor of a committed Thread version.
pub fn next_commit_thread_version(
    current_thread_version: u64,
) -> Result<u64, CommitOperationAdmissionError> {
    current_thread_version
        .checked_add(1)
        .ok_or(CommitOperationAdmissionError::ThreadVersionOverflow)
}

/// Construct the only successful receipt shape from the values an adapter
/// persisted for the exact operation. No backend owns another receipt mapping.
#[must_use]
pub fn applied_commit_receipt(
    operation_id: CommitOperationId,
    payload_hash: CommitPayloadHash,
    commit_sequence: u64,
    thread_version: u64,
) -> CommitReceipt {
    CommitReceipt {
        operation_id,
        commit_sequence,
        thread_version,
        payload_hash,
        duplicate: false,
    }
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
        // H3 the real declared hash names this body (including reordered keys)
        // => accept; H4 it names a different real body => typed refusal. The
        // body comparison uses the production canonical encoder, not a free
        // hash predicate. Operation id/version remain outside the body encoding.
        let mut operation = CommitOperation {
            operation_id: CommitOperationId::new(RunId("run".into()), 0),
            expected_thread_version: u64::MAX,
            payload_hash: commit_payload_hash(&left).unwrap(),
            commit: reordered,
        };
        require_commit_operation_payload_hash(&operation).expect("H3 canonical body");
        operation.commit = changed;
        assert!(
            matches!(
                require_commit_operation_payload_hash(&operation),
                Err(CommitHashError::PayloadMismatch)
            ),
            "H4 another body's hash cannot certify this body"
        );
    }

    #[test]
    fn operation_admission_covers_replay_conflict_and_fresh_append_rules() {
        // Cause/effect graph: C1 Run identity matches; C2 a receipt exists; C3
        // its operation identity matches; C4 its payload hash matches; C5
        // Thread version matches; C6 Run ordinal matches; C7 version can
        // increment; C8 the receipt's original Thread matches the payload.
        // Effects: E1 exact replay returns
        // the old receipt marked duplicate; E2 conflicting identity/hash is
        // rejected; E3 a fresh exact prefix yields version+1; E4 stale, gapped,
        // or overflowing prefixes are rejected. Decision rules:
        // A1=C1+C2+C3+C4+C8=>E1; A2=!C1 or C2+(!C3 or !C4 or !C8)=>E2;
        // A3=C1+!C2+C5+C6+C7=>E3; A4=otherwise=>E4. For a replay, original
        // Run/operation-id/hash errors precede Thread mismatch; expected CAS is
        // irrelevant after an exact receipt exists. With no receipt, C8 is not
        // an input and the replay boundary returns None even for a different
        // Thread or stale prefix, leaving the original CAS boundary to decide.
        // Storage adapters share these decisions and own only transaction and
        // append mechanics; these tests do not introduce body-hash validation.
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
        let stored_thread = operation.commit.thread_id.clone();
        assert!(require_commit_operation_identity(&operation).is_ok());
        let replay =
            replay_commit_operation_receipt(&operation, Some((&stored_thread, &receipt))).unwrap();
        assert!(replay.is_some_and(|receipt| receipt.duplicate), "A1/E1");
        let mut foreign_thread = operation.clone();
        foreign_thread.commit.thread_id = ThreadId("foreign-thread".into());
        assert_eq!(
            replay_commit_operation_receipt(&foreign_thread, Some((&stored_thread, &receipt)),),
            Err(CommitOperationAdmissionError::ReceiptIdentityMismatch),
            "A2/E2 same Run/id/hash cannot borrow another Thread's receipt",
        );
        foreign_thread.expected_thread_version = u64::MAX;
        assert_eq!(
            replay_commit_operation_receipt(&foreign_thread, None),
            Ok(None),
            "A3/A4 no receipt means no stored Thread or replay-side CAS",
        );
        let mut exact_stale_prefix = operation.clone();
        exact_stale_prefix.expected_thread_version = u64::MAX;
        let exact =
            replay_commit_operation_receipt(&exact_stale_prefix, Some((&stored_thread, &receipt)))
                .unwrap()
                .unwrap();
        assert_eq!(
            exact.commit_sequence, receipt.commit_sequence,
            "A1 exact replay precedes CAS"
        );
        assert_eq!(
            exact.thread_version, receipt.thread_version,
            "A1 original version"
        );
        assert_eq!(
            exact.payload_hash, receipt.payload_hash,
            "A1 original declared hash"
        );
        let mut run_conflict = foreign_thread.clone();
        run_conflict.operation_id.run_id = RunId("other-run".into());
        assert_eq!(
            replay_commit_operation_receipt(&run_conflict, Some((&stored_thread, &receipt)),),
            Err(CommitOperationAdmissionError::RunIdentityMismatch),
            "A2 original Run error precedes receipt/Thread mismatch",
        );
        assert_eq!(
            replay_commit_operation_receipt(&run_conflict, None),
            Err(CommitOperationAdmissionError::RunIdentityMismatch),
            "A2 absence does not bypass original Run identity",
        );
        // A2/K1 also requires Run rejection when every stored receipt field
        // matches. An exact-looking receipt cannot bypass intrinsic Run identity.
        let mut exact_receipt_bad_run = run_conflict.clone();
        exact_receipt_bad_run.commit.thread_id = stored_thread.clone();
        let mut matching_bad_run_receipt = receipt.clone();
        matching_bad_run_receipt.operation_id = exact_receipt_bad_run.operation_id.clone();
        assert_eq!(
            replay_commit_operation_receipt(
                &exact_receipt_bad_run,
                Some((&stored_thread, &matching_bad_run_receipt)),
            ),
            Err(CommitOperationAdmissionError::RunIdentityMismatch),
            "A2 intrinsic Run rejection also precedes an otherwise exact receipt",
        );
        let mut identity_conflict = receipt.clone();
        identity_conflict.operation_id.ordinal = 1;
        identity_conflict.payload_hash = CommitPayloadHash("other-hash".into());
        assert_eq!(
            replay_commit_operation_receipt(
                &foreign_thread,
                Some((&stored_thread, &identity_conflict)),
            ),
            Err(CommitOperationAdmissionError::ReceiptIdentityMismatch),
            "A2 original operation-id rejection precedes hash/Thread mismatch",
        );
        let mut hash_conflict = receipt.clone();
        hash_conflict.payload_hash = CommitPayloadHash("other-hash".into());
        assert_eq!(
            replay_commit_operation_receipt(
                &foreign_thread,
                Some((&stored_thread, &hash_conflict)),
            ),
            Err(CommitOperationAdmissionError::PayloadMismatch {
                run_id: operation.operation_id.run_id.0.clone(),
                ordinal: operation.operation_id.ordinal,
            }),
            "A2 original hash rejection precedes Thread mismatch",
        );
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
            replay_commit_operation_receipt(&operation, Some((&stored_thread, &wrong_receipt)))
                .is_err(),
            "A2/E2"
        );
        let mut wrong_hash = receipt;
        wrong_hash.payload_hash = CommitPayloadHash("other".into());
        assert!(
            replay_commit_operation_receipt(&operation, Some((&stored_thread, &wrong_hash)))
                .is_err(),
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

    #[test]
    fn applied_receipt_maps_the_exact_operation_and_successor_version() {
        // Causes: C1 the current Thread version has a successor; C2 the adapter
        // has one durable sequence. Effects: E1 one nonduplicate receipt
        // preserves the operation id, payload hash, durable sequence, and
        // admitted successor; E2 overflow is rejected before an append.
        // Rules P1=C1+C2=>E1 and P2=!C1=>E2. The contract owns successor
        // admission, so adapters cannot introduce another version calculation.
        let operation_id = CommitOperationId::new(RunId("run".into()), 3);
        let payload_hash = CommitPayloadHash("hash".into());
        let receipt = applied_commit_receipt(
            operation_id.clone(),
            payload_hash.clone(),
            9,
            next_commit_thread_version(4).expect("P1 admitted successor"),
        );
        assert_eq!(receipt.operation_id, operation_id, "P1/E1");
        assert_eq!(receipt.payload_hash, payload_hash, "P1/E1");
        assert_eq!(receipt.commit_sequence, 9, "P1/E1");
        assert_eq!(receipt.thread_version, 5, "P1/E1");
        assert!(!receipt.duplicate, "P1/E1");
        assert!(next_commit_thread_version(u64::MAX).is_err(), "P2/E2");
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_worker.acknowledged_projection_is_monotonic
    /// @obligation remote_worker.committed_truth_is_coordinator_owned
    /// @obligation remote_worker.operation_identity_maps_to_one_hash_and_append
    /// @obligation store_history.exact_retry_and_every_rejection_are_atomic_stutters
    /// @obligation store_history.receipt_refines_one_exact_durable_record
    /// @obligation store_history.recovery_preserves_committed_time_and_receipt_identity
    /// @proof-input-debt free_symbol classify_operation_identity run_identity_matches
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

    /// @obligation remote_worker.committed_truth_is_coordinator_owned
    /// @obligation remote_worker.operation_identity_maps_to_one_hash_and_append
    /// @obligation store_history.exact_retry_and_every_rejection_are_atomic_stutters
    /// @obligation store_history.receipt_refines_one_exact_durable_record
    /// @obligation store_history.recovery_preserves_committed_time_and_receipt_identity
    #[kani::proof]
    fn operation_replay_binds_stored_thread_and_preserves_identity_precedence() {
        // Causes are concrete operation/receipt fields: C1 operation Run matches
        // its payload; C2 a stored receipt exists; C3 receipt operation id matches;
        // C4 declared hash matches; C5 stored Thread matches the payload Thread.
        // Rules K1=!C1=>RunIdentityMismatch; K2=C1+!C2=>None, with C3-C5
        // irrelevant; K3=C1+C2+!C3=>ReceiptIdentityMismatch; K4=C1+C2+C3+!C4
        // =>PayloadMismatch even if !C5; K5=C1+C2+C3+C4+!C5=>ReceiptIdentityMismatch;
        // K6=C1+C2+C3+C4+C5=>the exact original receipt with duplicate=true.
        // Select bounded actual records, then call the production replay entry;
        // no free predicate stands in for any comparison it derives. The stale
        // expected version demonstrates receipt replay stays before new CAS.
        // Every input use reads only bits 0..5: F(v) = F(v & 63).
        // The original u8 domain therefore contains four copies of these exact
        // 64 record images. Check every image with the same production call and
        // assertions; no assume, predicate substitute, or unwind cut is used.
        // A single local closure owns construction and checks for all images.
        let check_variant = |variant: u8| {
            // Allocate each varying field as one byte, then choose its ASCII value:
            // this removes conditional literal pointers from the input construction,
            // while preserving the same field values and production comparisons.
            // Safe UTF-8 conversion reuses each Vec; no assumption prunes any input.
            let mut operation_run = vec![b'r'];
            operation_run[0] = if variant & 2 != 0 { b'x' } else { b'r' };
            let operation_run = match String::from_utf8(operation_run) {
                Ok(value) => value,
                Err(_) => panic!("ASCII Run input"),
            };
            let mut receipt_run = vec![b'r'];
            receipt_run[0] = if variant & 32 != 0 { b's' } else { b'r' };
            let receipt_run = match String::from_utf8(receipt_run) {
                Ok(value) => value,
                Err(_) => panic!("ASCII receipt Run input"),
            };
            let mut receipt_hash = vec![b'h'];
            receipt_hash[0] = if variant & 8 != 0 { b'j' } else { b'h' };
            let receipt_hash = match String::from_utf8(receipt_hash) {
                Ok(value) => value,
                Err(_) => panic!("ASCII declared hash input"),
            };
            let mut request_thread = vec![b't'];
            request_thread[0] = if variant & 16 != 0 { b'u' } else { b't' };
            let request_thread = match String::from_utf8(request_thread) {
                Ok(value) => value,
                Err(_) => panic!("ASCII Thread input"),
            };
            let stored_thread = ThreadId("t".into());
            let operation = CommitOperation {
                operation_id: CommitOperationId::new(RunId(operation_run), 0),
                expected_thread_version: u64::MAX,
                payload_hash: CommitPayloadHash("h".into()),
                commit: ThreadCommit {
                    thread_id: ThreadId(request_thread),
                    run: crate::thread::commit::staged::RunDisposition::running(RunId("r".into())),
                    messages: Vec::new(),
                    state: Vec::new(),
                    events: Vec::new(),
                },
            };
            let receipt = CommitReceipt {
                operation_id: CommitOperationId::new(
                    RunId(receipt_run),
                    if variant & 4 != 0 { 1 } else { 0 },
                ),
                commit_sequence: 7,
                thread_version: 1,
                payload_hash: CommitPayloadHash(receipt_hash),
                duplicate: false,
            };
            let existing = if variant & 1 != 0 {
                Some((&stored_thread, &receipt))
            } else {
                None
            };
            let result = replay_commit_operation_receipt(&operation, existing);
            // Check exact result tags/fields without constructing another expected
            // heap-backed error or comparing every unrelated Result/Option payload.
            // K1-K5 retain the same error precedence and every error-owned field;
            // K6 below still checks every field of the actual returned receipt.
            if operation.operation_id.run_id != *operation.commit.run_id() {
                assert!(matches!(
                    result,
                    Err(CommitOperationAdmissionError::RunIdentityMismatch)
                ));
            } else if existing.is_none() {
                assert!(matches!(result, Ok(None)));
            } else if receipt.operation_id != operation.operation_id {
                assert!(matches!(
                    result,
                    Err(CommitOperationAdmissionError::ReceiptIdentityMismatch)
                ));
            } else if receipt.payload_hash != operation.payload_hash {
                match &result {
                    Err(CommitOperationAdmissionError::PayloadMismatch { run_id, ordinal }) => {
                        assert_eq!(run_id, &operation.operation_id.run_id.0);
                        assert_eq!(*ordinal, operation.operation_id.ordinal);
                    }
                    _ => panic!("K4 requires the exact payload-mismatch variant"),
                }
            } else if stored_thread != operation.commit.thread_id {
                assert!(matches!(
                    result,
                    Err(CommitOperationAdmissionError::ReceiptIdentityMismatch)
                ));
            } else {
                // K6 accepts exactly the same Ok(Some) as the two expects, but
                // does not construct a dynamic Debug message for rejected results.
                let duplicate = match result {
                    Ok(Some(receipt)) => receipt,
                    _ => panic!("K6 requires an exact stored receipt"),
                };
                assert_eq!(duplicate.operation_id, receipt.operation_id);
                assert_eq!(duplicate.payload_hash, receipt.payload_hash);
                assert_eq!(duplicate.commit_sequence, receipt.commit_sequence);
                assert_eq!(duplicate.thread_version, receipt.thread_version);
                assert!(duplicate.duplicate);
            }
        };
        check_variant(0);
        check_variant(1);
        check_variant(2);
        check_variant(3);
        check_variant(4);
        check_variant(5);
        check_variant(6);
        check_variant(7);
        check_variant(8);
        check_variant(9);
        check_variant(10);
        check_variant(11);
        check_variant(12);
        check_variant(13);
        check_variant(14);
        check_variant(15);
        check_variant(16);
        check_variant(17);
        check_variant(18);
        check_variant(19);
        check_variant(20);
        check_variant(21);
        check_variant(22);
        check_variant(23);
        check_variant(24);
        check_variant(25);
        check_variant(26);
        check_variant(27);
        check_variant(28);
        check_variant(29);
        check_variant(30);
        check_variant(31);
        check_variant(32);
        check_variant(33);
        check_variant(34);
        check_variant(35);
        check_variant(36);
        check_variant(37);
        check_variant(38);
        check_variant(39);
        check_variant(40);
        check_variant(41);
        check_variant(42);
        check_variant(43);
        check_variant(44);
        check_variant(45);
        check_variant(46);
        check_variant(47);
        check_variant(48);
        check_variant(49);
        check_variant(50);
        check_variant(51);
        check_variant(52);
        check_variant(53);
        check_variant(54);
        check_variant(55);
        check_variant(56);
        check_variant(57);
        check_variant(58);
        check_variant(59);
        check_variant(60);
        check_variant(61);
        check_variant(62);
        check_variant(63);
    }

    /// @obligation remote_worker.committed_truth_is_coordinator_owned
    /// @obligation remote_worker.operation_identity_maps_to_one_hash_and_append
    /// @obligation store_history.concurrent_compare_and_swap_has_one_winner_per_thread_version
    /// @obligation store_history.exact_retry_and_every_rejection_are_atomic_stutters
    /// @obligation store_history.receipt_refines_one_exact_durable_record
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

    /// @obligation store_history.exact_retry_and_every_rejection_are_atomic_stutters
    /// @obligation store_history.receipt_refines_one_exact_durable_record
    /// @obligation store_history.recovery_preserves_committed_time_and_receipt_identity
    #[kani::proof]
    fn applied_receipt_is_exact_and_names_one_successor_record() {
        let ordinal: u64 = kani::any();
        let expected_version: u64 = kani::any();
        kani::assume(expected_version < u64::MAX);
        let thread_version = next_commit_thread_version(expected_version)
            .expect("assumption provides one successor");
        let commit_sequence: u64 = kani::any();
        let operation_id = CommitOperationId::new(RunId(String::new()), ordinal);
        let payload_hash = CommitPayloadHash(String::new());
        let receipt = applied_commit_receipt(
            operation_id.clone(),
            payload_hash.clone(),
            commit_sequence,
            thread_version,
        );
        assert_eq!(receipt.operation_id, operation_id);
        assert_eq!(receipt.payload_hash, payload_hash);
        assert_eq!(receipt.commit_sequence, commit_sequence);
        assert_eq!(
            Some(receipt.thread_version),
            expected_version.checked_add(1)
        );
        assert!(!receipt.duplicate);
    }
}
