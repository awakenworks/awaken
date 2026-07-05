//! Retry policy for inference calls.
//!
//! Exponential backoff with equal jitter, a longer base for provider overload,
//! and the server's `Retry-After` hint honored when it is longer than the
//! computed backoff — capped so a misconfigured or hostile header cannot park
//! the loop indefinitely. Permanent errors never reach this policy: the caller
//! checks `Error::is_retryable()` first.

use std::time::Duration;

use awaken_runtime_contract::llm::Error;

/// Backoff never exceeds this, regardless of attempt count.
const MAX_BACKOFF_MS: u64 = 8_000;

/// A server `Retry-After` longer than the computed backoff is adopted verbatim,
/// but never beyond this cap.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How inference retries are paced. Held by the `Runtime` and applied by the
/// engine's inference call; tests inject a fast policy to keep suites quick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRetryPolicy {
    /// Extra attempts after the first failure (2 means up to 3 calls).
    pub max_retries: usize,
    /// Exponential backoff base for a generic retryable error.
    pub backoff_base_ms: u64,
    /// Backoff base when the provider reports overload — deliberately longer,
    /// since hammering an overloaded provider extends the outage.
    pub overloaded_backoff_base_ms: u64,
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_base_ms: 500,
            overloaded_backoff_base_ms: 2_000,
        }
    }
}

impl LlmRetryPolicy {
    /// How long to wait before retry number `retry` (0-based): the jittered
    /// exponential backoff for the error class, or the server's `Retry-After`
    /// when that is longer (adopted verbatim, capped at [`MAX_RETRY_AFTER`]).
    pub(crate) fn delay_before_retry(&self, err: &Error, retry: usize) -> Duration {
        let base = match err {
            Error::Overloaded { .. } => self.overloaded_backoff_base_ms,
            _ => self.backoff_base_ms,
        };
        let exp = base
            .saturating_mul(1u64 << retry.min(32) as u32)
            .min(MAX_BACKOFF_MS);
        let backoff = jitter_backoff(exp);
        match err.retry_after() {
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
}
