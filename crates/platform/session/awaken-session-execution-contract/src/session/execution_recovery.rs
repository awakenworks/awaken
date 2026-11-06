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
        /// Original operation coordinates for the business owner's reconciliation.
        /// Protocol adapters explicitly select this expanded read projection.
        #[serde(default, skip_serializing)]
        operation_inventory: Option<SessionExecutionOperationInventory>,
        session_revision: u64,
        runtime_commit_cursor: u64,
        thread_ids: Vec<String>,
        closure_records: Vec<SessionExecutionClosureReference>,
    },
}

/// Select original read details, without authorizing execution or retry.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionEvidenceSelection {
    /// Opt-in safe configuration and original model-operation inspection.
    #[serde(default)]
    pub include_inspection: bool,
    #[serde(default)]
    pub include_operation_inventory: bool,
}

impl SessionExecutionRecoveryObservation {
    /// Move original details into a caller's read projection. Closure coordinates
    /// remain unchanged and the default selection retains the existing surface.
    pub fn take_projection_details(
        &mut self,
        selection: SessionExecutionEvidenceSelection,
    ) -> (
        Option<SessionExecutionOperationFootprint>,
        Option<SessionRecoveryPreparationReference>,
        Option<SessionExecutionOperationInventory>,
    ) {
        match self {
            Self::EffectsClosed {
                operation_footprint,
                preparation,
                operation_inventory,
                ..
            } => (
                operation_footprint.take(),
                preparation.take(),
                operation_inventory
                    .take()
                    .filter(|_| selection.include_operation_inventory),
            ),
            Self::NotProven { .. } => (None, None, None),
        }
    }
}

/// Read-only source-I/O boundary, independent of business-effect closure.
/// This value is rebuilt from the original current Dispatch scope; it is not
/// serialized, persisted or accepted as an attempt/cleanup acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionSourceDispatchObservation {
    Settled,
    Pending,
    NotProven { reason: SessionExecutionRecoveryGap },
}

impl Default for SessionSourceDispatchObservation {
    fn default() -> Self {
        Self::NotProven {
            reason: SessionExecutionRecoveryGap::HistoryUnavailable,
        }
    }
}

impl SessionSourceDispatchObservation {
    /// Carry the current source boundary into preparation authorization without
    /// promoting it to complete execution or business-effect closure.
    pub fn terminal_source_dispatch(
        self,
    ) -> Result<crate::SessionTerminalSourceDispatch, crate::SessionRealizationControlFailure> {
        use crate::SessionExecutionRecoveryGap as Gap;
        use crate::{SessionRealizationControlFailure, SessionTerminalSourceDispatch as Source};
        match self {
            Self::Settled => Ok(Source::Settled {}),
            Self::Pending => Ok(Source::Pending {}),
            Self::NotProven {
                reason:
                    reason @ (Gap::IdentityMismatch
                    | Gap::NotTerminal
                    | Gap::AdmissionFenceMissing
                    | Gap::ScopeNotFrozen),
            } => Err(SessionRealizationControlFailure::Invalid(format!(
                "terminal source recovery projection is invalid: {reason:?}"
            ))),
            Self::NotProven { reason } => Ok(Source::Unavailable {
                detail: format!("current source dispatch boundary is unproven: {reason:?}"),
            }),
        }
    }
}

/// Scope-wide facts from original entry journals, independent of backend names.
/// Absence is unknown, never a no-effects declaration for legacy execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionOperationFootprint {
    ResponseOnly,
}

/// Complete original tool selection when models have no provider-hosted effects.
/// Opaque model scope remains unproven; these references say nothing about the
/// settlement or physical closure of the listed tool operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionExecutionOperationInventory {
    NotProven {
        reason: SessionExecutionRecoveryGap,
    },
    Recorded {
        tools: Vec<SessionExecutionToolOperation>,
    },
}

/// The original durable ToolBatch operation, without arguments or bearer data.
/// This reference is not evidence that its business effect succeeded or stopped.
#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionToolOperation {
    pub thread_id: String,
    pub run_id: String,
    pub operation_id: String,
    pub tool_id: String,
}

impl SessionExecutionToolOperation {
    /// Every coordinate is required for exact operation-source equality; the
    /// tool name cannot be borrowed from another Thread, Run or operation.
    pub fn identity(&self) -> (&str, &str, &str, &str) {
        (
            &self.thread_id,
            &self.run_id,
            &self.operation_id,
            &self.tool_id,
        )
    }
}

impl PartialEq for SessionExecutionToolOperation {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
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
mod recovery_observation_tests {
    use super::*;
    #[test]
    fn operation_inventory_reference_requires_every_original_coordinate() {
        // O1 exact Thread/Run/operation/tool -> equal; O2 changing any single
        // coordinate -> unequal. Neither equality nor the read reference grants
        // business replay or physical-disposal authority.
        let original = SessionExecutionToolOperation {
            thread_id: "thread".into(),
            run_id: "run".into(),
            operation_id: "operation".into(),
            tool_id: "tool".into(),
        };
        assert_eq!(original, original.clone(), "O1");
        for field in 0..4 {
            let mut changed = original.clone();
            match field {
                0 => changed.thread_id.push_str("-other"),
                1 => changed.run_id.push_str("-other"),
                2 => changed.operation_id.push_str("-other"),
                _ => changed.tool_id.push_str("-other"),
            }
            assert_ne!(original, changed, "O2 {field}");
        }
    }

    #[test]
    fn terminal_source_authorization_keeps_current_dispatch_distinct_from_global_closure() {
        // Cause/effect table over the source reader's closed vocabulary:
        // S1 current scope settled -> live source I/O; S2 an occupied/runnable
        // member -> Pending; S3 unproven observations -> Unavailable;
        // S4 nonterminal, missing admission/scope or foreign identity -> Invalid.
        // No unproven source observation grants access. This conversion cannot
        // close a slot, certify business closure or create cleanup evidence.
        use crate::{
            SessionExecutionRecoveryGap as Gap, SessionSourceDispatchObservation as Observation,
        };
        use crate::{SessionRealizationControlFailure, SessionTerminalSourceDispatch as Source};
        let closed = Observation::Settled;
        assert_eq!(
            closed.terminal_source_dispatch().unwrap(),
            Source::Settled {},
            "S1"
        );
        assert_eq!(
            Observation::Pending.terminal_source_dispatch().unwrap(),
            Source::Pending {},
            "S2"
        );
        assert!(
            matches!(
                Observation::default().terminal_source_dispatch(),
                Ok(Source::Unavailable { .. })
            ),
            "S3 an unimplemented fixture/read cannot grant source access"
        );
        for reason in [
            Gap::HistoryUnavailable,
            Gap::HistoryMissing,
            Gap::InventoryIncomplete,
            Gap::AttemptStillOpen,
            Gap::OrdinaryReturnOnly,
            Gap::ReadBudgetExceeded,
        ] {
            assert!(
                matches!(
                    (Observation::NotProven { reason }).terminal_source_dispatch(),
                    Ok(Source::Unavailable { .. })
                ),
                "S3"
            );
        }
        for reason in [
            Gap::NotTerminal,
            Gap::AdmissionFenceMissing,
            Gap::ScopeNotFrozen,
            Gap::IdentityMismatch,
        ] {
            assert!(
                matches!(
                    (Observation::NotProven { reason }).terminal_source_dispatch(),
                    Err(SessionRealizationControlFailure::Invalid(_))
                ),
                "S4"
            );
        }
    }

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
