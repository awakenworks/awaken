//! Run-scoped evidence for actual model executor invocations, not usage or a
//! provider terminal receipt. The existing ThreadCommit log owns persistence.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::resolved::ModelBinding;
use crate::{MergePolicy, Scope};
use awaken_agent_contract::agent::state::StateKey;

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
}

impl ModelOperation {
    /// Sequence allocation never wraps and does not infer quiescence from any
    /// prior phase. Admission/physical exclusion still belongs to Dispatch.
    pub fn next(previous: Option<&Self>, binding: ModelBinding) -> Option<Self> {
        next_ordinal(previous.map_or(0, |value| value.ordinal.get())).map(|ordinal| Self {
            ordinal,
            binding,
            phase: ModelOperationPhase::Entered,
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
