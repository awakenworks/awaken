// @formal entrypoint session_execution.one_physical_attempt_per_thread
use super::{DispatchError, RunClaim, WorkerIdentity};
use awaken_agent_contract::agent::thread::Id as ThreadId;
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalAttempt {
    pub claim: RunClaim,
    pub binding: Option<AttemptExecutionBinding>,
    /// Logical Thread whose committed prefix contains this Run's execution
    /// evidence. Internal admission comparisons may omit it; the authoritative
    /// `load_physical_attempt` read freezes it from the same Dispatch row.
    pub thread_id: Option<ThreadId>,
    /// Physical Session partition used by child-Thread recovery. `None` means
    /// the logical Thread is also its storage partition.
    pub session_thread_id: Option<ThreadId>,
    /// Local entry has irreversibly closed, or the exact Worker exited. The
    /// same slot then requires an independent domain-closure ACK; ordinary
    /// late return and continuation handoff cannot clear or replace it.
    pub closing: bool,
}

impl PhysicalAttempt {
    /// Validate retained entry identity, without interpreting it as closure.
    pub fn validate_admission(&self) -> Result<(), DispatchError> {
        self.validate_subject("invalid retained physical admission identity")?;
        if self.closing || self.thread_id.is_none() || self.claim.epoch == 0 {
            return Err(DispatchError::Rejected(
                "retained admission requires an open exact epoch and Thread".into(),
            ));
        }
        Ok(())
    }

    /// Intrinsic subject data shared by return and independent-closure records.
    /// This validates identity only; no recorded data grants execution or stop.
    fn validate_subject(&self, invalid_subject: &str) -> Result<(), DispatchError> {
        if self.claim.run_id.0.trim().is_empty()
            || self.claim.owner.trim().is_empty()
            || self
                .thread_id
                .as_ref()
                .is_some_and(|id| id.0.trim().is_empty())
            || self
                .session_thread_id
                .as_ref()
                .is_some_and(|id| id.0.trim().is_empty())
        {
            return Err(DispatchError::Rejected(invalid_subject.into()));
        }
        if let Some(binding) = &self.binding {
            binding.validate_for(&self.claim)?;
        }
        Ok(())
    }
}

/// Local observations of the original drive, never an assertion of Provider
/// or Session-domain termination. Earlier resolution IO and external durable
/// work are outside this observation's scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptReturnCause {
    OperationFutureNotEntered,
    StructuredReturnObserved,
}

/// The exact original slot frozen in the ordinary ACK transaction. Its stable
/// reference is the original operation event cursor; no Provider URI is minted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptAcknowledgementRecord {
    attempt: PhysicalAttempt,
    cause: AttemptReturnCause,
}

impl AttemptAcknowledgementRecord {
    pub fn try_new(
        attempt: PhysicalAttempt,
        cause: AttemptReturnCause,
    ) -> Result<Self, DispatchError> {
        let record = Self { attempt, cause };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<(), DispatchError> {
        if self.attempt.closing {
            return Err(DispatchError::Rejected(
                "ordinary ACK cannot attest an independently closing attempt".into(),
            ));
        }
        self.attempt
            .validate_subject("attempt fact requires its exact claim and Thread subject")
    }

    pub fn attempt(&self) -> &PhysicalAttempt {
        &self.attempt
    }
    pub fn cause(&self) -> AttemptReturnCause {
        self.cause
    }
}

/// The original trusted source's two different closure assertions. A retained
/// value describes one attempt, never complete old-Worker-domain membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptClosureKind {
    AllExecutionStopped,
    ControlledExecutionClosed,
}

/// Non-secret retained data accepted only through Coordinator attempt control.
/// Deserializing this record grants no control role or Provider stop authority;
/// the application still obtains its value from the private trusted receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptClosureRecord {
    attempt: PhysicalAttempt,
    kind: AttemptClosureKind,
    evidence_ref: String,
}

impl AttemptClosureRecord {
    pub fn try_new(
        attempt: PhysicalAttempt,
        kind: AttemptClosureKind,
        evidence_ref: impl Into<String>,
    ) -> Result<Self, DispatchError> {
        let record = Self {
            attempt,
            kind,
            evidence_ref: evidence_ref.into(),
        };
        record.validate()?;
        Ok(record)
    }

    /// Intrinsic data validation, not proof of Provider honesty or a grant.
    pub fn validate(&self) -> Result<(), DispatchError> {
        let invalid_subject =
            "attempt closure requires its exact closed subject, Thread and Provider reference";
        if !self.attempt.closing || self.evidence_ref.trim().is_empty() {
            return Err(DispatchError::Rejected(invalid_subject.into()));
        }
        self.attempt.validate_subject(invalid_subject)
    }

    pub fn attempt(&self) -> &PhysicalAttempt {
        &self.attempt
    }
    pub fn kind(&self) -> AttemptClosureKind {
        self.kind
    }
    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }
}

/// One original operation-page data sum. Independent closure retains its old
/// bytes; an ordinary acknowledgement cannot be consumed as either stop kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum AttemptPhysicalFact {
    #[serde(rename = "attempt_acknowledgement")]
    Acknowledgement {
        record: AttemptAcknowledgementRecord,
    },
    #[serde(rename = "attempt_closure")]
    IndependentClosure { record: AttemptClosureRecord },
}

impl AttemptPhysicalFact {
    pub fn attempt(&self) -> &PhysicalAttempt {
        match self {
            Self::Acknowledgement { record } => record.attempt(),
            Self::IndependentClosure { record } => record.attempt(),
        }
    }
    pub fn validate(&self) -> Result<(), DispatchError> {
        match self {
            Self::Acknowledgement { record } => record.validate(),
            Self::IndependentClosure { record } => record.validate(),
        }
    }
}

/// Server-local read data using the original operation table's global cursor.
/// This is not part of the public diagnostics Page or a permission receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptClosureEvent {
    pub cursor: crate::DispatchCursor,
    pub recorded_at_ms: Option<u64>,
    pub record: AttemptPhysicalFact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptClosurePage {
    pub events: Vec<AttemptClosureEvent>,
    pub next_cursor: crate::DispatchCursor,
}

/// Response-loss retry must preserve even the absence of a binding. In
/// particular, adding coordinates after effects have started cannot turn a
/// historical unknown execution into a provider-bound execution.
#[must_use]
pub const fn attempt_binding_retry_is_exact(exact_slot: bool, binding_equal: bool) -> bool {
    !exact_slot || binding_equal
}

#[cfg(kani)]
/// @obligation session_execution.one_physical_attempt_per_thread
/// @proof-input-debt free_symbol attempt_binding_retry_is_exact binding_equal
/// @proof-input-debt free_symbol attempt_binding_retry_is_exact exact_slot
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
