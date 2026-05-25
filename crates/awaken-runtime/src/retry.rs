//! Bounded exponential-backoff retry primitive.
//!
//! A small generic helper used by subsystems (credential broker, future
//! MCP retry, etc.) that need *one*-shot retry-with-backoff semantics
//! around an async operation. The full LLM-side retry coordinator lives
//! in [`crate::engine::retry`] — that one also handles LLM circuit
//! breaking and stream resume, none of which apply here.
//!
//! Contract:
//! - `is_retryable(&err)` returning false short-circuits on the current
//!   attempt — permanent errors are not retried.
//! - At most `policy.max_attempts` attempts (≥ 1).
//! - The most recent error is surfaced when the budget is exhausted, so
//!   callers see the actual current root cause rather than a stale
//!   first-blip message.
//! - Backoff doubles by `policy.multiplier` per failure, capped by
//!   `policy.max`. The first retry waits `policy.initial`.

use std::time::Duration;

/// Knobs for [`with_backoff`].
#[derive(Debug, Clone)]
pub struct BackoffPolicy {
    /// Total attempts (including the first). `1` disables retry.
    pub max_attempts: u32,
    /// Backoff before the second attempt. Doubled (× `multiplier`) each
    /// subsequent time, then capped by `max`.
    pub initial: Duration,
    /// Multiplier applied to backoff after each failed attempt.
    pub multiplier: f64,
    /// Cap on backoff growth — stops one slow tail-attempt from delaying
    /// the caller beyond `max` per retry.
    pub max: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial: Duration::from_millis(100),
            multiplier: 2.0,
            max: Duration::from_secs(1),
        }
    }
}

impl BackoffPolicy {
    /// Disable retries — every failure surfaces immediately on attempt 1.
    pub fn disabled() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }
}

/// Retry an async operation with bounded exponential backoff.
///
/// `op` is invoked up to `policy.max_attempts` times. After each failed
/// attempt that `is_retryable` accepts, the loop sleeps for the current
/// backoff (starting at `policy.initial`, multiplied by `policy.multiplier`
/// each step, clamped by `policy.max`) before the next attempt.
///
/// `on_retry(attempt, err, backoff)` fires *before* the sleep so callers
/// can record telemetry / log the transient failure. It does not fire
/// for terminal failures.
pub async fn with_backoff<T, E, F, Fut>(
    policy: &BackoffPolicy,
    is_retryable: impl Fn(&E) -> bool,
    on_retry: impl Fn(u32, &E, Duration),
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut attempt: u32 = 1;
    let mut backoff = policy.initial;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) if !is_retryable(&err) => return Err(err),
            Err(err) if attempt >= policy.max_attempts => return Err(err),
            Err(err) => {
                on_retry(attempt, &err, backoff);
                tokio::time::sleep(backoff).await;
                let scaled = backoff.as_secs_f64() * policy.multiplier;
                let scaled_dur = Duration::from_secs_f64(scaled);
                backoff = if scaled_dur > policy.max {
                    policy.max
                } else {
                    scaled_dur
                };
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fast(max_attempts: u32) -> BackoffPolicy {
        BackoffPolicy {
            max_attempts,
            initial: Duration::from_micros(10),
            multiplier: 2.0,
            max: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn ok_on_first_attempt_runs_op_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_op = Arc::clone(&calls);
        let result: Result<u32, &'static str> = with_backoff(
            &fast(5),
            |_| true,
            |_, _, _| {},
            || {
                let calls = Arc::clone(&calls_for_op);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(42)
                }
            },
        )
        .await;
        assert_eq!(result, Ok(42));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn permanent_error_short_circuits_attempt_one() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_op = Arc::clone(&calls);
        let result: Result<(), &'static str> = with_backoff(
            &fast(5),
            |_| false,
            |_, _, _| panic!("on_retry must not fire for permanent errors"),
            || {
                let calls = Arc::clone(&calls_for_op);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err("fatal")
                }
            },
        )
        .await;
        assert_eq!(result, Err("fatal"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_then_success_retries_then_returns_ok() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_op = Arc::clone(&calls);
        let result: Result<&'static str, &'static str> = with_backoff(
            &fast(3),
            |_| true,
            |_, _, _| {},
            || {
                let calls = Arc::clone(&calls_for_op);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 { Err("blip") } else { Ok("ok") }
                }
            },
        )
        .await;
        assert_eq!(result, Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn budget_exhausted_returns_last_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_op = Arc::clone(&calls);
        let result: Result<(), String> = with_backoff(
            &fast(4),
            |_| true,
            |_, _, _| {},
            || {
                let calls = Arc::clone(&calls_for_op);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    Err(format!("attempt #{n}"))
                }
            },
        )
        .await;
        assert_eq!(result, Err("attempt #4".to_owned()));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn disabled_policy_runs_exactly_once_even_on_transient() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_op = Arc::clone(&calls);
        let _: Result<(), &'static str> = with_backoff(
            &BackoffPolicy::disabled(),
            |_| true,
            |_, _, _| {},
            || {
                let calls = Arc::clone(&calls_for_op);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err("x")
                }
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backoff_growth_is_clamped_by_max() {
        // initial=1ms, multiplier=10.0, max=2ms → real sleeps are 1, 2, 2.
        // Total wall-clock ≤ 5ms; without clamping it would be 1+10+100 = 111ms.
        let policy = BackoffPolicy {
            max_attempts: 4,
            initial: Duration::from_millis(1),
            multiplier: 10.0,
            max: Duration::from_millis(2),
        };
        let start = std::time::Instant::now();
        let _: Result<(), &'static str> = with_backoff(
            &policy,
            |_| true,
            |_, _, _| {},
            || async move { Err("blip") },
        )
        .await;
        assert!(
            start.elapsed() < Duration::from_millis(55),
            "backoff appears unbounded: elapsed={:?}",
            start.elapsed()
        );
    }
}
