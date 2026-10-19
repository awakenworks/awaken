// @formal entrypoint runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed
//! Inference failure resilience: one classification and one retry decision.
//!
//! [`Error::class`] is the only projection of the inference error taxonomy
//! onto the retry domain. Everything that asks "may this be retried, and
//! when?" reads that projection:
//!
//! - [`Error::is_retryable`] (the circuit breaker's counted set and the
//!   checkpoint/salvage gate) is [`FailureClass::is_retryable`];
//! - the routing [`Disposition`] (failover / credential cooldown) is
//!   [`FailureClass::disposition`];
//! - the same-call retry loop asks [`RetryPolicy::decide`] once per failed
//!   attempt and obeys its [`RetryVerdict`].
//!
//! `Disposition` deliberately no longer answers "retry in place?": a 429 is
//! both a reason to cool this identity *and* a reason to retry the same call
//! after the provider's pause, and a second retry predicate is how the two
//! used to disagree.

use std::time::Duration;

use crate::llm::{Error, ModelInferenceBudget};
#[cfg(test)]
use crate::model_operation::ModelExecutionEvidence;
use crate::model_operation::ModelReentry;

/// The routing vocabulary lives beneath every contract that classifies
/// failures; the inference [`Error`] projection stays here with its owner.
pub use awaken_agent_contract::resilience::{Classify, Disposition};

/// What a failed inference says about trying again. Consumed by
/// [`RetryPolicy::decide`], [`Error::is_retryable`] and the routing
/// [`Disposition`]; produced only by [`Error::class`].
///
/// The names reuse the error taxonomy's own vocabulary (`rate_limited`,
/// `overloaded`, `usage_limit` codes; `Transient`/`Permanent` from
/// [`Disposition`]) so a class reads as the error family it summarizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The same call may succeed after a short pause: a 5xx, a timeout, a
    /// transport interruption or a terminal response the protocol rejected.
    Transient,
    /// The provider throttled this identity (429). `retry_after` is its
    /// requested pause when sent.
    RateLimited { retry_after: Option<Duration> },
    /// The provider lacks capacity (503/529). `retry_after` is its requested
    /// pause when sent.
    Overloaded { retry_after: Option<Duration> },
    /// A hard usage/quota exhaustion. It cannot clear inside a retry loop;
    /// `reset_after` is informational (routing cooldown only).
    UsageLimit { reset_after: Option<Duration> },
    /// This request shape cannot succeed on any retry of the same binding.
    Permanent,
    /// The caller cancelled the run. Says nothing about the provider.
    Cancelled,
}

impl FailureClass {
    /// Whether retrying the identical call can succeed at all. The circuit
    /// breaker counts exactly this set as provider-health failures.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::RateLimited { .. } | Self::Overloaded { .. }
        )
    }

    /// The provider's requested pause before the same call, when it sent one.
    #[must_use]
    pub const fn retry_after(self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } | Self::Overloaded { retry_after } => retry_after,
            _ => None,
        }
    }

    /// The provider's usage reset window, when it sent one.
    #[must_use]
    pub const fn reset_after(self) -> Option<Duration> {
        match self {
            Self::UsageLimit { reset_after } => reset_after,
            _ => None,
        }
    }

    /// The routing projection. A throttle, an overload and a hard usage limit
    /// are all signals against this identity/endpoint, so a router prefers
    /// another candidate and cools this one for the provider's hint.
    #[must_use]
    pub const fn disposition(self) -> Disposition {
        match self {
            Self::Transient => Disposition::Transient,
            Self::RateLimited { retry_after } | Self::Overloaded { retry_after } => {
                Disposition::Quota { retry_after }
            }
            Self::UsageLimit { reset_after } => Disposition::Quota {
                retry_after: reset_after,
            },
            Self::Permanent => Disposition::Permanent,
            // A cancellation is no evidence against this binding: neither
            // failover nor cooldown, which is exactly the Transient routing.
            Self::Cancelled => Disposition::Transient,
        }
    }
}

impl Error {
    /// The one projection of every error variant onto the retry domain.
    #[must_use]
    pub fn class(&self) -> FailureClass {
        match self.classification() {
            Error::Provider(_) | Error::InvalidResponse(_) | Error::Timeout(_) => {
                FailureClass::Transient
            }
            Error::RateLimited { retry_after, .. } => FailureClass::RateLimited {
                retry_after: *retry_after,
            },
            Error::Overloaded { retry_after, .. } => FailureClass::Overloaded {
                retry_after: *retry_after,
            },
            Error::UsageLimit { reset_after, .. } => FailureClass::UsageLimit {
                reset_after: *reset_after,
            },
            Error::Binding(_)
            | Error::ContextOverflow(_)
            | Error::InvalidRequest(_)
            | Error::Unauthorized(_)
            | Error::LoginRequired(_)
            | Error::ModelNotFound(_)
            | Error::ContentFiltered(_) => FailureClass::Permanent,
            Error::Cancelled(_) => FailureClass::Cancelled,
            // `classification()` never returns the evidence wrapper.
            Error::Observed { source, .. } => source.class(),
        }
    }
}

impl Classify for Error {
    fn disposition(&self) -> Disposition {
        self.class().disposition()
    }
}

/// The only non-terminal retry lifecycle state.  Numeric input is intentional:
/// persisted or protocol-produced unknown states must be representable and fail
/// closed instead of being coerced into another retry.
pub const RETRY_ACTIVE_STATE: u8 = 0;
/// Absorbing state after the caller has decided not to make another attempt.
pub const RETRY_TERMINAL_STATE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    Retry,
    Terminal,
}

/// Exact bounded-retry kernel inside [`RetryPolicy::decide`], proved by Kani.
/// Only a known active state, a retryable classification, and remaining
/// budget authorize another provider effect.  Terminal and unknown states are
/// absorbing/fail-closed.
#[must_use]
pub const fn bounded_retry_decision(
    state: u8,
    retryable: bool,
    failed_attempt: u32,
    max_retries: u32,
) -> RetryDecision {
    if state == RETRY_ACTIVE_STATE && retryable && failed_attempt < max_retries {
        RetryDecision::Retry
    } else {
        RetryDecision::Terminal
    }
}

/// Generic and overload backoff never exceeds this.
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// A provider that omits Retry-After still needs enough time for existing
/// long-running requests to release account concurrency. Derive the larger
/// base from the configured generic base so zero-delay tests remain hermetic.
const RATE_LIMIT_BACKOFF_MULTIPLIER: u64 = 30;
/// The default count bound of a rate-limited request (see
/// [`RetryPolicy::throttled_max_retries`]).
const DEFAULT_THROTTLED_MAX_RETRIES: usize = 12;
const RATE_LIMIT_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A server `Retry-After` longer than the computed backoff is adopted verbatim,
/// but never beyond this cap.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Why the retry loop stops. Named `RetryStop` because `llm::StopReason`
/// already names the model's own terminal stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryStop {
    /// The caller cancelled the run; no further attempt is wanted.
    Cancelled,
    /// The failure class cannot clear by retrying the identical call.
    NotRetryable,
    /// The attempt could not prove the provider stopped (or never started), so
    /// another physical call could duplicate its effect.
    ExecutionUncertain,
    /// The class is retryable but its retry count is spent.
    AttemptsExhausted,
    /// The pause plus the shortest useful attempt no longer fits before the
    /// request's logical deadline.
    DeadlineExhausted,
}

/// The one retry decision for a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryVerdict {
    /// Make the next attempt after this pause.
    Retry { after: Duration },
    /// Surface the failure.
    Stop(RetryStop),
}

/// How inference retries are paced. Held by the Runtime and read only by
/// [`decide`](Self::decide); tests inject a fast policy to keep suites quick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Extra attempts after the first failure (2 means up to 3 calls). Zero is
    /// an absolute disable for every class, a rate limit included.
    pub max_retries: usize,
    /// The retry count of a rate-limited request when retries are enabled:
    /// it gets `max(max_retries, throttled_max_retries)`, because a throttle
    /// clears with time and deserves more patience than a generic failure,
    /// yet an instant 429 with no pause must still stop after a bounded
    /// number of calls. The logical deadline bounds it too; whichever comes
    /// first stops the loop.
    pub throttled_max_retries: usize,
    /// Exponential backoff base for a generic retryable error.
    pub backoff_base_ms: u64,
    /// Backoff base when the provider reports overload — deliberately longer,
    /// since hammering an overloaded provider extends the outage.
    pub overloaded_backoff_base_ms: u64,
    /// The caller-declared time budget of one model request: its logical
    /// deadline across retries, each attempt's response window, the stream
    /// silence window and the watchdog's return allowance.
    pub budget: ModelInferenceBudget,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            throttled_max_retries: DEFAULT_THROTTLED_MAX_RETRIES,
            backoff_base_ms: 500,
            overloaded_backoff_base_ms: 2_000,
            budget: ModelInferenceBudget::DEFAULT,
        }
    }
}

impl RetryPolicy {
    /// Decide what follows failed attempt number `failed_attempt` (0-based).
    ///
    /// Rules, in order: a cancelled run stops (`Cancelled`); an attempt
    /// without reentry evidence stops (`ExecutionUncertain`); a class that
    /// cannot clear in place stops (`NotRetryable`); the bounded kernel admits
    /// the attempt count (`AttemptsExhausted` otherwise); the pause is the
    /// class's jittered exponential backoff, or the provider's requested pause
    /// when longer (capped); and the pause plus one silence window — the
    /// shortest attempt that can show any progress — must fit in `remaining`,
    /// the time left before the request's logical deadline
    /// (`DeadlineExhausted` otherwise). `jitter` is one uniform random draw,
    /// supplied by the caller so the decision itself is pure.
    #[must_use]
    pub fn decide(
        &self,
        class: FailureClass,
        reentry: ModelReentry,
        failed_attempt: u32,
        remaining: Duration,
        jitter: u64,
    ) -> RetryVerdict {
        if class == FailureClass::Cancelled {
            return RetryVerdict::Stop(RetryStop::Cancelled);
        }
        if !reentry.allows_request() {
            return RetryVerdict::Stop(RetryStop::ExecutionUncertain);
        }
        if !class.is_retryable() {
            return RetryVerdict::Stop(RetryStop::NotRetryable);
        }
        if bounded_retry_decision(
            RETRY_ACTIVE_STATE,
            true,
            failed_attempt,
            self.retry_limit(class),
        ) == RetryDecision::Terminal
        {
            return RetryVerdict::Stop(RetryStop::AttemptsExhausted);
        }
        let after = self.pause(class, failed_attempt, jitter);
        if after.saturating_add(self.budget.idle()) > remaining {
            return RetryVerdict::Stop(RetryStop::DeadlineExhausted);
        }
        RetryVerdict::Retry { after }
    }

    /// Keep the caller's explicit zero as an absolute retry disable. A rate
    /// limit otherwise gets the larger throttled count; every other class
    /// keeps the configured count. The logical deadline applies to all.
    fn retry_limit(&self, class: FailureClass) -> u32 {
        let limit = match class {
            FailureClass::RateLimited { .. } if self.max_retries > 0 => {
                self.max_retries.max(self.throttled_max_retries)
            }
            _ => self.max_retries,
        };
        u32::try_from(limit).unwrap_or(u32::MAX)
    }

    /// The jittered exponential backoff for the class, or the provider's
    /// requested pause when that is longer (adopted verbatim, capped at
    /// [`MAX_RETRY_AFTER`]). Rate limiting uses its own larger cap.
    fn pause(&self, class: FailureClass, failed_attempt: u32, jitter: u64) -> Duration {
        let (base, cap) = match class {
            FailureClass::RateLimited { .. } => (
                self.backoff_base_ms
                    .saturating_mul(RATE_LIMIT_BACKOFF_MULTIPLIER),
                RATE_LIMIT_MAX_BACKOFF,
            ),
            FailureClass::Overloaded { .. } => (self.overloaded_backoff_base_ms, MAX_BACKOFF),
            _ => (self.backoff_base_ms, MAX_BACKOFF),
        };
        let nominal =
            Duration::from_millis(base.saturating_mul(1u64 << failed_attempt.min(32))).min(cap);
        let backoff = equal_jitter(nominal, jitter);
        match class.retry_after() {
            Some(hint) if hint > backoff => hint.min(MAX_RETRY_AFTER),
            _ => backoff,
        }
    }
}

/// Equal jitter: a uniform draw from `[nominal/2, nominal]` (millisecond
/// grain), so synchronized clients spread out while every wait keeps at least
/// half the intended pause.
fn equal_jitter(nominal: Duration, draw: u64) -> Duration {
    let nominal_ms = u64::try_from(nominal.as_millis()).unwrap_or(u64::MAX);
    if nominal_ms == 0 {
        return Duration::ZERO;
    }
    let half = nominal_ms / 2;
    let span = nominal_ms - half;
    Duration::from_millis(half + draw % span.saturating_add(1))
}

#[cfg(kani)]
mod kani_proofs {
    use super::{RETRY_ACTIVE_STATE, RETRY_TERMINAL_STATE, RetryDecision, bounded_retry_decision};

    /// @obligation runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed
    #[kani::proof]
    fn retry_is_authorized_only_inside_the_exact_bounded_budget() {
        let retryable: bool = kani::any();
        let failed_attempt: u32 = kani::any();
        let max_retries: u32 = kani::any();
        let decision =
            bounded_retry_decision(RETRY_ACTIVE_STATE, retryable, failed_attempt, max_retries);
        assert_eq!(
            decision == RetryDecision::Retry,
            retryable && failed_attempt < max_retries
        );
        if decision == RetryDecision::Retry {
            assert!(failed_attempt.checked_add(1).is_some());
            assert!(failed_attempt + 1 <= max_retries);
        }
    }

    /// @obligation runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed
    #[kani::proof]
    fn non_retryable_failure_is_immediately_terminal() {
        let failed_attempt: u32 = kani::any();
        let max_retries: u32 = kani::any();
        assert_eq!(
            bounded_retry_decision(RETRY_ACTIVE_STATE, false, failed_attempt, max_retries,),
            RetryDecision::Terminal
        );
    }

    /// @obligation runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed
    #[kani::proof]
    fn terminal_retry_state_never_reopens() {
        let retryable: bool = kani::any();
        let failed_attempt: u32 = kani::any();
        let max_retries: u32 = kani::any();
        let first =
            bounded_retry_decision(RETRY_TERMINAL_STATE, retryable, failed_attempt, max_retries);
        assert_eq!(first, RetryDecision::Terminal);
        let second = bounded_retry_decision(RETRY_TERMINAL_STATE, true, failed_attempt, u32::MAX);
        assert_eq!(second, RetryDecision::Terminal);
    }

    /// @obligation runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed
    #[kani::proof]
    fn unknown_retry_state_fails_closed() {
        let state: u8 = kani::any();
        kani::assume(state != RETRY_ACTIVE_STATE && state != RETRY_TERMINAL_STATE);
        let retryable: bool = kani::any();
        let failed_attempt: u32 = kani::any();
        let max_retries: u32 = kani::any();
        assert_eq!(
            bounded_retry_decision(state, retryable, failed_attempt, max_retries),
            RetryDecision::Terminal
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model_operation::{NoDispatchProof, ResponseEnvelope, TerminalReceipt};

    const SECOND: Duration = Duration::from_secs(1);

    fn terminated() -> ModelExecutionEvidence {
        ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
            ResponseEnvelope::CompleteErrorBody,
        ))
    }

    #[test]
    fn every_error_variant_projects_one_class_decision_table() {
        // Cause: the error variant (and the evidence wrapper). Effects: E1 the
        // class, E2 is_retryable, E3 routing disposition, E4 retry/reset hint.
        // Rule: E2/E3/E4 are projections of E1, so a throttle is retryable in
        // place AND a quota signal for routing — the two can no longer
        // disagree.
        const HINT: Option<Duration> = Some(Duration::from_secs(30));
        type Row = (fn() -> Error, FailureClass, bool, Disposition);
        let rows: Vec<Row> = vec![
            (
                || Error::Provider("5xx".into()),
                FailureClass::Transient,
                true,
                Disposition::Transient,
            ),
            (
                || Error::InvalidResponse("bad".into()),
                FailureClass::Transient,
                true,
                Disposition::Transient,
            ),
            (
                || Error::Timeout("slow".into()),
                FailureClass::Transient,
                true,
                Disposition::Transient,
            ),
            (
                || Error::RateLimited {
                    message: "429".into(),
                    retry_after: HINT,
                },
                FailureClass::RateLimited { retry_after: HINT },
                true,
                Disposition::Quota { retry_after: HINT },
            ),
            (
                || Error::Overloaded {
                    message: "529".into(),
                    retry_after: None,
                },
                FailureClass::Overloaded { retry_after: None },
                true,
                Disposition::Quota { retry_after: None },
            ),
            (
                || Error::UsageLimit {
                    message: "weekly".into(),
                    reset_after: HINT,
                },
                FailureClass::UsageLimit { reset_after: HINT },
                false,
                Disposition::Quota { retry_after: HINT },
            ),
            (
                || Error::Binding("bad".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            (
                || Error::ContextOverflow("long".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            (
                || Error::InvalidRequest("400".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            (
                || Error::Unauthorized("401".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            (
                || Error::LoginRequired("expired".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            (
                || Error::ModelNotFound("404".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            (
                || Error::ContentFiltered("blocked".into()),
                FailureClass::Permanent,
                false,
                Disposition::Permanent,
            ),
            // A cancellation is not the binding's fault: no retry, and a
            // routing disposition that neither fails over nor cools.
            (
                || Error::Cancelled("run cancelled".into()),
                FailureClass::Cancelled,
                false,
                Disposition::Transient,
            ),
        ];
        for (error, class, retryable, disposition) in rows {
            let observed = error().with_execution_evidence(terminated());
            let error = error();
            for error in [error, observed] {
                assert_eq!(error.class(), class, "E1 {error:?}");
                assert_eq!(error.is_retryable(), retryable, "E2 {error:?}");
                assert_eq!(error.disposition(), disposition, "E3 {error:?}");
                assert_eq!(error.retry_after(), class.retry_after(), "E4 {error:?}");
                assert_eq!(error.reset_hint(), class.reset_after(), "E4 {error:?}");
            }
        }
    }

    #[test]
    fn retry_verdict_decision_table() {
        // Causes: C1 class (+ provider pause), C2 execution evidence, C3
        // failed attempt index, C4 policy (count budget), C5 time left before
        // the logical deadline, C6 jitter draw. Effect: the verdict. Rules:
        // R0 a cancelled run => Cancelled, whatever else holds; R1 no reentry
        // evidence => ExecutionUncertain; R2 a class that cannot clear in
        // place => NotRetryable; R3 count spent => AttemptsExhausted (a
        // throttle gets max(max_retries, throttled_max_retries = 12) unless
        // the caller disabled retries); R5 pause + one silence window
        // (default 300s) beyond the time left => DeadlineExhausted; a
        // throttle stops at whichever of R3/R5 comes first; R4 otherwise retry after the
        // equal-jitter exponential backoff (8s cap; throttle 30x base, 60s
        // cap) or the provider's longer pause (60s cap).
        let policy = RetryPolicy::default();
        let fast = RetryPolicy {
            max_retries: 50,
            ..RetryPolicy::default()
        };
        let disabled = RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        };
        let instant = RetryPolicy {
            backoff_base_ms: 0,
            overloaded_backoff_base_ms: 0,
            ..RetryPolicy::default()
        };
        let not_dispatched =
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::runtime_admission());
        let unknown = ModelExecutionEvidence::Unknown;
        let transient = FailureClass::Transient;
        let throttle = |retry_after| FailureClass::RateLimited { retry_after };
        let overload = |retry_after| FailureClass::Overloaded { retry_after };
        let ms = Duration::from_millis;
        let retry = |after| RetryVerdict::Retry { after };
        let stop = RetryVerdict::Stop;
        let idle = ModelInferenceBudget::DEFAULT.idle();
        let hour = 3_600 * SECOND;
        struct Row<'a> {
            name: &'static str,
            policy: &'a RetryPolicy,
            class: FailureClass,
            evidence: ModelExecutionEvidence,
            attempt: u32,
            remaining: Duration,
            jitter: u64,
            verdict: RetryVerdict,
        }
        #[rustfmt::skip]
        let rows = [
            Row { name: "R0 cancelled, unknown evidence", policy: &policy, class: FailureClass::Cancelled, evidence: unknown, attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::Cancelled) },
            Row { name: "R0 cancelled, budget left", policy: &fast, class: FailureClass::Cancelled, evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::Cancelled) },
            Row { name: "R1 unknown evidence", policy: &policy, class: transient, evidence: unknown, attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::ExecutionUncertain) },
            Row { name: "R1 beats a throttle", policy: &policy, class: throttle(Some(SECOND)), evidence: unknown, attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::ExecutionUncertain) },
            Row { name: "R2 permanent", policy: &policy, class: FailureClass::Permanent, evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::NotRetryable) },
            Row { name: "R2 usage limit", policy: &policy, class: FailureClass::UsageLimit { reset_after: Some(SECOND) }, evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::NotRetryable) },
            Row { name: "R4 first transient, low draw", policy: &policy, class: transient, evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: retry(ms(250)) },
            Row { name: "R4 first transient, high draw", policy: &policy, class: transient, evidence: terminated(), attempt: 0, remaining: hour, jitter: 250, verdict: retry(ms(500)) },
            Row { name: "R4 not dispatched", policy: &policy, class: transient, evidence: not_dispatched, attempt: 1, remaining: hour, jitter: 0, verdict: retry(ms(500)) },
            Row { name: "R3 transient count spent", policy: &policy, class: transient, evidence: terminated(), attempt: 2, remaining: hour, jitter: 0, verdict: stop(RetryStop::AttemptsExhausted) },
            Row { name: "R4 generic cap", policy: &fast, class: transient, evidence: terminated(), attempt: 40, remaining: hour, jitter: 4_000, verdict: retry(ms(8_000)) },
            Row { name: "R4 overload base", policy: &policy, class: overload(None), evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: retry(ms(1_000)) },
            Row { name: "R4 longer pause wins", policy: &policy, class: overload(Some(30 * SECOND)), evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: retry(30 * SECOND) },
            Row { name: "R4 pause is capped", policy: &policy, class: overload(Some(600 * SECOND)), evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: retry(60 * SECOND) },
            Row { name: "R4 shorter pause loses", policy: &policy, class: overload(Some(ms(1))), evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: retry(ms(1_000)) },
            Row { name: "R4 throttle base", policy: &policy, class: throttle(None), evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: retry(ms(7_500)) },
            Row { name: "R4 throttle cap", policy: &policy, class: throttle(None), evidence: terminated(), attempt: 8, remaining: hour, jitter: 30_000, verdict: retry(ms(60_000)) },
            Row { name: "R3 throttle count admits the 12th retry", policy: &policy, class: throttle(None), evidence: terminated(), attempt: 11, remaining: hour, jitter: 0, verdict: retry(ms(30_000)) },
            Row { name: "R3 throttle count spent", policy: &policy, class: throttle(None), evidence: terminated(), attempt: 12, remaining: hour, jitter: 0, verdict: stop(RetryStop::AttemptsExhausted) },
            Row { name: "R3 larger generic count wins for a throttle", policy: &fast, class: throttle(None), evidence: terminated(), attempt: 49, remaining: hour, jitter: 0, verdict: retry(ms(30_000)) },
            Row { name: "R3 overload keeps the generic count", policy: &policy, class: overload(None), evidence: terminated(), attempt: 2, remaining: hour, jitter: 0, verdict: stop(RetryStop::AttemptsExhausted) },
            Row { name: "R3 instant 429, zero backoff: retried at once", policy: &instant, class: throttle(None), evidence: terminated(), attempt: 11, remaining: hour, jitter: 0, verdict: retry(Duration::ZERO) },
            Row { name: "R3 instant 429, zero backoff: count stops it first", policy: &instant, class: throttle(None), evidence: terminated(), attempt: 12, remaining: hour, jitter: 0, verdict: stop(RetryStop::AttemptsExhausted) },
            Row { name: "R5 long backoff: deadline stops it before the count", policy: &policy, class: throttle(Some(60 * SECOND)), evidence: terminated(), attempt: 3, remaining: 5 * 60 * SECOND, jitter: 0, verdict: stop(RetryStop::DeadlineExhausted) },
            Row { name: "R3 explicit zero disables", policy: &disabled, class: throttle(None), evidence: terminated(), attempt: 0, remaining: hour, jitter: 0, verdict: stop(RetryStop::AttemptsExhausted) },
            Row { name: "R5 pause + silence fits exactly", policy: &policy, class: throttle(Some(60 * SECOND)), evidence: terminated(), attempt: 0, remaining: 60 * SECOND + idle, jitter: 0, verdict: retry(60 * SECOND) },
            Row { name: "R5 pause + silence overruns", policy: &policy, class: throttle(Some(60 * SECOND)), evidence: terminated(), attempt: 0, remaining: 60 * SECOND + idle - ms(1), jitter: 0, verdict: stop(RetryStop::DeadlineExhausted) },
            Row { name: "R5 deadline spent", policy: &policy, class: transient, evidence: not_dispatched, attempt: 0, remaining: Duration::ZERO, jitter: 0, verdict: stop(RetryStop::DeadlineExhausted) },
        ];
        for row in rows {
            assert_eq!(
                row.policy.decide(
                    row.class,
                    ModelReentry::observed(
                        crate::model_operation::ModelOperationPhase::Returned,
                        crate::model_operation::ModelEffectScope::Opaque,
                        row.evidence,
                    ),
                    row.attempt,
                    row.remaining,
                    row.jitter
                ),
                row.verdict,
                "{}",
                row.name
            );
        }
    }

    #[test]
    fn equal_jitter_stays_within_half_to_full_of_the_nominal_pause() {
        // Causes: nominal pause, arbitrary draw. Effect: the pause lies in
        // [nominal/2, nominal]; a zero nominal never divides by zero.
        assert_eq!(equal_jitter(Duration::ZERO, u64::MAX), Duration::ZERO);
        for nominal in [1u64, 2, 3, 500, 8_000, 60_000] {
            for draw in [0, 1, 7, u64::MAX / 3, u64::MAX] {
                let pause = equal_jitter(Duration::from_millis(nominal), draw).as_millis() as u64;
                assert!(
                    (nominal / 2..=nominal).contains(&pause),
                    "{nominal}/{draw}: {pause}"
                );
            }
        }
    }

    #[test]
    fn returned_response_only_retry_keeps_cancellation_count_and_deadline_gates() {
        use crate::model_operation::{ModelEffectScope, ModelOperationPhase};
        // R1 qualified returned uncertainty => fresh request only, never output
        // salvage. Cross C1 cancellation/permanent/transient with C2 retry
        // count and C3 logical deadline: R2 cancellation, R3 permanent refusal,
        // R4 spent count and R5 spent deadline all still stop. No remote receipt
        // is created, and this table invokes the original RetryPolicy owner.
        let reentry = ModelReentry::observed(
            ModelOperationPhase::Returned,
            ModelEffectScope::ResponseOnly,
            ModelExecutionEvidence::Unknown,
        );
        assert!(reentry.allows_request());
        assert!(!reentry.allows_retained_output());
        let p = RetryPolicy {
            backoff_base_ms: 0,
            ..RetryPolicy::default()
        };
        for (class, count, remaining, expected) in [
            (
                FailureClass::Transient,
                0,
                Duration::from_secs(3600),
                RetryVerdict::Retry {
                    after: Duration::ZERO,
                },
            ),
            (
                FailureClass::Cancelled,
                0,
                Duration::from_secs(3600),
                RetryVerdict::Stop(RetryStop::Cancelled),
            ),
            (
                FailureClass::Permanent,
                0,
                Duration::from_secs(3600),
                RetryVerdict::Stop(RetryStop::NotRetryable),
            ),
            (
                FailureClass::Transient,
                2,
                Duration::from_secs(3600),
                RetryVerdict::Stop(RetryStop::AttemptsExhausted),
            ),
            (
                FailureClass::Transient,
                0,
                Duration::ZERO,
                RetryVerdict::Stop(RetryStop::DeadlineExhausted),
            ),
        ] {
            assert_eq!(p.decide(class, reentry, count, remaining, 0), expected);
        }
    }
}
