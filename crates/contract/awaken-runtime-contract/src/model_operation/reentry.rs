// @formal entrypoint model_operation.reentry_requires_positive_execution_evidence
//! Reentry projection over original model observations, never provider stop evidence.

use super::{ModelExecutionEvidence, ModelOperationPhase};
use serde::{Deserialize, Serialize};

/// Independent of provider termination and of the business result. A
/// ResponseOnly adapter owns its response consumer, sends no provider-hosted
/// actions, and cannot retain tool/state authority after that consumer closes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelEffectScope {
    #[default]
    Opaque,
    ResponseOnly,
}

impl ModelEffectScope {
    pub const fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque)
    }
}

/// What may be reused after the owned response consumer has returned.
/// This transient value adds no journal field and cannot upgrade Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelReentry {
    Blocked,
    FreshRequest,
    RetainedOutput,
}

impl ModelReentry {
    /// Runtime selects Returned only after closing the exact response consumer;
    /// recovery reads that same committed phase and original pre-entry scope.
    #[must_use]
    pub const fn observed(
        phase: ModelOperationPhase,
        scope: ModelEffectScope,
        execution: ModelExecutionEvidence,
    ) -> Self {
        if execution.permits_reentry() {
            Self::RetainedOutput
        } else if matches!(
            (phase, scope),
            (
                ModelOperationPhase::Returned,
                ModelEffectScope::ResponseOnly
            )
        ) {
            Self::FreshRequest
        } else {
            Self::Blocked
        }
    }

    #[must_use]
    pub const fn allows_request(self) -> bool {
        !matches!(self, Self::Blocked)
    }

    #[must_use]
    pub const fn allows_retained_output(self) -> bool {
        matches!(self, Self::RetainedOutput)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_operation::{NoDispatchProof, ResponseEnvelope, TerminalReceipt};

    #[test]
    fn model_reentry_decision_preserves_original_scope_phase_and_outcome() {
        // Cause/effect graph: C1 original scope opaque/response-only; C2 original
        // phase Entered/Returned/TimedOut; C3 execution Unknown/pre-dispatch/
        // terminal. P1 positive C3 => request+confirmed-output eligibility;
        // P2 Unknown+ResponseOnly+Returned => fresh request only; P3 every other
        // Unknown => blocked. Cross all 18 rows, without changing C3 or inventing
        // a terminal fact. Checkpoint recovery additionally requires P1.
        for scope in [ModelEffectScope::Opaque, ModelEffectScope::ResponseOnly] {
            for phase in [
                ModelOperationPhase::Entered,
                ModelOperationPhase::Returned,
                ModelOperationPhase::TimedOut,
            ] {
                for execution in [
                    ModelExecutionEvidence::Unknown,
                    ModelExecutionEvidence::NotDispatched(NoDispatchProof::runtime_admission()),
                    ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                        ResponseEnvelope::CompleteBody,
                    )),
                ] {
                    let actual = ModelReentry::observed(phase, scope, execution);
                    let positive = execution.permits_reentry();
                    let fresh = scope == ModelEffectScope::ResponseOnly
                        && phase == ModelOperationPhase::Returned;
                    assert_eq!(
                        actual.allows_request(),
                        positive || fresh,
                        "{scope:?}/{phase:?}/{execution:?}"
                    );
                    assert_eq!(actual.allows_retained_output(), positive);
                    assert_eq!(
                        matches!(execution, ModelExecutionEvidence::Unknown),
                        !positive,
                        "outcome stays unknown"
                    );
                }
            }
        }
    }
}
