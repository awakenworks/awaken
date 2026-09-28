// @formal entrypoint session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
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

pub(super) const EXTERNAL_WAKE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(1);

/// Coalesce a burst of committed dependency hints without delaying the existing
/// scheduled recovery deadline. Hints received while a full scan runs retain
/// one Tokio Notify permit; this bounded delay lets the rest of that commit
/// burst join the same next scan instead of turning every arrival into another
/// scan of all Sessions.
pub(super) fn external_wake_coalescing_delay(
    scheduled_remaining: std::time::Duration,
    cooldown_remaining: std::time::Duration,
) -> std::time::Duration {
    scheduled_remaining.min(cooldown_remaining.min(EXTERNAL_WAKE_COOLDOWN))
}

/// Arm the coalescing window only where a hint can actually be redundant, and
/// only for what that redundancy cost.
///
/// A scan reached by the scheduled deadline is answering no hint at all, and a
/// hint-driven scan that settled durable work is the live progress path: the
/// hint after it is the Session's next step, not a duplicate of the one just
/// served. Only a hint-driven scan that settled nothing evidences the burst
/// this window exists to collapse.
///
/// That scan earns a window of its own elapsed cost, capped at
/// [`EXTERNAL_WAKE_COOLDOWN`]. Charging a scan its own duration holds
/// hint-driven scanning to at most half this supervisor's time however fast
/// hints arrive, while the latency a later hint can inherit stays proportional
/// to what a scan actually costs instead of a fixed second of wall clock.
pub(super) fn next_external_wake_cooldown(
    trigger: SessionRecoveryTrigger,
    settled: usize,
    scan_elapsed: std::time::Duration,
) -> std::time::Duration {
    match trigger {
        SessionRecoveryTrigger::ExternalWake if settled == 0 => {
            scan_elapsed.min(EXTERNAL_WAKE_COOLDOWN)
        }
        _ => std::time::Duration::ZERO,
    }
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
    use super::{
        EXTERNAL_WAKE_COOLDOWN, SessionRecoveryTrigger, external_wake_coalescing_delay,
        next_external_wake_cooldown, next_session_recovery_delay, session_recovery_delay,
    };
    use std::time::Duration;

    /// @obligation session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
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

    /// @obligation session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
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

    /// @obligation session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
    #[kani::proof]
    fn external_wake_coalescing_is_bounded_and_never_postpones_recovery() {
        let seconds: u64 = kani::any();
        let nanos: u32 = kani::any();
        kani::assume(nanos < 1_000_000_000);
        let cooldown_seconds: u64 = kani::any();
        let cooldown_nanos: u32 = kani::any();
        kani::assume(cooldown_nanos < 1_000_000_000);
        let scheduled = Duration::new(seconds, nanos);
        let cooldown = Duration::new(cooldown_seconds, cooldown_nanos);
        let delay = external_wake_coalescing_delay(scheduled, cooldown);
        assert!(delay <= scheduled);
        assert!(delay <= cooldown);
        assert!(delay <= EXTERNAL_WAKE_COOLDOWN);
        assert_eq!(
            external_wake_coalescing_delay(Duration::MAX, Duration::ZERO),
            Duration::ZERO
        );
    }

    /// @obligation session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
    #[kani::proof]
    fn only_an_empty_hint_driven_scan_arms_the_coalescing_window() {
        let settled: usize = kani::any();
        let elapsed_seconds: u64 = kani::any();
        let elapsed_nanos: u32 = kani::any();
        kani::assume(elapsed_nanos < 1_000_000_000);
        let elapsed = Duration::new(elapsed_seconds, elapsed_nanos);
        assert_eq!(
            next_external_wake_cooldown(SessionRecoveryTrigger::Scheduled, settled, elapsed),
            Duration::ZERO
        );
        let armed =
            next_external_wake_cooldown(SessionRecoveryTrigger::ExternalWake, settled, elapsed);
        assert!(armed <= EXTERNAL_WAKE_COOLDOWN);
        assert!(armed <= elapsed);
        if settled != 0 {
            assert_eq!(armed, Duration::ZERO);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EXTERNAL_WAKE_COOLDOWN, SessionRecoveryTrigger, external_wake_coalescing_delay,
        next_external_wake_cooldown, next_session_recovery_delay, session_recovery_delay,
    };

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

    #[test]
    fn external_wake_bursts_share_one_bounded_coalescing_window() {
        /* Cause/effect graph: C1 the scheduled deadline is due/near/later;
         * C2 the post-scan cooldown is expired/active; C3 one or many external
         * hints arrive. Effects: E1 a due deadline remains immediate; E2 an
         * expired cooldown makes the first quiet-period hint immediate; E3 a
         * nearer scheduled deadline wins; E4 an active tail is bounded by one
         * second; E5 C3 cardinality cannot extend any bound. Decision table:
         * W1=(due,active)=>0; W2=(later,expired)=>0;
         * W3=(100ms,active)=>100ms; W4=(30s,active)=>1s. */
        use std::time::Duration;

        for (scheduled, cooldown, expected) in [
            (Duration::ZERO, Duration::from_secs(1), Duration::ZERO),
            (Duration::from_secs(30), Duration::ZERO, Duration::ZERO),
            (
                Duration::from_millis(100),
                Duration::from_secs(1),
                Duration::from_millis(100),
            ),
            (
                Duration::from_secs(30),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
        ] {
            assert_eq!(
                external_wake_coalescing_delay(scheduled, cooldown),
                expected
            );
        }
    }

    #[test]
    fn only_an_empty_hint_driven_scan_makes_the_next_hint_wait() {
        /* Cause/effect graph: C1 what reached this scan (scheduled deadline or
         * external hint); C2 whether the scan settled durable work. Effects:
         * E1 the next hint is served immediately; E2 the next hint waits out
         * the coalescing window. Decision table: A1=(scheduled,nothing)=>E1,
         * because a deadline scan answered no hint and evidences no burst;
         * A2=(scheduled,work)=>E1; A3=(hint,work)=>E1, because the hint that
         * follows live progress is the next step, not a duplicate;
         * A4=(hint,nothing)=>E2 for the scan's own elapsed cost, the one case
         * where the hint just served found nothing and its successors
         * plausibly repeat it; A5=(hint,nothing,scan longer than the cap)=>E2
         * capped at one second. A3 keeps an interactive Session off a per-step
         * wait, and the A4 charge keeps that wait proportional to what an empty
         * scan actually costs. */
        use std::time::Duration;

        let scan = Duration::from_millis(4);
        for (trigger, settled, elapsed, expected) in [
            (SessionRecoveryTrigger::Scheduled, 0, scan, Duration::ZERO),
            (SessionRecoveryTrigger::Scheduled, 7, scan, Duration::ZERO),
            (
                SessionRecoveryTrigger::ExternalWake,
                7,
                scan,
                Duration::ZERO,
            ),
            (SessionRecoveryTrigger::ExternalWake, 0, scan, scan),
            (
                SessionRecoveryTrigger::ExternalWake,
                0,
                Duration::from_secs(30),
                EXTERNAL_WAKE_COOLDOWN,
            ),
        ] {
            assert_eq!(
                next_external_wake_cooldown(trigger, settled, elapsed),
                expected
            );
        }
    }
}
