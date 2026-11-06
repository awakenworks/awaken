// @formal surface obligation=session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal reason: The existing neutral timing policy now owns request-anchored elapsed arithmetic.
// @formal reason: WorkerExecutionAuthority abstracts expiry and regressed response rejection; adjacent
// @formal reason: Rust decision tables check exact submillisecond subtraction and scheduler independence.
// @formal reason: No lease, clock I/O, cross-host clock correspondence, or Rust refinement is claimed.
//! Process-local timing policy for proving a durable lease remains owned.
//!
//! The durable store remains the only lease authority. This value object only
//! derives a conservative renewal cadence, bounded request/retry timing, and a
//! local proof deadline from the authority-provided TTL. Worker-incarnation and
//! Run-claim supervisors share it so the two distinct aggregates cannot drift
//! into different liveness arithmetic.
//!
//! It also owns the one renewal law both supervisors apply to every attempt
//! (the Worker's initial heartbeat included): a renewal request may run for the
//! whole remaining proof, a reply that arrives inside it is applied, an
//! unanswered request is retried while proof remains, and only an exhausted
//! proof or an explicit refusal ends local authority. Ending authority is never
//! a durable cancellation; see [`AuthorityEnd`].

use awaken_time::mono::Instant;
use std::time::Duration;

/// Process-local observation taken before an authority request. The platform
/// adapter supplies the elapsed clock; this value object performs no clock I/O,
/// owns no lease, and is deliberately not serializable. Scheduling Instant and
/// authority elapsed time need not advance together (for example on suspend).
///
/// The two halves have different owners on purpose. `scheduled` only paces
/// the next attempt and reports latency, so it is the one monotonic scheduling
/// point, `awaken_time::mono` (a paused test runtime controls it).
/// `elapsed` is the suspend-inclusive authority clock
/// (`awaken-authority-clock`, BOOTTIME on Linux): it alone is compared with the
/// physical cutoff, and nothing here substitutes a scheduling reading for it.
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

/// What one renewal request (a Worker heartbeat or a Run-claim renewal)
/// returned to its supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewalReply {
    /// The authority applied the renewal.
    Applied,
    /// The authority explicitly refused it: this owner no longer holds the lease.
    Refused,
    /// No answer: a transport error, or the request ran out of remaining proof.
    Unanswered,
}

/// Why process-local authority ended. Neither cause is a durable cancellation:
/// the supervisor stops and abandons the work bound to the lost authority
/// without committing a terminal outcome for it, so the durable owner's
/// recovery decides what happens to that work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityEnd {
    /// The authority refused a renewal; another owner may already hold it.
    AuthorityLost,
    /// No renewal was applied before the conservative local proof ran out.
    ProofExpired,
}

/// The supervisor's next step after one renewal reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewalDecision {
    /// Apply the receipt, anchored at the request start.
    Apply,
    /// The previous proof still covers the work: retry after this delay.
    RetryAfter(Duration),
    /// Stop and abandon; never commit the bound work as cancelled.
    Stop(AuthorityEnd),
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

    /// The nominal request budget: the bound for auxiliary requests (probes,
    /// acknowledgements) and the delivery term of publication budgets. A
    /// renewal that outlives it is slow, not lost; renewal requests are bounded
    /// by [`Self::renewal_request_budget`] instead.
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

    /// How long one renewal request may run: the whole remaining proof, never
    /// more than one proof window. A reply inside it can still be applied, so
    /// cutting the request short would only discard a valid renewal. `None`
    /// means the proof is exhausted and no request may be started.
    #[must_use]
    pub fn renewal_request_budget(self, remaining_proof: Duration) -> Option<Duration> {
        (!remaining_proof.is_zero()).then_some(remaining_proof.min(self.proof_window))
    }

    /// The one renewal law, applied to `reply` with the previous proof's
    /// remainder observed when the reply arrived.
    ///
    /// | Rule | Reply | Previous proof left | Decision |
    /// | R1 | Applied | > 0 | Apply (a slow reply inside the proof counts) |
    /// | R2 | Applied | 0 | Stop(ProofExpired) |
    /// | R3 | Unanswered | > 0 | RetryAfter(min(retry delay, proof left)) |
    /// | R4 | Unanswered | 0 | Stop(ProofExpired) |
    /// | R5 | Refused | any | Stop(AuthorityLost) |
    #[must_use]
    pub fn decide_renewal(self, remaining_proof: Duration, reply: RenewalReply) -> RenewalDecision {
        match reply {
            RenewalReply::Refused => RenewalDecision::Stop(AuthorityEnd::AuthorityLost),
            _ if remaining_proof.is_zero() => RenewalDecision::Stop(AuthorityEnd::ProofExpired),
            RenewalReply::Applied => RenewalDecision::Apply,
            RenewalReply::Unanswered => {
                RenewalDecision::RetryAfter(self.retry_delay.min(remaining_proof))
            }
        }
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
        let scheduled = awaken_time::mono::now();
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
        let scheduled = awaken_time::mono::now();
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
    fn a_renewal_request_may_use_the_whole_remaining_proof() {
        // Cause: the previous proof's remainder when a renewal request starts
        // (Worker heartbeat, initial heartbeat or Run-claim renewal).
        // | Rule | TTL | proof left | budget |
        // | B1 | 30_000ms | 20s (fresh) | 20s, not the 5s nominal request bound |
        // | B2 | 30_000ms | 10s (regular renewal point) | 10s |
        // | B3 | 30_000ms | 1us | 1us |
        // | B4 | 30_000ms | 0 | None: no request may start |
        // | B5 | 30_000ms | 25s (corrupt caller) | 20s, never past one proof window |
        let timing = AuthorityLeaseTiming::from_ttl_ms(30_000);
        for (rule, remaining, budget) in [
            ("B1", Duration::from_secs(20), Some(Duration::from_secs(20))),
            ("B2", Duration::from_secs(10), Some(Duration::from_secs(10))),
            (
                "B3",
                Duration::from_micros(1),
                Some(Duration::from_micros(1)),
            ),
            ("B4", Duration::ZERO, None),
            ("B5", Duration::from_secs(25), Some(Duration::from_secs(20))),
        ] {
            assert_eq!(timing.renewal_request_budget(remaining), budget, "{rule}");
        }
        assert!(
            timing
                .renewal_request_budget(Duration::from_secs(20))
                .unwrap()
                > timing.request_timeout(),
            "B1: the nominal request bound no longer cuts a renewal short"
        );
    }

    #[test]
    fn renewal_decision_table_is_exhaustive() {
        // Every reply crossed with an exhausted, a tiny and a large previous
        // proof (rows R1-R5 of `decide_renewal`). Fail-closed rows: R2, R4, R5.
        // R1 is the slow-reply row: a reply after the 5s nominal bound with
        // proof left is applied. R3 is the initial-heartbeat row: an
        // unanswered first heartbeat with registration proof left retries.
        let timing = AuthorityLeaseTiming::from_ttl_ms(30_000);
        let tiny = Duration::from_millis(300);
        let large = Duration::from_secs(14);
        for (rule, reply, remaining, decision) in [
            ("R1", RenewalReply::Applied, tiny, RenewalDecision::Apply),
            ("R1", RenewalReply::Applied, large, RenewalDecision::Apply),
            (
                "R2",
                RenewalReply::Applied,
                Duration::ZERO,
                RenewalDecision::Stop(AuthorityEnd::ProofExpired),
            ),
            (
                "R3",
                RenewalReply::Unanswered,
                tiny,
                RenewalDecision::RetryAfter(tiny),
            ),
            (
                "R3",
                RenewalReply::Unanswered,
                large,
                RenewalDecision::RetryAfter(Duration::from_secs(1)),
            ),
            (
                "R4",
                RenewalReply::Unanswered,
                Duration::ZERO,
                RenewalDecision::Stop(AuthorityEnd::ProofExpired),
            ),
            (
                "R5",
                RenewalReply::Refused,
                Duration::ZERO,
                RenewalDecision::Stop(AuthorityEnd::AuthorityLost),
            ),
            (
                "R5",
                RenewalReply::Refused,
                tiny,
                RenewalDecision::Stop(AuthorityEnd::AuthorityLost),
            ),
            (
                "R5",
                RenewalReply::Refused,
                large,
                RenewalDecision::Stop(AuthorityEnd::AuthorityLost),
            ),
        ] {
            assert_eq!(timing.decide_renewal(remaining, reply), decision, "{rule}");
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
