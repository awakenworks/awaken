//! Process-local timing policy for proving a durable lease remains owned.
//!
//! The durable store remains the only lease authority. This value object only
//! derives a conservative renewal cadence, bounded request/retry timing, and a
//! local proof deadline from the authority-provided TTL. Worker-incarnation and
//! Run-claim supervisors share it so the two distinct aggregates cannot drift
//! into different liveness arithmetic.

use std::time::Duration;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
