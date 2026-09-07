//! Run-scoped evidence for actual model executor invocations, not usage or a
//! provider terminal receipt. The existing ThreadCommit log owns persistence.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::resolved::ModelBinding;
use crate::{MergePolicy, Scope};
use awaken_agent_contract::agent::state::StateKey;

/// Evidence about one invocation, independent of response quality and error
/// classification. Missing evidence is unknown, including legacy Returned.
/// Only the trusted adapter may establish NotDispatched or Terminated; a local
/// timeout, dropped stream, HTTP status or request id is not such a proof.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelExecutionEvidence {
    #[default]
    Unknown,
    NotDispatched,
    Terminated,
}

impl ModelExecutionEvidence {
    /// Physical permission only. Retry policy, current claim and budget remain
    /// independent mandatory gates owned by Runtime/Dispatch.
    #[must_use]
    pub const fn permits_reentry(self) -> bool {
        matches!(self, Self::NotDispatched | Self::Terminated)
    }
}

/// Local observation only. In particular, a returned transport error or a
/// dropped timeout future does not prove remote computation has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelOperationPhase {
    /// Entry intent committed before calling the executor. A crash between
    /// this commit and the call is deliberately indistinguishable from entry.
    Entered,
    Returned,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperation {
    pub ordinal: NonZeroU64,
    pub binding: ModelBinding,
    pub phase: ModelOperationPhase,
    #[serde(default)]
    pub execution: ModelExecutionEvidence,
}

impl ModelOperation {
    /// A retained partial requires positive predecessor evidence. Absence is
    /// admissible only for fresh inference, never as a historical stop proof.
    #[must_use]
    pub fn permits_reentry(previous: Option<&Self>, recovering_checkpoint: bool) -> bool {
        previous.map_or(!recovering_checkpoint, |operation| {
            operation.execution.permits_reentry()
        })
    }

    /// Sequence allocation never wraps and does not infer quiescence from any
    /// prior phase. Admission/physical exclusion still belongs to Dispatch.
    pub fn next(previous: Option<&Self>, binding: ModelBinding) -> Option<Self> {
        next_ordinal(previous.map_or(0, |value| value.ordinal.get())).map(|ordinal| Self {
            ordinal,
            binding,
            phase: ModelOperationPhase::Entered,
            execution: ModelExecutionEvidence::Unknown,
        })
    }
}

fn next_ordinal(previous: u64) -> Option<NonZeroU64> {
    previous.checked_add(1).and_then(NonZeroU64::new)
}

/// One latest-value projection; older invocations remain in the append-only
/// Thread state log. ToolBatch and model usage keep their existing owners.
pub struct ActiveModelOperation;

impl StateKey for ActiveModelOperation {
    const KEY: &'static str = "runtime.model_operation.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = Option<ModelOperation>;
}

#[cfg(kani)]
#[kani::proof]
fn model_execution_reentry_requires_positive_evidence() {
    // R1 unknown => deny; R2 not dispatched / terminal => only the physical
    // gate opens. Retry policy and claim/budget remain separate authorities.
    let selector: u8 = kani::any();
    let evidence = match selector % 3 {
        0 => ModelExecutionEvidence::Unknown,
        1 => ModelExecutionEvidence::NotDispatched,
        _ => ModelExecutionEvidence::Terminated,
    };
    assert_eq!(evidence.permits_reentry(), selector % 3 != 0);
    // R3 missing journal + retained checkpoint => deny; R4 neither => fresh
    // entry allowed. Cross R1..R4 with each local phase: phase cannot authorize.
    // This exhausts the production decision, not persistence or adapter truth.
    let present: bool = kani::any();
    let recovering_checkpoint: bool = kani::any();
    let phase_selector: u8 = kani::any();
    let operation = ModelOperation {
        ordinal: NonZeroU64::MIN,
        binding: ModelBinding::new("", "", ""),
        phase: match phase_selector % 3 {
            0 => ModelOperationPhase::Entered,
            1 => ModelOperationPhase::Returned,
            _ => ModelOperationPhase::TimedOut,
        },
        execution: evidence,
    };
    assert_eq!(
        ModelOperation::permits_reentry(present.then_some(&operation), recovering_checkpoint),
        if present {
            selector % 3 != 0
        } else {
            !recovering_checkpoint
        }
    );
}

#[cfg(kani)]
#[kani::proof]
fn model_operation_ordinal_never_wraps_or_reuses_a_coordinate() {
    // C1 previous < MAX / C2 previous == MAX; E1 exact nonzero successor /
    // E2 refusal. R1=C1=>E1; R2=C2=>E2. Exhaust the production u64 kernel.
    let previous: u64 = kani::any();
    match next_ordinal(previous) {
        Some(next) => {
            assert!(previous < u64::MAX);
            assert!(next.get() > previous);
            assert_eq!(next.get() - previous, 1);
        }
        None => assert_eq!(previous, u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_local_phases_never_create_termination_evidence() {
        // L1 old phase is Entered/Returned/TimedOut; L2 evidence field absent;
        // L3 explicit evidence Unknown/NotDispatched/Terminated. Effects: all
        // L1+L2 decode Unknown; L3 round-trips exactly, independent of phase.
        // A local Returned is never promoted to remote termination on upgrade.
        for phase in [
            ModelOperationPhase::Entered,
            ModelOperationPhase::Returned,
            ModelOperationPhase::TimedOut,
        ] {
            let mut operation =
                ModelOperation::next(None, ModelBinding::new("p", "m", "b")).unwrap();
            operation.phase = phase;
            let mut legacy = serde_json::to_value(&operation).unwrap();
            legacy.as_object_mut().unwrap().remove("execution");
            let decoded: ModelOperation = serde_json::from_value(legacy).unwrap();
            assert_eq!(decoded.execution, ModelExecutionEvidence::Unknown);
            assert!(!decoded.execution.permits_reentry());
            for execution in [
                ModelExecutionEvidence::Unknown,
                ModelExecutionEvidence::NotDispatched,
                ModelExecutionEvidence::Terminated,
            ] {
                operation.execution = execution;
                assert_eq!(
                    serde_json::from_value::<ModelOperation>(
                        serde_json::to_value(&operation).unwrap()
                    )
                    .unwrap(),
                    operation
                );
            }
        }
    }

    #[test]
    fn execution_evidence_does_not_change_failure_taxonomy_or_retry_hints() {
        use crate::llm::Error;
        use crate::resilience::Classify;
        // C1 every closed error class; C2 absent or each evidence value.
        // E1 absent => Unknown; E2 code, message, health, routing and delay
        // are preserved for all C1*C2; E3 only evidence controls physical
        // re-entry. Wrapping an already annotated error remains flat.
        for execution in [
            ModelExecutionEvidence::Unknown,
            ModelExecutionEvidence::NotDispatched,
            ModelExecutionEvidence::Terminated,
        ] {
            for error in [
                Error::Binding("binding".into()),
                Error::Provider("provider".into()),
                Error::Timeout("timeout".into()),
                Error::ContextOverflow("context".into()),
                Error::InvalidRequest("request".into()),
                Error::Unauthorized("auth".into()),
                Error::LoginRequired("login".into()),
                Error::ModelNotFound("model".into()),
                Error::ContentFiltered("filter".into()),
                Error::RateLimited {
                    message: "rate".into(),
                    retry_after: Some(std::time::Duration::from_secs(2)),
                },
                Error::Overloaded {
                    message: "overload".into(),
                    retry_after: Some(std::time::Duration::from_secs(3)),
                },
                Error::UsageLimit {
                    message: "usage".into(),
                    reset_after: Some(std::time::Duration::from_secs(4)),
                },
            ] {
                assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
                let expected = (
                    error.code(),
                    error.to_string(),
                    error.is_retryable(),
                    error.retry_after(),
                    error.reset_hint(),
                    error.disposition(),
                );
                let observed = error
                    .with_execution_evidence(execution)
                    .with_execution_evidence(execution);
                assert_eq!(
                    (
                        observed.code(),
                        observed.to_string(),
                        observed.is_retryable(),
                        observed.retry_after(),
                        observed.reset_hint(),
                        observed.disposition()
                    ),
                    expected
                );
                assert_eq!(observed.execution_evidence(), execution);
                let Error::Observed { source, .. } = observed else {
                    panic!("explicit evidence required")
                };
                assert!(!matches!(*source, Error::Observed { .. }));
            }
        }
    }

    #[test]
    fn model_operation_coordinate_and_wire_decision_table() {
        // Causes: C1 absent/ordinary/MAX predecessor; C2 any local phase;
        // C3 zero ordinal on wire. Effects: E1 exact successor; E2 MAX rejects;
        // E3 malformed zero rejects. R1=!MAX+C2=>E1; R2=MAX+C2=>E2;
        // R3=C3=>E3. No phase, including Returned, is termination authority.
        let binding = ModelBinding::new("provider", "model", "adapter");
        for previous in [0, 1, u64::MAX - 1, u64::MAX] {
            for phase in [
                ModelOperationPhase::Entered,
                ModelOperationPhase::Returned,
                ModelOperationPhase::TimedOut,
            ] {
                let prior = NonZeroU64::new(previous).map(|ordinal| ModelOperation {
                    ordinal,
                    binding: binding.clone(),
                    phase,
                    execution: ModelExecutionEvidence::Unknown,
                });
                let next = ModelOperation::next(prior.as_ref(), binding.clone());
                assert_eq!(
                    next.as_ref().map(|v| v.ordinal.get()),
                    previous.checked_add(1)
                );
                if let Some(next) = next {
                    assert_eq!(next.phase, ModelOperationPhase::Entered);
                    let mut wire = serde_json::to_value(&next).unwrap();
                    assert_eq!(
                        serde_json::from_value::<ModelOperation>(wire.clone()).unwrap(),
                        next
                    );
                    wire["ordinal"] = 0.into();
                    assert!(serde_json::from_value::<ModelOperation>(wire).is_err());
                }
            }
        }
    }
}
