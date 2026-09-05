//! The circuit's only transition authority. The enclosing adapter owns the
//! mutex, model map, clock observation and metrics, but no state decisions.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(super) enum Phase {
    Closed,
    Open,
    HalfOpen { probes: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(super) struct CircuitKernel {
    pub(super) phase: Phase,
    pub(super) failures: u32,
    pub(super) generation: u64,
}

impl Default for CircuitKernel {
    fn default() -> Self {
        Self {
            phase: Phase::Closed,
            failures: 0,
            generation: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(super) struct PermitKey {
    generation: u64,
    probe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmissionError {
    Open,
    ProbeCapacity,
    GenerationExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(super) enum Completion {
    Success,
    RetryableFailure,
    Inconclusive,
}

impl CircuitKernel {
    pub(super) fn admit(
        &mut self,
        max_probes: u32,
        cooldown_elapsed: bool,
    ) -> Result<PermitKey, AdmissionError> {
        let probe = match self.phase {
            Phase::Closed => false,
            Phase::Open => {
                if !cooldown_elapsed {
                    return Err(AdmissionError::Open);
                }
                if max_probes == 0 {
                    return Err(AdmissionError::ProbeCapacity);
                }
                let next = self
                    .generation
                    .checked_add(1)
                    .ok_or(AdmissionError::GenerationExhausted)?;
                self.generation = next;
                self.phase = Phase::HalfOpen { probes: 1 };
                true
            }
            Phase::HalfOpen { probes } => {
                if probes >= max_probes {
                    return Err(AdmissionError::ProbeCapacity);
                }
                self.phase = Phase::HalfOpen { probes: probes + 1 };
                true
            }
        };
        Ok(PermitKey {
            generation: self.generation,
            probe,
        })
    }

    /// Return whether the adapter must start a new cooldown. Every completion,
    /// including an ordinary Closed request, must match both epoch and phase.
    pub(super) fn finish(
        &mut self,
        permit: PermitKey,
        outcome: Completion,
        threshold: u32,
    ) -> bool {
        let current = permit.generation == self.generation
            && match self.phase {
                Phase::Closed => !permit.probe,
                Phase::HalfOpen { .. } => permit.probe,
                Phase::Open => false,
            };
        if !current {
            return false;
        }
        match outcome {
            Completion::Success => {
                self.phase = Phase::Closed;
                self.failures = 0;
                false
            }
            Completion::RetryableFailure => {
                self.failures = self.failures.saturating_add(1);
                if permit.probe || self.failures >= threshold {
                    self.phase = Phase::Open;
                    true
                } else {
                    false
                }
            }
            Completion::Inconclusive => {
                if permit.probe {
                    self.phase = Phase::Open;
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn circuit_admission_is_capacity_bounded_nonwrapping_and_rejection_stutters() {
        // Causes: arbitrary u32 capacity/probes, u64 epoch, phase, cooldown.
        // A1 Closed admits a fenced ordinary key. A2 Open admits iff cooldown,
        // positive capacity and an available successor. A3 HalfOpen admits iff
        // probes < capacity. Every other row stutters. No narrowed integer bound.
        let before: CircuitKernel = kani::any();
        let max: u32 = kani::any();
        let elapsed: bool = kani::any();
        let mut after = before;
        let result = after.admit(max, elapsed);
        let admissible = match before.phase {
            Phase::Closed => true,
            Phase::Open => elapsed && max > 0 && before.generation < u64::MAX,
            Phase::HalfOpen { probes } => probes < max,
        };
        assert_eq!(result.is_ok(), admissible);
        assert_eq!(after.failures, before.failures);
        match result {
            Err(_) => assert_eq!(after, before),
            Ok(key) => {
                assert_eq!(key.generation, after.generation);
                assert!(after.generation >= before.generation);
                match after.phase {
                    Phase::HalfOpen { probes } => {
                        assert!(key.probe && probes > 0 && probes <= max);
                        if before.phase == Phase::Open {
                            assert!(after.generation > before.generation);
                        } else {
                            assert_eq!(after.generation, before.generation);
                        }
                    }
                    Phase::Closed => {
                        assert!(!key.probe);
                        assert_eq!(after, before);
                    }
                    Phase::Open => unreachable!(),
                }
            }
        }
    }

    #[kani::proof]
    fn circuit_completion_requires_exact_phase_and_generation_for_every_outcome() {
        // F1 stale phase or epoch => exact stutter for success/failure/drop.
        // F2 current success => Closed/reset. F3 retryable => saturating count
        // and threshold-or-probe reopening. F4 inconclusive => only a current
        // probe reopens, without counting a provider failure. All integer values.
        let before: CircuitKernel = kani::any();
        let permit: PermitKey = kani::any();
        let outcome: Completion = kani::any();
        let threshold: u32 = kani::any();
        let mut after = before;
        let opened = after.finish(permit, outcome, threshold);
        let eligible = permit.generation == before.generation
            && ((before.phase == Phase::Closed && !permit.probe)
                || (matches!(before.phase, Phase::HalfOpen { .. }) && permit.probe));
        assert_eq!(after.generation, before.generation);
        if !eligible {
            assert_eq!(after, before);
            assert!(!opened);
        } else {
            match outcome {
                Completion::Success => {
                    assert_eq!(after.phase, Phase::Closed);
                    assert_eq!(after.failures, 0);
                    assert!(!opened);
                }
                Completion::RetryableFailure => {
                    assert_eq!(after.failures, before.failures.saturating_add(1));
                    assert_eq!(opened, permit.probe || after.failures >= threshold);
                    assert_eq!(after.phase, if opened { Phase::Open } else { before.phase });
                }
                Completion::Inconclusive => {
                    assert_eq!(after.failures, before.failures);
                    assert_eq!(opened, permit.probe);
                    assert_eq!(after.phase, if opened { Phase::Open } else { before.phase });
                }
            }
        }
    }
}
