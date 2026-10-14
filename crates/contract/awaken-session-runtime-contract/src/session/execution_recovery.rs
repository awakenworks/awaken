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
        session_revision: u64,
        runtime_commit_cursor: u64,
        thread_ids: Vec<String>,
        closure_records: Vec<SessionExecutionClosureReference>,
    },
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
