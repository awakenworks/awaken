//! Retry policy for inference calls.
//!
//! Exponential backoff with equal jitter, a longer base for provider overload,
//! and the server's `Retry-After` hint honored when it is longer than the
//! computed backoff — capped so a misconfigured or hostile header cannot await
//! the loop indefinitely. Permanent errors never reach this policy: the caller
//! checks `Error::is_retryable()` first.

use std::time::Duration;

use awaken_runtime_contract::llm::DEFAULT_MODEL_ATTEMPT_WATCHDOG;
use awaken_runtime_contract::llm::Error;
use awaken_runtime_contract::resilience::Classify;

/// Generic and overload backoff never exceeds this.
const MAX_BACKOFF_MS: u64 = 8_000;

/// A provider that omits Retry-After still needs enough time for existing
/// long-running requests to release account concurrency. Derive the larger
/// base from the configured generic base so zero-delay tests remain hermetic.
const RATE_LIMIT_BACKOFF_MULTIPLIER: u64 = 30;
const RATE_LIMIT_MAX_BACKOFF_MS: u64 = 60_000;
const RATE_LIMIT_MIN_RETRIES: usize = 12;

/// A server `Retry-After` longer than the computed backoff is adopted verbatim,
/// but never beyond this cap.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How inference retries are paced. Held by the `Runtime` and applied by the
/// engine's inference call; tests inject a fast policy to keep suites quick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRetryPolicy {
    /// Extra attempts after the first failure (2 means up to 3 calls). A
    /// nonzero budget enables the bounded rate-limit-specific floor below;
    /// zero remains an absolute disable.
    pub max_retries: usize,
    /// Exponential backoff base for a generic retryable error.
    pub backoff_base_ms: u64,
    /// Backoff base when the provider reports overload — deliberately longer,
    /// since hammering an overloaded provider extends the outage.
    pub overloaded_backoff_base_ms: u64,
    /// Hard ceiling for one provider attempt, including its complete stream.
    pub attempt_timeout: Duration,
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_base_ms: 500,
            overloaded_backoff_base_ms: 2_000,
            attempt_timeout: DEFAULT_MODEL_ATTEMPT_WATCHDOG,
        }
    }
}

impl LlmRetryPolicy {
    /// Keep the caller's explicit zero as an absolute retry disable. Otherwise
    /// an exact rate-limit response gets a longer bounded availability budget;
    /// all other classes retain the configured generic budget.
    pub(crate) fn retry_limit(&self, err: &Error) -> usize {
        if self.max_retries > 0 && matches!(err.classification(), Error::RateLimited { .. }) {
            self.max_retries.max(RATE_LIMIT_MIN_RETRIES)
        } else {
            self.max_retries
        }
    }

    /// How long to wait before retry number `retry` (0-based): the jittered
    /// exponential backoff for the error class, or the server's `Retry-After`
    /// when that is longer (adopted verbatim, capped at [`MAX_RETRY_AFTER`]).
    /// Rate limiting uses its own larger cap; generic and overload errors keep
    /// [`MAX_BACKOFF_MS`].
    pub(crate) fn delay_before_retry(&self, err: &Error, retry: usize) -> Duration {
        let (base, cap) = match err.classification() {
            Error::RateLimited { .. } => (
                self.backoff_base_ms
                    .saturating_mul(RATE_LIMIT_BACKOFF_MULTIPLIER),
                RATE_LIMIT_MAX_BACKOFF_MS,
            ),
            Error::Overloaded { .. } => (self.overloaded_backoff_base_ms, MAX_BACKOFF_MS),
            _ => (self.backoff_base_ms, MAX_BACKOFF_MS),
        };
        let exp = base.saturating_mul(1u64 << retry.min(32) as u32).min(cap);
        let backoff = jitter_backoff(exp);
        // The retry-after hint via the unified failure Disposition (E3-1): for a
        // retryable error this is exactly `err.retry_after()` — a Quota signal
        // carries the hint, a Transient one carries none — so backoff is unchanged,
        // but the classifier is the single source of the "how long" decision.
        match err.disposition().retry_after() {
            Some(hint) if hint > backoff => hint.min(MAX_RETRY_AFTER),
            _ => backoff,
        }
    }
}

/// Equal jitter: a uniform draw from `[delay/2, delay]`, so synchronized
/// clients spread out while every wait keeps at least half the intended pause.
fn jitter_backoff(delay_ms: u64) -> Duration {
    if delay_ms == 0 {
        return Duration::ZERO;
    }
    let half = delay_ms / 2;
    let jittered = half + next_random() % (delay_ms - half + 1);
    Duration::from_millis(jittered)
}

/// Thread-local SplitMix64 — enough randomness for jitter without pulling a
/// rand dependency into the runtime core.
fn next_random() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }
    STATE.with(|state| {
        let mut z = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        state.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED)
        ^ (std::process::id() as u64) << 32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overloaded(retry_after: Option<Duration>) -> Error {
        Error::Overloaded {
            message: "overloaded".to_string(),
            retry_after,
        }
    }

    #[test]
    fn default_attempt_watchdog_uses_the_shared_inference_contract() {
        // Cause/effect rule D3: default Runtime composition + shared watchdog
        // contract => the retry loop uses that exact outer bound. Effects: the
        // provider response deadline wins first; custom test/host policies stay
        // explicit. This connects the contract decision table to the one owner
        // that executes the watchdog rather than duplicating a duration literal.
        assert_eq!(
            LlmRetryPolicy::default().attempt_timeout,
            DEFAULT_MODEL_ATTEMPT_WATCHDOG
        );
    }

    #[test]
    fn backoff_grows_exponentially_within_jitter_bounds() {
        let policy = LlmRetryPolicy::default();
        let err = Error::Provider("500".to_string());
        for (retry, expected_ms) in [(0u32, 500u64), (1, 1_000), (2, 2_000), (3, 4_000)] {
            let delay = policy.delay_before_retry(&err, retry as usize);
            let ms = delay.as_millis() as u64;
            assert!(
                (expected_ms / 2..=expected_ms).contains(&ms),
                "retry {retry}: {ms}ms outside [{}, {expected_ms}]",
                expected_ms / 2
            );
        }
    }

    #[test]
    fn backoff_is_capped() {
        let policy = LlmRetryPolicy::default();
        let err = Error::Provider("500".to_string());
        for retry in [10usize, 40, 200] {
            assert!(policy.delay_before_retry(&err, retry).as_millis() as u64 <= MAX_BACKOFF_MS);
        }
    }

    #[test]
    fn overloaded_uses_longer_base() {
        let policy = LlmRetryPolicy::default();
        let ms = policy.delay_before_retry(&overloaded(None), 0).as_millis() as u64;
        assert!((1_000..=2_000).contains(&ms), "{ms}ms outside [1000, 2000]");
    }

    #[test]
    fn jitter_of_zero_is_zero() {
        // The early-return guard: a zero backoff never draws a (would-panic on the
        // `% (0 - 0 + 1)`) jitter — it is exactly `Duration::ZERO`.
        assert_eq!(jitter_backoff(0), Duration::ZERO);
    }

    #[test]
    fn retry_after_wins_when_longer_and_is_capped() {
        let policy = LlmRetryPolicy::default();
        // Longer than the backoff: adopted verbatim.
        let hint = Duration::from_secs(30);
        assert_eq!(policy.delay_before_retry(&overloaded(Some(hint)), 0), hint);
        // Beyond the cap: clamped to 60s.
        let excessive = Duration::from_secs(600);
        assert_eq!(
            policy.delay_before_retry(&overloaded(Some(excessive)), 0),
            MAX_RETRY_AFTER
        );
        // Shorter than the backoff: the backoff stands.
        let tiny = Duration::from_millis(1);
        assert!(
            policy.delay_before_retry(&overloaded(Some(tiny)), 0) >= Duration::from_millis(1_000)
        );
    }

    #[test]
    fn rate_limit_without_hint_gets_a_long_bounded_budget() {
        // Causes: C1 generic/overload/rate-limit error; C2 retry budget is
        // zero/nonzero; C3 no Retry-After. Effects: E1 generic limits remain
        // unchanged; E2 explicit zero disables every retry; E3 rate limiting
        // gets twelve extra attempts and a nominal 15-60s exponential backoff
        // (equal-jitter waits remain within half-to-full of each nominal value).
        // Rules R1=C1(generic)=>E1, R2=C2(zero)=>E2,
        // R3=C1(rate)+C2(nonzero)+C3=>E3. UsageLimit never reaches this policy
        // because the engine's existing is_retryable gate remains authoritative.
        let policy = LlmRetryPolicy::default();
        let limited = Error::RateLimited {
            message: "concurrency limit".into(),
            retry_after: None,
        };
        assert_eq!(
            policy.retry_limit(&Error::Provider("500".into())),
            2,
            "R1/E1"
        );
        assert_eq!(
            policy.retry_limit(&limited),
            RATE_LIMIT_MIN_RETRIES,
            "R3/E3"
        );
        assert_eq!(
            LlmRetryPolicy {
                max_retries: 0,
                ..policy.clone()
            }
            .retry_limit(&limited),
            0,
            "R2/E2"
        );
        let first = policy.delay_before_retry(&limited, 0).as_millis() as u64;
        let saturated = policy.delay_before_retry(&limited, 8).as_millis() as u64;
        assert!((7_500..=15_000).contains(&first), "R3/E3 first={first}");
        assert!(
            (30_000..=RATE_LIMIT_MAX_BACKOFF_MS).contains(&saturated),
            "R3/E3 saturated={saturated}"
        );
    }
}
