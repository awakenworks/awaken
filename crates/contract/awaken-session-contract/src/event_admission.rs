//! Minimal retained admission evidence. No message, tool input or execution log.
use crate::{SessionEventBatch, SessionRevision};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEventAdmission {
    Accepted {
        batch_id: String,
        /// None is a legacy retained receipt with no observation coordinate.
        operation_ids: Option<Vec<String>>,
    },
    Closed {
        batch_id: String,
    },
    Deleted {
        deleted_revision: SessionRevision,
    },
}

impl From<&SessionEventBatch> for SessionEventAdmission {
    fn from(batch: &SessionEventBatch) -> Self {
        if batch.admission_closed {
            Self::Closed {
                batch_id: batch.batch_id.clone(),
            }
        } else {
            Self::Accepted {
                batch_id: batch.batch_id.clone(),
                operation_ids: Some(batch.operation_ids()),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedEventAdmission {
    pub key: String,
    pub request_fingerprint: String,
    pub batch_id: String,
    pub closed: bool,
    /// Opaque identities only; never retain original message/tool content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_ids: Option<Vec<String>>,
}

/// None is an old tombstone with unknown receipt retention. Some(empty) is a
/// complete new deletion snapshot proving no keyed batch was ever accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedSessionEventAdmissions {
    pub deleted_revision: SessionRevision,
    pub admissions: Option<Vec<RetainedEventAdmission>>,
}

impl DeletedSessionEventAdmissions {
    pub fn resolve(
        &self,
        key: &str,
        fingerprint: &str,
    ) -> Result<SessionEventAdmission, crate::RunError> {
        let admissions = self.admissions.as_ref().ok_or_else(|| {
            crate::RunError::unavailable("Legacy Session deletion has no retained admission proof")
        })?;
        let receipt = admissions.iter().find(|receipt| receipt.key == key);
        // Retention owns lookup/payloads; the same canonical admission class
        // as the live root owns fingerprint-before-closed precedence.
        match crate::classify_session_event_batch_admission(
            receipt.is_some(),
            receipt.is_some_and(|receipt| receipt.request_fingerprint == fingerprint),
            receipt.is_some_and(|receipt| receipt.closed),
        ) {
            crate::SessionEventBatchAdmissionClass::Conflict => Err(event_admission_conflict()),
            crate::SessionEventBatchAdmissionClass::Closed => Ok(SessionEventAdmission::Closed {
                batch_id: receipt
                    .expect("closed class has retained evidence")
                    .batch_id
                    .clone(),
            }),
            crate::SessionEventBatchAdmissionClass::Accepted => {
                let receipt = receipt.expect("accepted class has retained evidence");
                Ok(SessionEventAdmission::Accepted {
                    batch_id: receipt.batch_id.clone(),
                    operation_ids: receipt.operation_ids.clone(),
                })
            }
            crate::SessionEventBatchAdmissionClass::Absent => Ok(SessionEventAdmission::Deleted {
                deleted_revision: self.deleted_revision,
            }),
        }
    }
}

pub fn event_admission_conflict() -> crate::RunError {
    crate::RunError {
        message: "Idempotency-Key was already used for another Session Event batch".into(),
        kind: crate::RunErrorKind::BadRequest,
        code: "idempotency_conflict".into(),
    }
}

pub fn event_admission_closed() -> crate::RunError {
    crate::RunError {
        message: "Session Event batch admission was permanently closed".into(),
        kind: crate::RunErrorKind::BadRequest,
        code: "event_admission_closed".into(),
    }
}

#[cfg(any(test, kani))]
mod verification {
    use super::*;

    // This oracle states each observable outcome's necessary and sufficient
    // causes. The caller supplies the expected first index independently; it
    // never searches with the production iterator or calls its error factory.
    fn check_result(
        proof: &DeletedSessionEventAdmissions,
        first: Option<usize>,
        fingerprint: &str,
        result: &Result<SessionEventAdmission, crate::RunError>,
    ) {
        let selected = first.map(|index| &proof.admissions.as_ref().unwrap()[index]);
        match result {
            Err(error) => {
                if proof.admissions.is_none() {
                    assert!(
                        error.kind == crate::RunErrorKind::Unavailable,
                        "exact retained admission field"
                    );
                    assert!(
                        error.code == "unavailable",
                        "exact retained admission field"
                    );
                    assert!(
                        error.message == "Legacy Session deletion has no retained admission proof",
                        "exact retained admission field"
                    );
                } else {
                    let receipt =
                        selected.expect("complete inventory errors require a matching key");
                    assert!(receipt.request_fingerprint != fingerprint);
                    assert!(
                        error.kind == crate::RunErrorKind::BadRequest,
                        "exact retained admission field"
                    );
                    assert!(
                        error.code == "idempotency_conflict",
                        "exact retained admission field"
                    );
                    assert!(
                        error.message
                            == "Idempotency-Key was already used for another Session Event batch",
                        "exact retained admission field"
                    );
                }
            }
            Ok(SessionEventAdmission::Deleted { deleted_revision }) => {
                assert!(proof.admissions.is_some());
                assert!(first.is_none());
                assert!(
                    *deleted_revision == proof.deleted_revision,
                    "exact retained admission field"
                );
            }
            Ok(SessionEventAdmission::Closed { batch_id }) => {
                let receipt = selected.expect("closed requires retained evidence");
                assert!(
                    receipt.request_fingerprint == fingerprint,
                    "exact retained admission field"
                );
                assert!(receipt.closed);
                assert!(
                    *batch_id == receipt.batch_id,
                    "exact retained admission field"
                );
            }
            Ok(SessionEventAdmission::Accepted {
                batch_id,
                operation_ids,
            }) => {
                let receipt = selected.expect("acceptance requires retained evidence");
                assert!(
                    receipt.request_fingerprint == fingerprint,
                    "exact retained admission field"
                );
                assert!(!receipt.closed);
                assert!(
                    *batch_id == receipt.batch_id,
                    "exact retained admission field"
                );
                assert!(
                    *operation_ids == receipt.operation_ids,
                    "exact retained admission field"
                );
            }
        }
    }

    fn check_replay_and_frame(
        proof: DeletedSessionEventAdmissions,
        key: &str,
        fingerprint: &str,
        first: Option<usize>,
    ) {
        let before = proof.clone();
        let mut result = proof.resolve(key, fingerprint);
        check_result(&before, first, fingerprint, &result);
        assert!(proof == before);
        // Mutating the returned owned clone must not poison retained evidence
        // or the next replay, including ordered operation ids and batch id.
        match &mut result {
            Ok(SessionEventAdmission::Accepted {
                batch_id,
                operation_ids,
            }) => {
                batch_id.clear();
                if let Some(ids) = operation_ids {
                    ids.reverse();
                    if let Some(id) = ids.first_mut() {
                        id.clear();
                    }
                }
            }
            Ok(SessionEventAdmission::Closed { batch_id }) => batch_id.clear(),
            _ => {}
        }
        assert!(proof == before);
        check_result(
            &before,
            first,
            fingerprint,
            &proof.resolve(key, fingerprint),
        );
        assert!(proof == before);
    }

    #[cfg(test)]
    #[test]
    fn deleted_admission_decision_table() {
        // Causes C1 legacy None / complete inventory lengths 0,1,2; C2 first
        // matching key at none/0/1 (including duplicate keys); C3 fingerprint
        // same/different independently per receipt; C4 each receipt closed;
        // C5 operation ids None/Some(empty)/Some(one)/Some(two ordered ids).
        // R1 !C1 => exact Unavailable. R2 complete/no key => full revision.
        // R3 first key + different fingerprint => exact conflict even if closed.
        // R4/R5 first key + same fingerprint => original closed/accepted receipt.
        // Every rule checks source equality, mutation-isolated owned output and
        // a second real resolve. The first-index table is independent of find.
        let mut cases = 0;
        for revision in [0, 1, u64::MAX] {
            for admissions in [None, Some(vec![])] {
                check_replay_and_frame(
                    DeletedSessionEventAdmissions {
                        deleted_revision: SessionRevision(revision),
                        admissions,
                    },
                    "k",
                    "fp",
                    None,
                );
                cases += 1;
            }
            // Only occupied slots contribute causes: no phantom second-slot
            // flags in the one-receipt rows. Each receipt owns independent ids.
            for len in [1, 2] {
                for keys in 0..(1 << len) {
                    let first = if keys & 1 != 0 {
                        Some(0)
                    } else if len == 2 && keys & 2 != 0 {
                        Some(1)
                    } else {
                        None
                    };
                    for flags in 0..(1 << (2 * len)) {
                        for ids in 0..(1 << (2 * len)) {
                            let receipts = (0..len)
                                .map(|index| RetainedEventAdmission {
                                    key: if keys & (1 << index) == 0 {
                                        "other"
                                    } else {
                                        "k"
                                    }
                                    .into(),
                                    request_fingerprint: if flags & (1 << index) == 0 {
                                        "fp"
                                    } else {
                                        "changed"
                                    }
                                    .into(),
                                    batch_id: format!("batch:{index}"),
                                    closed: flags & (1 << (len + index)) != 0,
                                    operation_ids: match (ids >> (2 * index)) & 3 {
                                        0 => None,
                                        1 => Some(vec![]),
                                        2 => Some(vec![format!("{index}:op:0")]),
                                        _ => Some(vec![
                                            format!("{index}:op:0"),
                                            format!("{index}:op:1"),
                                        ]),
                                    },
                                })
                                .collect();
                            check_replay_and_frame(
                                DeletedSessionEventAdmissions {
                                    deleted_revision: SessionRevision(revision),
                                    admissions: Some(receipts),
                                },
                                "k",
                                "fp",
                                first,
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 3 * (2 + 32 + 1024));
    }

    #[cfg(kani)]
    fn short_str() -> &'static str {
        match kani::any::<u8>() % 5 {
            0 => "",
            1 => "a",
            2 => "b",
            3 => "ab",
            _ => "ba",
        }
    }

    // Bounded actual resolver proof: Some(empty), all u64 revisions and
    // request key/fingerprint in {empty,a,b,ab,ba}. It must return Deleted with
    // the exact revision, preserve the complete source and repeat identically.
    // Real Vec/String/Result operations and Drop retain all safety checks.
    // This partition excludes legacy None and one/two retained receipts; it
    // does not prove the whole resolver or SQL codec/transaction behavior.
    #[cfg(kani)]
    #[kani::proof]
    #[kani::unwind(4)]
    fn deleted_admission_resolver_preserves_empty() {
        check_replay_and_frame(
            DeletedSessionEventAdmissions {
                deleted_revision: SessionRevision(kani::any()),
                admissions: Some(vec![]),
            },
            short_str(),
            short_str(),
            None,
        );
    }
}
