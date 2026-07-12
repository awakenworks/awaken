//! Execution-poison detection (ADR-0115 parity): a run that keeps failing on
//! execution/infra signals must not be redispatched forever. **Content-blind** — it
//! reads only terminal execution signals, never the run's data. Neutral: it names no
//! dispatch type; the dispatch aggregate feeds it a run's recent attempts and reads
//! the verdict to decide whether to redispatch or dead-letter.

/// One attempt's terminal execution signal (content-blind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptSignal {
    /// Completed with a normal outcome (success, or a model/tool result) — not a
    /// poison signal even if the *task* failed.
    Settled,
    /// The sandbox/process crashed or the infra faulted (launch failure, OOM-kill,
    /// transport loss) — a redispatch-eligible fault.
    InfraFault,
}

/// The redispatch verdict for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoisonVerdict {
    /// Keep going — redispatch is allowed.
    Healthy,
    /// Poisoned — quarantine; stop redispatching (dead-letter).
    Quarantine,
}

/// Classify a run as poisoned when its last `threshold` attempts were **all**
/// consecutive infra faults (a crash-loop). Fewer than `threshold` attempts, any
/// recent `Settled`, a `threshold` of 0, or an empty history is `Healthy` — there is
/// nothing to quarantine on.
#[must_use]
pub fn classify(recent: &[AttemptSignal], threshold: usize) -> PoisonVerdict {
    if threshold == 0 || recent.len() < threshold {
        return PoisonVerdict::Healthy;
    }
    let tail = &recent[recent.len() - threshold..];
    if tail.iter().all(|s| *s == AttemptSignal::InfraFault) {
        PoisonVerdict::Quarantine
    } else {
        PoisonVerdict::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AttemptSignal::{InfraFault, Settled};

    #[test]
    fn under_threshold_is_healthy() {
        assert_eq!(
            classify(&[InfraFault, InfraFault], 3),
            PoisonVerdict::Healthy
        );
    }

    #[test]
    fn a_full_crash_loop_quarantines() {
        assert_eq!(
            classify(&[InfraFault, InfraFault, InfraFault], 3),
            PoisonVerdict::Quarantine
        );
    }

    #[test]
    fn a_recent_settle_breaks_the_loop() {
        // The last 3 include a Settled → not a pure crash-loop.
        assert_eq!(
            classify(&[InfraFault, Settled, InfraFault, InfraFault], 3),
            PoisonVerdict::Healthy
        );
    }

    #[test]
    fn only_the_tail_matters() {
        // Early faults, then a clean tail → healthy.
        assert_eq!(
            classify(&[InfraFault, InfraFault, Settled, Settled], 2),
            PoisonVerdict::Healthy
        );
    }

    #[test]
    fn zero_threshold_or_empty_is_healthy() {
        assert_eq!(classify(&[InfraFault], 0), PoisonVerdict::Healthy);
        assert_eq!(classify(&[], 3), PoisonVerdict::Healthy);
    }
}
