//! Retry cadence for the existing Session realization recovery cycle.

pub(super) fn session_recovery_delay(failure_streak: u32) -> std::time::Duration {
    const BASE_SECONDS: u64 = 30;
    const MAX_SECONDS: u64 = 300;
    let multiplier = 1_u64.checked_shl(failure_streak.min(4)).unwrap_or(16);
    std::time::Duration::from_secs(
        BASE_SECONDS
            .checked_mul(multiplier)
            .unwrap_or(MAX_SECONDS)
            .min(MAX_SECONDS),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionRecoveryTrigger {
    Scheduled,
    ExternalWake,
}

/// Select the next delay without allowing an early dependency hint to postpone
/// an already-scheduled retry. The caller supplies the remaining duration from
/// the existing single deadline; this function creates no second timer.
pub(super) fn next_session_recovery_delay(
    trigger: SessionRecoveryTrigger,
    existing_remaining: std::time::Duration,
    failure_streak: u32,
) -> std::time::Duration {
    let proposed = session_recovery_delay(failure_streak);
    match trigger {
        SessionRecoveryTrigger::Scheduled => proposed,
        SessionRecoveryTrigger::ExternalWake => existing_remaining.min(proposed),
    }
}

#[cfg(kani)]
mod proofs {
    use super::{SessionRecoveryTrigger, next_session_recovery_delay, session_recovery_delay};
    use std::time::Duration;

    #[kani::proof]
    fn session_recovery_backoff_is_bounded_monotonic_and_saturating() {
        let failure_streak: u32 = kani::any();
        let delay = session_recovery_delay(failure_streak);
        let successor = session_recovery_delay(failure_streak.saturating_add(1));

        assert!(delay >= Duration::from_secs(30));
        assert!(delay <= Duration::from_secs(300));
        assert!(successor >= delay);
        if failure_streak >= 4 {
            assert_eq!(delay, Duration::from_secs(300));
        }
    }

    #[kani::proof]
    fn dependency_wake_never_postpones_the_single_existing_deadline() {
        let existing_seconds: u64 = kani::any();
        let existing_nanos: u32 = kani::any();
        kani::assume(existing_nanos < 1_000_000_000);
        let failure_streak: u32 = kani::any();
        let existing = Duration::new(existing_seconds, existing_nanos);
        let proposed = session_recovery_delay(failure_streak);
        let selected = next_session_recovery_delay(
            SessionRecoveryTrigger::ExternalWake,
            existing,
            failure_streak,
        );

        assert!(selected <= existing);
        assert!(selected <= proposed);
        assert_eq!(
            next_session_recovery_delay(
                SessionRecoveryTrigger::Scheduled,
                existing,
                failure_streak,
            ),
            proposed,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionRecoveryTrigger, next_session_recovery_delay, session_recovery_delay};

    #[test]
    fn retry_backoff_follows_the_failure_streak_decision_table() {
        /* Causes: C1 consecutive retryable failure count. Effect: E1 next
         * recovery delay. Rules: R1 C1=0=>30s normal cadence; R2 C1=1=>60s;
         * R3 C1=2=>120s; R4 C1=3=>240s; R5 C1>=4=>300s cap. Quarantine is
         * excluded because it is operator-repair work, not retryable work. */
        let rules = [(0, 30), (1, 60), (2, 120), (3, 240), (4, 300), (99, 300)];
        for (streak, seconds) in rules {
            assert_eq!(session_recovery_delay(streak).as_secs(), seconds);
        }
    }

    #[test]
    fn dependency_wakes_never_postpone_the_existing_retry_deadline() {
        /* Causes: C1 the scheduled deadline fired; C2 an external dependency
         * changed before the deadline; C3 the new failure streak proposes a
         * later/earlier deadline. Effects: E1 C1 installs the proposed backoff;
         * E2 C2+C3(later) preserves the existing earlier deadline; E3
         * C2+C3(earlier) advances to the proposed deadline. The constraint is
         * one deadline only: a wake is a hint, never a parallel scheduler.
         * Rules: S1=(C1, streak=2)=>120s; S2=(C2, existing=30s,
         * streak=2)=>30s; S3=(C2, existing=90s, streak=0)=>30s. */
        use std::time::Duration;

        assert_eq!(
            next_session_recovery_delay(SessionRecoveryTrigger::Scheduled, Duration::ZERO, 2,),
            Duration::from_secs(120),
            "S1/E1",
        );
        assert_eq!(
            next_session_recovery_delay(
                SessionRecoveryTrigger::ExternalWake,
                Duration::from_secs(30),
                2,
            ),
            Duration::from_secs(30),
            "S2/E2",
        );
        assert_eq!(
            next_session_recovery_delay(
                SessionRecoveryTrigger::ExternalWake,
                Duration::from_secs(90),
                0,
            ),
            Duration::from_secs(30),
            "S3/E3",
        );
    }
}
