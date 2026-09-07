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
        match admissions.iter().find(|receipt| receipt.key == key) {
            Some(receipt) if receipt.request_fingerprint != fingerprint => {
                Err(event_admission_conflict())
            }
            Some(receipt) if receipt.closed => Ok(SessionEventAdmission::Closed {
                batch_id: receipt.batch_id.clone(),
            }),
            Some(receipt) => Ok(SessionEventAdmission::Accepted {
                batch_id: receipt.batch_id.clone(),
                operation_ids: receipt.operation_ids.clone(),
            }),
            None => Ok(SessionEventAdmission::Deleted {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_admission_decision_table() {
        // Causes: C1 complete inventory, C2 matching key, C3 matching body,
        // C4 accepted versus closed. Effects: legacy unknown never releases;
        // absent key is fenced by deletion; matching receipt retains outcome;
        // changed body conflicts. R1 !C1=>unknown; R2 C1,!C2=>deleted;
        // R3 C1,C2,!C3=>conflict; R4/R5 C1,C2,C3=>original C4.
        let mut proof = DeletedSessionEventAdmissions {
            deleted_revision: SessionRevision(7),
            admissions: None,
        };
        assert_eq!(
            proof.resolve("key", "fp").unwrap_err().kind,
            crate::RunErrorKind::Unavailable
        );
        proof.admissions = Some(vec![]);
        assert_eq!(
            proof.resolve("key", "fp").unwrap(),
            SessionEventAdmission::Deleted {
                deleted_revision: SessionRevision(7)
            }
        );
        for closed in [false, true] {
            proof.admissions = Some(vec![RetainedEventAdmission {
                key: "key".into(),
                request_fingerprint: "fp".into(),
                batch_id: "original".into(),
                closed,
                operation_ids: None,
            }]);
            assert_eq!(
                proof.resolve("key", "changed").unwrap_err().code,
                "idempotency_conflict"
            );
            assert_eq!(
                proof.resolve("key", "fp").unwrap(),
                if closed {
                    SessionEventAdmission::Closed {
                        batch_id: "original".into(),
                    }
                } else {
                    SessionEventAdmission::Accepted {
                        batch_id: "original".into(),
                        operation_ids: None,
                    }
                }
            );
            assert_eq!(
                proof.resolve("other", "fp").unwrap(),
                SessionEventAdmission::Deleted {
                    deleted_revision: SessionRevision(7)
                }
            );
        }
    }
}
