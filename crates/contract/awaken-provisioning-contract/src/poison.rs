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

/// Whether a run should be redispatched given its poison verdict — the crash-loop
/// gate (awaken-next `PodSupervisor` / ADR-0115): a `Quarantine` verdict stops
/// redispatch (dead-letter) so a crash-looping run can't pin the fleet; `Healthy`
/// continues. Pure and content-blind; the dispatch plane consumes it.
#[must_use]
pub fn should_redispatch(verdict: PoisonVerdict) -> bool {
    matches!(verdict, PoisonVerdict::Healthy)
}

/// The resolution of a call that was outstanding when the sandbox/process crashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightOutcome {
    /// The call completed before the crash — its result stands.
    Settled,
    /// The call was in flight — its effect **may** have happened, so it must not be
    /// silently retried; the runtime surfaces it as indeterminate/error, never a
    /// fabricated success.
    Indeterminate,
}

/// Resolve a crashed sandbox's outstanding call: a call still in flight at the crash
/// is `Indeterminate` (content-blind — the effect may have run), otherwise `Settled`.
/// The runtime maps `Indeterminate` onto its tool-result vocabulary.
#[must_use]
pub fn resolve_inflight(in_flight_at_crash: bool) -> InFlightOutcome {
    if in_flight_at_crash {
        InFlightOutcome::Indeterminate
    } else {
        InFlightOutcome::Settled
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

    #[test]
    fn a_threshold_of_one_quarantines_on_a_single_infra_fault() {
        // Boundary: threshold == 1 means one infra fault at the tail is a crash-loop,
        // but a single Settled tail is healthy.
        assert_eq!(
            classify(&[Settled, InfraFault], 1),
            PoisonVerdict::Quarantine
        );
        assert_eq!(classify(&[InfraFault, Settled], 1), PoisonVerdict::Healthy);
    }

    #[test]
    fn quarantine_stops_redispatch() {
        assert!(should_redispatch(PoisonVerdict::Healthy));
        assert!(!should_redispatch(PoisonVerdict::Quarantine));
    }

    #[test]
    fn an_in_flight_call_at_crash_is_indeterminate() {
        assert_eq!(resolve_inflight(true), InFlightOutcome::Indeterminate);
        assert_eq!(resolve_inflight(false), InFlightOutcome::Settled);
    }
}
