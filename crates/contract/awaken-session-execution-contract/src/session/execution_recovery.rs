// @formal surface exclude=read_model_contract
// @formal reason: Read-only projection of original Session and Dispatch evidence, not a second receipt or state authority.
//! Recovery evidence from the execution owner; physical disposal stays separate.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionRecoveryGap {
    NotTerminal,
    AdmissionFenceMissing,
    ScopeNotFrozen,
    HistoryUnavailable,
    HistoryMissing,
    InventoryIncomplete,
    AttemptStillOpen,
    OrdinaryReturnOnly,
    IdentityMismatch,
    ReadBudgetExceeded,
}

/// Rebuilt on every read. `EffectsClosed` is containment evidence only: Flow
/// must still reconcile accepted business operations and its current cursor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionExecutionRecoveryObservation {
    NotProven {
        reason: SessionExecutionRecoveryGap,
    },
    EffectsClosed {
        #[serde(default, skip_serializing)]
        preparation: Option<SessionRecoveryPreparationReference>,
        // The Managed adapter projects this as a top-level optional field so
        // existing strict recovery readers retain their exact wire shape.
        #[serde(default, skip_serializing)]
        operation_footprint: Option<SessionExecutionOperationFootprint>,
        session_revision: u64,
        runtime_commit_cursor: u64,
        thread_ids: Vec<String>,
        closure_records: Vec<SessionExecutionClosureReference>,
    },
}

/// Scope-wide facts from original entry journals, independent of backend names.
/// Absence is unknown, never a no-effects declaration for legacy execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionOperationFootprint {
    ResponseOnly,
}

/// Original Session aggregate preparation coordinates, never a cleanup ACK.
#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecoveryPreparationReference {
    pub session_id: String,
    pub effect_id: String,
    pub preparation_fingerprint: String,
}

impl SessionRecoveryPreparationReference {
    pub fn identity(&self) -> (&str, &str, &str) {
        (
            self.session_id.as_str(),
            self.effect_id.as_str(),
            self.preparation_fingerprint.as_str(),
        )
    }
}
impl PartialEq for SessionRecoveryPreparationReference {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

#[cfg(test)]
mod preparation_identity_tests {
    use super::*;
    #[test]
    fn preparation_reference_compares_every_original_coordinate() {
        // I1 all original coordinates equal -> same reference; I2 any one
        // coordinate differs -> different. Invalid values remain ordinary
        // data under Eq, never admission proof (consumer validates separately).
        let original = SessionRecoveryPreparationReference {
            session_id: "session".into(),
            effect_id: "effect".into(),
            preparation_fingerprint: "fingerprint".into(),
        };
        assert_eq!(original, original.clone(), "I1");
        for field in 0..3 {
            let mut other = original.clone();
            match field {
                0 => other.session_id.clear(),
                1 => other.effect_id.clear(),
                _ => other.preparation_fingerprint.clear(),
            }
            assert_ne!(original, other, "I2 {field}");
        }
    }
}

/// Coordinates of an original execution-owner record, not a new receipt.
#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionClosureReference {
    pub run_id: String,
    pub evidence_ref: String,
}

impl SessionExecutionClosureReference {
    #[must_use]
    pub fn identity(&self) -> (&str, &str) {
        (&self.run_id, &self.evidence_ref)
    }
}

impl PartialEq for SessionExecutionClosureReference {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}
