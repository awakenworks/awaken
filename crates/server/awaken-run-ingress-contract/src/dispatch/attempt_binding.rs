use super::{DispatchError, RunClaim, WorkerIdentity};
use serde::{Deserialize, Serialize};

/// Non-secret, provider-scoped coordinate frozen before executor entry.
/// This is neither a termination receipt nor proof that a caller controls the
/// named process. The trusted composition must obtain the coordinate from its
/// execution provider; a recovery consumer must independently verify that
/// provenance and all remote participants before authorizing any transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptExecutionBinding {
    worker: WorkerIdentity,
    provider_id: String,
    execution_ref: String,
}

impl AttemptExecutionBinding {
    pub fn new(
        worker: WorkerIdentity,
        provider_id: impl Into<String>,
        execution_ref: impl Into<String>,
    ) -> Result<Self, DispatchError> {
        let binding = Self {
            worker,
            provider_id: provider_id.into(),
            execution_ref: execution_ref.into(),
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn worker(&self) -> &WorkerIdentity {
        &self.worker
    }
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
    pub fn execution_ref(&self) -> &str {
        &self.execution_ref
    }

    pub fn validate_for(&self, claim: &RunClaim) -> Result<(), DispatchError> {
        self.validate()?;
        if self.worker.lease_owner() != claim.owner {
            return Err(DispatchError::Rejected(
                "execution binding does not belong to the claim's Worker incarnation".into(),
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), DispatchError> {
        if self.worker.generation == 0
            || [
                self.worker.worker_id.as_str(),
                self.worker.incarnation_id.as_str(),
                self.provider_id.as_str(),
                self.execution_ref.as_str(),
            ]
            .into_iter()
            .any(|part| part.trim().is_empty())
        {
            return Err(DispatchError::Rejected("execution binding requires a provider, physical reference and nonzero Worker incarnation".into()));
        }
        Ok(())
    }
}

/// Narrow read of the existing physical slot, not the current mutation lease.
/// Missing binding means historical/unbound execution, never stopped execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalAttempt {
    pub claim: RunClaim,
    pub binding: Option<AttemptExecutionBinding>,
}

/// Response-loss retry must preserve even the absence of a binding. In
/// particular, adding coordinates after effects have started cannot turn a
/// historical unknown execution into a provider-bound execution.
#[must_use]
pub const fn attempt_binding_retry_is_exact(exact_slot: bool, binding_equal: bool) -> bool {
    !exact_slot || binding_equal
}

#[cfg(kani)]
#[kani::proof]
fn physical_binding_cannot_change_on_admission_retry() {
    // B1 exact slot + equal binding => stutter; B2 exact + different (including
    // None/Some) => no admission; B3 other/empty => ordinary slot kernel decides.
    let exact: bool = kani::any();
    let equal: bool = kani::any();
    let admitted = attempt_binding_retry_is_exact(exact, equal);
    assert_eq!(admitted, !exact || equal);
    assert!(!admitted || !exact || equal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_requires_complete_coordinates_even_after_deserialization() {
        // B20 each missing/blank identity coordinate or zero generation =>
        // reject; B21 complete coordinate => valid only for its claim owner.
        // Persistence serde is deliberately not a provider-evidence boundary:
        // malformed decoded values still require admission validation.
        let worker = WorkerIdentity::new("worker", "boot", 1);
        let good = AttemptExecutionBinding::new(worker.clone(), "cluster", "pod").unwrap();
        let claim = RunClaim {
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            owner: worker.lease_owner(),
            epoch: 1,
        };
        assert!(good.validate_for(&claim).is_ok());
        let mut foreign = claim.clone();
        foreign.owner = "foreign".into();
        assert!(good.validate_for(&foreign).is_err());
        for path in [
            "worker_id",
            "incarnation_id",
            "provider_id",
            "execution_ref",
            "generation",
        ] {
            let mut json = serde_json::to_value(&good).unwrap();
            match path {
                "worker_id" | "incarnation_id" => json["worker"][path] = "  ".into(),
                "generation" => json["worker"][path] = 0.into(),
                _ => json[path] = "  ".into(),
            }
            let invalid: AttemptExecutionBinding = serde_json::from_value(json).unwrap();
            assert!(invalid.validate_for(&claim).is_err(), "B20 {path}");
        }
        for exact in [false, true] {
            for equal in [false, true] {
                assert_eq!(
                    attempt_binding_retry_is_exact(exact, equal),
                    !exact || equal
                );
            }
        }
    }
}
