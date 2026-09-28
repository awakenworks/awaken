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

use crate::llm::{DEFAULT_MODEL_ATTEMPT_WATCHDOG, Error};
use crate::model_operation::ModelExecutionEvidence;

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
const RATE_LIMIT_MAX_BACKOFF: Duration = Duration::from_secs(60);
const RATE_LIMIT_MIN_RETRIES: usize = 12;

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
    /// The class is retryable but the attempt budget is spent.
    AttemptsExhausted,
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
    /// Extra attempts after the first failure (2 means up to 3 calls). A
    /// nonzero budget enables the bounded rate-limit-specific floor; zero
    /// remains an absolute disable.
    pub max_retries: usize,
    /// Exponential backoff base for a generic retryable error.
    pub backoff_base_ms: u64,
    /// Backoff base when the provider reports overload — deliberately longer,
    /// since hammering an overloaded provider extends the outage.
    pub overloaded_backoff_base_ms: u64,
    /// Hard ceiling for one provider attempt, including its complete stream.
    pub attempt_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_base_ms: 500,
            overloaded_backoff_base_ms: 2_000,
            attempt_timeout: DEFAULT_MODEL_ATTEMPT_WATCHDOG,
        }
    }
}

impl RetryPolicy {
    /// Decide what follows failed attempt number `failed_attempt` (0-based).
    ///
    /// Rules, in order: an attempt without reentry evidence stops
    /// (`ExecutionUncertain`); a class that cannot clear in place stops
    /// (`NotRetryable`); the bounded kernel admits the attempt budget
    /// (`AttemptsExhausted` otherwise); the pause is the class's jittered
    /// exponential backoff, or the provider's requested pause when longer
    /// (capped). `jitter` is one uniform random draw, supplied by the caller
    /// so the decision itself is pure.
    #[must_use]
    pub fn decide(
        &self,
        class: FailureClass,
        evidence: ModelExecutionEvidence,
        failed_attempt: u32,
        jitter: u64,
    ) -> RetryVerdict {
        if class == FailureClass::Cancelled {
            return RetryVerdict::Stop(RetryStop::Cancelled);
        }
        if !evidence.permits_reentry() {
            return RetryVerdict::Stop(RetryStop::ExecutionUncertain);
        }
        if !class.is_retryable() {
            return RetryVerdict::Stop(RetryStop::NotRetryable);
        }
        let limit = u32::try_from(self.retry_limit(class)).unwrap_or(u32::MAX);
        match bounded_retry_decision(RETRY_ACTIVE_STATE, true, failed_attempt, limit) {
            RetryDecision::Retry => RetryVerdict::Retry {
                after: self.pause(class, failed_attempt, jitter),
            },
            RetryDecision::Terminal => RetryVerdict::Stop(RetryStop::AttemptsExhausted),
        }
    }

    /// Keep the caller's explicit zero as an absolute retry disable. Otherwise
    /// an exact rate-limit response gets a longer bounded availability budget;
    /// all other classes retain the configured generic budget.
    fn retry_limit(&self, class: FailureClass) -> usize {
        if self.max_retries > 0 && matches!(class, FailureClass::RateLimited { .. }) {
            self.max_retries.max(RATE_LIMIT_MIN_RETRIES)
        } else {
            self.max_retries
        }
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

    #[kani::proof]
    fn non_retryable_failure_is_immediately_terminal() {
        let failed_attempt: u32 = kani::any();
        let max_retries: u32 = kani::any();
        assert_eq!(
            bounded_retry_decision(RETRY_ACTIVE_STATE, false, failed_attempt, max_retries,),
            RetryDecision::Terminal
        );
    }

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
        // failed attempt index, C4 policy budget, C5 jitter draw. Effect: the
        // verdict. Rules: R0 a cancelled run => Cancelled, whatever else
        // holds; R1 no reentry evidence => ExecutionUncertain; R2 a
        // class that cannot clear in place => NotRetryable; R3 budget spent
        // => AttemptsExhausted (a throttle gets at least 12 retries unless the
        // caller disabled retries); R4 otherwise retry after the equal-jitter
        // exponential backoff (8s cap; throttle 30x base, 60s cap) or the
        // provider's longer pause (60s cap).
        let policy = RetryPolicy::default();
        let fast = RetryPolicy {
            max_retries: 50,
            ..RetryPolicy::default()
        };
        let disabled = RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        };
        let not_dispatched =
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::runtime_admission());
        let throttle = |retry_after| FailureClass::RateLimited { retry_after };
        let overload = |retry_after| FailureClass::Overloaded { retry_after };
        let ms = Duration::from_millis;
        let retry = |after| RetryVerdict::Retry { after };
        let stop = RetryVerdict::Stop;
        type Row<'a> = (
            &'static str,
            &'a RetryPolicy,
            FailureClass,
            ModelExecutionEvidence,
            u32,
            u64,
            RetryVerdict,
        );
        let rows: &[Row<'_>] = &[
            (
                "R1 unknown evidence",
                &policy,
                FailureClass::Transient,
                ModelExecutionEvidence::Unknown,
                0,
                0,
                stop(RetryStop::ExecutionUncertain),
            ),
            (
                "R0 cancelled, unknown evidence",
                &policy,
                FailureClass::Cancelled,
                ModelExecutionEvidence::Unknown,
                0,
                0,
                stop(RetryStop::Cancelled),
            ),
            (
                "R0 cancelled with budget left",
                &fast,
                FailureClass::Cancelled,
                terminated(),
                0,
                0,
                stop(RetryStop::Cancelled),
            ),
            (
                "R1 beats a throttle",
                &policy,
                throttle(Some(SECOND)),
                ModelExecutionEvidence::Unknown,
                0,
                0,
                stop(RetryStop::ExecutionUncertain),
            ),
            (
                "R2 permanent",
                &policy,
                FailureClass::Permanent,
                terminated(),
                0,
                0,
                stop(RetryStop::NotRetryable),
            ),
            (
                "R2 usage limit",
                &policy,
                FailureClass::UsageLimit {
                    reset_after: Some(SECOND),
                },
                terminated(),
                0,
                0,
                stop(RetryStop::NotRetryable),
            ),
            (
                "R4 first transient, low draw",
                &policy,
                FailureClass::Transient,
                terminated(),
                0,
                0,
                retry(ms(250)),
            ),
            (
                "R4 first transient, high draw",
                &policy,
                FailureClass::Transient,
                terminated(),
                0,
                250,
                retry(ms(500)),
            ),
            (
                "R4 not dispatched",
                &policy,
                FailureClass::Transient,
                not_dispatched,
                1,
                0,
                retry(ms(500)),
            ),
            (
                "R3 transient budget spent",
                &policy,
                FailureClass::Transient,
                terminated(),
                2,
                0,
                stop(RetryStop::AttemptsExhausted),
            ),
            (
                "R4 generic cap",
                &fast,
                FailureClass::Transient,
                terminated(),
                40,
                4_000,
                retry(ms(8_000)),
            ),
            (
                "R4 overload base",
                &policy,
                overload(None),
                terminated(),
                0,
                0,
                retry(ms(1_000)),
            ),
            (
                "R4 longer pause wins",
                &policy,
                overload(Some(30 * SECOND)),
                terminated(),
                0,
                0,
                retry(30 * SECOND),
            ),
            (
                "R4 pause is capped",
                &policy,
                overload(Some(600 * SECOND)),
                terminated(),
                0,
                0,
                retry(60 * SECOND),
            ),
            (
                "R4 shorter pause loses",
                &policy,
                overload(Some(ms(1))),
                terminated(),
                0,
                0,
                retry(ms(1_000)),
            ),
            (
                "R4 throttle base",
                &policy,
                throttle(None),
                terminated(),
                0,
                0,
                retry(ms(7_500)),
            ),
            (
                "R4 throttle cap",
                &policy,
                throttle(None),
                terminated(),
                8,
                30_000,
                retry(ms(60_000)),
            ),
            (
                "R3 throttle floor admits 12",
                &policy,
                throttle(None),
                terminated(),
                11,
                0,
                retry(ms(30_000)),
            ),
            (
                "R3 throttle floor spent",
                &policy,
                throttle(None),
                terminated(),
                12,
                0,
                stop(RetryStop::AttemptsExhausted),
            ),
            (
                "R3 explicit zero disables",
                &disabled,
                throttle(None),
                terminated(),
                0,
                0,
                stop(RetryStop::AttemptsExhausted),
            ),
        ];
        for (row, policy, class, evidence, attempt, jitter, verdict) in rows.iter().copied() {
            assert_eq!(
                policy.decide(class, evidence, attempt, jitter),
                verdict,
                "{row}"
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
}
