//! Process-local timing policy for proving a durable lease remains owned.
//!
//! The durable store remains the only lease authority. This value object only
//! derives a conservative renewal cadence, bounded request/retry timing, and a
//! local proof deadline from the authority-provided TTL. Worker-incarnation and
//! Run-claim supervisors share it so the two distinct aggregates cannot drift
//! into different liveness arithmetic.

use std::time::{Duration, Instant};

/// Process-local observation taken before an authority request. The platform
/// adapter supplies the elapsed clock; this value object performs no clock I/O,
/// owns no lease, and is deliberately not serializable. Scheduling Instant and
/// authority elapsed time need not advance together (for example on suspend).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorityProofStarted {
    scheduled: Instant,
    elapsed: Duration,
}

impl AuthorityProofStarted {
    #[must_use]
    pub const fn from_clock_sample(scheduled: Instant, elapsed: Duration) -> Self {
        Self { scheduled, elapsed }
    }

    #[must_use]
    pub const fn scheduled(self) -> Instant {
        self.scheduled
    }

    #[must_use]
    pub const fn elapsed_clock(self) -> Duration {
        self.elapsed
    }

    /// Regression is missing evidence, never a zero-latency observation.
    #[must_use]
    pub fn elapsed_at(self, now: Self) -> Option<Duration> {
        now.elapsed.checked_sub(self.elapsed)
    }
}

/// Validated timing derived from one positive durable lease TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorityLeaseTiming {
    lease_ttl: Duration,
    renew_interval: Duration,
    request_timeout: Duration,
    retry_delay: Duration,
    proof_window: Duration,
}

impl AuthorityLeaseTiming {
    /// Derive three renewal opportunities and retain one interval of safety
    /// before the durable lease may be reclaimed.
    #[must_use]
    pub fn from_ttl_ms(lease_ttl_ms: u64) -> Self {
        let renew_interval_ms = (lease_ttl_ms / 3).max(1);
        Self {
            lease_ttl: Duration::from_millis(lease_ttl_ms.max(1)),
            renew_interval: Duration::from_millis(renew_interval_ms),
            request_timeout: Duration::from_millis((renew_interval_ms / 2).max(1)),
            retry_delay: Duration::from_millis((renew_interval_ms / 10).clamp(1, 1_000)),
            proof_window: Duration::from_millis(
                lease_ttl_ms.saturating_sub(renew_interval_ms).max(1),
            ),
        }
    }

    #[must_use]
    pub const fn lease_ttl(self) -> Duration {
        self.lease_ttl
    }

    #[must_use]
    pub const fn renew_interval(self) -> Duration {
        self.renew_interval
    }

    #[must_use]
    pub const fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    #[must_use]
    pub const fn retry_delay(self) -> Duration {
        self.retry_delay
    }

    #[must_use]
    pub const fn proof_window(self) -> Duration {
        self.proof_window
    }

    /// Remaining conservative proof after local request/response latency. The
    /// receipt cannot authorize time spent waiting for that receipt.
    #[must_use]
    pub fn remaining_proof_after(self, request_elapsed: Duration) -> Duration {
        self.proof_window.saturating_sub(request_elapsed)
    }

    /// Compose the same timing policy with a request-anchored clock sample.
    /// Neither a regressed clock nor an exhausted proof grants any lifetime.
    #[must_use]
    pub fn remaining_proof_at(
        self,
        started: AuthorityProofStarted,
        now: AuthorityProofStarted,
    ) -> Option<Duration> {
        let remaining = self.remaining_proof_after(started.elapsed_at(now)?);
        (!remaining.is_zero()).then_some(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_observation_uses_elapsed_clock_and_rejects_regression() {
        // C1 elapsed clock equal/forward/backward; C2 scheduler unchanged/forward.
        // E1 exact submillisecond elapsed for equal/forward C1, regardless of C2;
        // E2 None for backward C1, never saturating into renewed authority.
        // This pure decision table is platform-neutral; the Local process
        // adapter separately checks that Linux samples the existing BOOTTIME.
        let scheduled = Instant::now();
        let start = AuthorityProofStarted::from_clock_sample(scheduled, Duration::from_secs(1));
        for scheduling in [Duration::ZERO, Duration::from_secs(2)] {
            for (elapsed, expected) in [
                (Duration::from_micros(999_999), None),
                (Duration::from_secs(1), Some(Duration::ZERO)),
                (
                    Duration::from_micros(1_019_999),
                    Some(Duration::from_micros(19_999)),
                ),
            ] {
                let now = AuthorityProofStarted::from_clock_sample(scheduled + scheduling, elapsed);
                assert_eq!(start.elapsed_at(now), expected);
                assert_eq!(now.scheduled(), scheduled + scheduling);
                assert_eq!(now.elapsed_clock(), elapsed);
            }
        }
    }

    #[test]
    fn anchored_proof_never_grants_latency_expiry_or_clock_regression() {
        // P1 TTL=30ms yields 20ms proof; P2 elapsed is below/equal/above that
        // boundary; P3 elapsed clock regresses; P4 scheduler does not advance.
        // Only P1+below yields the exact remainder. Equal/above/regression deny;
        // P4 never weakens that decision. No float conversion or upward rounding.
        let timing = AuthorityLeaseTiming::from_ttl_ms(30);
        let scheduled = Instant::now();
        let started = AuthorityProofStarted::from_clock_sample(scheduled, Duration::from_secs(1));
        for (elapsed_micros, remaining_micros) in [
            (999_999, None),
            (1_000_000, Some(20_000)),
            (1_019_999, Some(1)),
            (1_020_000, None),
            (1_020_001, None),
        ] {
            let now = AuthorityProofStarted::from_clock_sample(
                scheduled,
                Duration::from_micros(elapsed_micros),
            );
            assert_eq!(
                timing.remaining_proof_at(started, now),
                remaining_micros.map(Duration::from_micros)
            );
        }
    }

    #[test]
    fn timing_is_total_and_preserves_a_recovery_margin() {
        // Cause/effect decision table. C1 is a normal 30s authority TTL; C2 is
        // the smallest/corrupt zero input that a defensive caller can present.
        // E1 provides three regular opportunities, a half-interval request
        // bound, and one full interval of recovery margin; E2 clamps every
        // duration positive without overflow or a zero-duration busy loop. C3
        // is a 5s delayed authority response; E3 consumes that latency instead
        // of extending the durable proof deadline in the caller.
        // | Rule | TTL | Effect |
        // | L1 | 30_000ms | interval=10s, timeout=5s, retry=1s, proof=20s |
        // | L2 | 0ms | every derived duration is at least 1ms |
        // | L3 | 30_000ms + 5s response latency | remaining proof=15s |
        let normal = AuthorityLeaseTiming::from_ttl_ms(30_000);
        assert_eq!(normal.lease_ttl(), Duration::from_secs(30), "L1");
        assert_eq!(normal.renew_interval(), Duration::from_secs(10), "L1");
        assert_eq!(normal.request_timeout(), Duration::from_secs(5), "L1");
        assert_eq!(normal.retry_delay(), Duration::from_secs(1), "L1");
        assert_eq!(normal.proof_window(), Duration::from_secs(20), "L1");

        let minimum = AuthorityLeaseTiming::from_ttl_ms(0);
        for duration in [
            minimum.lease_ttl(),
            minimum.renew_interval(),
            minimum.request_timeout(),
            minimum.retry_delay(),
            minimum.proof_window(),
        ] {
            assert!(duration >= Duration::from_millis(1), "L2");
        }
        assert_eq!(
            normal.remaining_proof_after(Duration::from_secs(5)),
            Duration::from_secs(15),
            "L3"
        );
        assert_eq!(
            normal.remaining_proof_after(Duration::from_secs(25)),
            Duration::ZERO,
            "L3 latency cannot underflow into new authority"
        );
    }
}
