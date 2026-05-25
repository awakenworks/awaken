//! Per-model circuit breaker for LLM inference.
//!
//! Prevents cascading failures by short-circuiting requests to models that
//! have experienced repeated consecutive failures. After a cooldown period
//! the circuit transitions to half-open, allowing a limited number of probe
//! requests before fully closing again on success.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use awaken_contract::contract::executor::InferenceExecutionError;

/// Circuit breaker status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitStatus {
    /// Normal operation — requests flow through.
    Closed,
    /// Too many failures — requests are rejected immediately.
    Open,
    /// Cooldown elapsed — a limited number of probe requests are allowed.
    HalfOpen,
}

/// Internal per-model state.
#[derive(Debug)]
struct CircuitState {
    consecutive_failures: u32,
    last_failure: Instant,
    status: CircuitStatus,
    half_open_attempts: u32,
}

impl CircuitState {
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
            last_failure: Instant::now(),
            status: CircuitStatus::Closed,
            half_open_attempts: 0,
        }
    }
}

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before the circuit opens.
    pub failure_threshold: u32,
    /// Duration the circuit stays open before transitioning to half-open.
    pub cooldown: Duration,
    /// Maximum probe requests allowed in the half-open state.
    pub half_open_max: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
            half_open_max: 1,
        }
    }
}

/// Per-model circuit breaker.
///
/// Thread-safe — uses `parking_lot::RwLock` for interior mutability.
pub struct CircuitBreaker {
    states: parking_lot::RwLock<HashMap<String, CircuitState>>,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    /// Create a circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            states: parking_lot::RwLock::new(HashMap::new()),
            config,
        }
    }

    /// Check whether a request to `model` is allowed.
    ///
    /// Returns `Ok(())` if the circuit is closed or half-open (under probe limit).
    /// Returns `Err(Provider("circuit breaker open for model X"))` if open.
    pub fn check(&self, model: &str) -> Result<(), InferenceExecutionError> {
        let mut states = self.states.write();
        let state = states
            .entry(model.to_string())
            .or_insert_with(CircuitState::new);

        match state.status {
            CircuitStatus::Closed => Ok(()),
            CircuitStatus::Open => {
                if state.last_failure.elapsed() >= self.config.cooldown {
                    state.status = CircuitStatus::HalfOpen;
                    state.half_open_attempts = 1;
                    Ok(())
                } else {
                    Err(InferenceExecutionError::Provider(format!(
                        "circuit breaker open for model {model}"
                    )))
                }
            }
            CircuitStatus::HalfOpen => {
                if state.half_open_attempts < self.config.half_open_max {
                    state.half_open_attempts += 1;
                    Ok(())
                } else {
                    Err(InferenceExecutionError::Provider(format!(
                        "circuit breaker open for model {model}"
                    )))
                }
            }
        }
    }

    /// Read-only health peek: would [`check`](Self::check) currently admit a
    /// request to `model`? Unlike `check`, this never mutates state (no
    /// Open→HalfOpen transition, no probe consumption), so callers can build a
    /// health mask over many models without spending half-open probes.
    pub fn is_available(&self, model: &str) -> bool {
        let states = self.states.read();
        match states.get(model) {
            None => true,
            Some(state) => match state.status {
                CircuitStatus::Closed => true,
                // Cooldown elapsed → check() would transition to half-open and admit.
                CircuitStatus::Open => state.last_failure.elapsed() >= self.config.cooldown,
                CircuitStatus::HalfOpen => state.half_open_attempts < self.config.half_open_max,
            },
        }
    }

    /// Record a successful request to `model`, resetting the circuit to closed.
    pub fn record_success(&self, model: &str) {
        let mut states = self.states.write();
        if let Some(state) = states.get_mut(model) {
            state.consecutive_failures = 0;
            state.half_open_attempts = 0;
            state.status = CircuitStatus::Closed;
        }
    }

    /// Record a failed request to `model`, potentially opening the circuit.
    pub fn record_failure(&self, model: &str) {
        let mut states = self.states.write();
        let state = states
            .entry(model.to_string())
            .or_insert_with(CircuitState::new);

        state.consecutive_failures += 1;
        state.last_failure = Instant::now();

        if state.status == CircuitStatus::HalfOpen {
            // Probe failed — re-open immediately.
            state.status = CircuitStatus::Open;
        } else if state.consecutive_failures >= self.config.failure_threshold {
            state.status = CircuitStatus::Open;
        }
    }

    /// Release an abandoned half-open probe without counting it as a normal
    /// provider failure.
    ///
    /// Stream cancellation/drop is often user-initiated, so treating every
    /// abandoned stream as a member failure would pollute health. In half-open
    /// state, however, a consumed probe must resolve; abandoning it re-opens
    /// the circuit without incrementing the consecutive failure counter.
    pub fn record_abandoned_probe(&self, model: &str) {
        let mut states = self.states.write();
        let Some(state) = states.get_mut(model) else {
            return;
        };
        if state.status == CircuitStatus::HalfOpen {
            state.status = CircuitStatus::Open;
            state.half_open_attempts = 0;
            state.last_failure = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::from_millis(50),
            half_open_max: 1,
        }
    }

    #[test]
    fn closed_allows_requests() {
        let cb = CircuitBreaker::new(fast_config());
        assert!(cb.check("model-a").is_ok());
    }

    #[test]
    fn opens_after_threshold_failures() {
        let cb = CircuitBreaker::new(fast_config());
        for _ in 0..3 {
            cb.record_failure("model-a");
        }
        assert!(cb.check("model-a").is_err());
    }

    #[test]
    fn below_threshold_stays_closed() {
        let cb = CircuitBreaker::new(fast_config());
        cb.record_failure("model-a");
        cb.record_failure("model-a");
        assert!(cb.check("model-a").is_ok());
    }

    #[test]
    fn success_resets_failure_count() {
        let cb = CircuitBreaker::new(fast_config());
        cb.record_failure("model-a");
        cb.record_failure("model-a");
        cb.record_success("model-a");
        cb.record_failure("model-a");
        cb.record_failure("model-a");
        // Only 2 consecutive after reset, threshold is 3
        assert!(cb.check("model-a").is_ok());
    }

    #[test]
    fn transitions_to_half_open_after_cooldown() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(10),
            half_open_max: 1,
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("model-a");
        cb.record_failure("model-a");
        assert!(cb.check("model-a").is_err());

        std::thread::sleep(Duration::from_millis(15));

        // Should transition to half-open and allow one probe
        assert!(cb.check("model-a").is_ok());
        // Second probe exceeds half_open_max
        assert!(cb.check("model-a").is_err());
    }

    #[test]
    fn half_open_success_closes_circuit() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(10),
            half_open_max: 1,
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("model-a");
        cb.record_failure("model-a");

        std::thread::sleep(Duration::from_millis(15));

        assert!(cb.check("model-a").is_ok());
        cb.record_success("model-a");

        // Circuit should be closed now — unlimited requests
        assert!(cb.check("model-a").is_ok());
        assert!(cb.check("model-a").is_ok());
    }

    #[test]
    fn half_open_failure_reopens_circuit() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(10),
            half_open_max: 1,
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("model-a");
        cb.record_failure("model-a");

        std::thread::sleep(Duration::from_millis(15));

        assert!(cb.check("model-a").is_ok());
        cb.record_failure("model-a");

        // Should be open again
        assert!(cb.check("model-a").is_err());
    }

    #[test]
    fn abandoned_half_open_probe_reopens_without_incrementing_failures() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::ZERO,
            half_open_max: 1,
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("model-a");
        cb.record_failure("model-a");
        cb.record_failure("model-a");

        assert!(cb.check("model-a").is_ok());
        cb.record_abandoned_probe("model-a");

        assert!(cb.check("model-a").is_ok());
        cb.record_success("model-a");
        assert!(cb.check("model-a").is_ok());
    }

    #[test]
    fn independent_models() {
        let cb = CircuitBreaker::new(fast_config());
        for _ in 0..3 {
            cb.record_failure("model-a");
        }
        assert!(cb.check("model-a").is_err());
        assert!(cb.check("model-b").is_ok());
    }

    #[test]
    fn is_available_true_for_unknown_and_closed() {
        let cb = CircuitBreaker::new(fast_config());
        assert!(cb.is_available("never-seen"));
        cb.record_failure("m");
        assert!(cb.is_available("m"), "below threshold stays available");
    }

    #[test]
    fn is_available_false_while_open_within_cooldown() {
        let cb = CircuitBreaker::new(fast_config());
        for _ in 0..3 {
            cb.record_failure("m");
        }
        assert!(!cb.is_available("m"), "open within cooldown is unavailable");
    }

    #[test]
    fn is_available_true_after_cooldown_without_mutating() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(10),
            half_open_max: 1,
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("m");
        cb.record_failure("m");
        std::thread::sleep(Duration::from_millis(15));
        // Peek twice: must stay available (no half-open probe consumed) since
        // it is read-only, unlike check().
        assert!(cb.is_available("m"));
        assert!(cb.is_available("m"));
        // And check() still gets its full probe budget afterwards.
        assert!(cb.check("m").is_ok());
    }

    #[test]
    fn default_config_values() {
        let config = CircuitBreakerConfig::default();
        assert_eq!(config.failure_threshold, 5);
        assert_eq!(config.cooldown, Duration::from_secs(30));
        assert_eq!(config.half_open_max, 1);
    }

    #[test]
    fn error_message_contains_model_name() {
        let cb = CircuitBreaker::new(fast_config());
        for _ in 0..3 {
            cb.record_failure("gpt-4o");
        }
        let err = cb.check("gpt-4o").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gpt-4o"), "error should mention model: {msg}");
        assert!(
            msg.contains("circuit breaker"),
            "error should mention circuit breaker: {msg}"
        );
    }

    #[test]
    fn half_open_allows_multiple_probes() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(10),
            half_open_max: 3,
        };
        let cb = CircuitBreaker::new(config);
        // Trip the circuit
        cb.record_failure("m");
        cb.record_failure("m");
        assert!(cb.check("m").is_err()); // Open

        std::thread::sleep(Duration::from_millis(15));

        // Should allow 3 probes in HalfOpen (first transitions Open→HalfOpen, counts as 1)
        assert!(cb.check("m").is_ok()); // probe 1 (transition)
        assert!(cb.check("m").is_ok()); // probe 2
        assert!(cb.check("m").is_ok()); // probe 3
        assert!(cb.check("m").is_err()); // probe 4 blocked
    }

    // ── Property-based tests ──

    mod proptest_circuit_breaker {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn opens_exactly_at_threshold(
                threshold in 1u32..20,
                failures in 0u32..30,
            ) {
                let cb = CircuitBreaker::new(CircuitBreakerConfig {
                    failure_threshold: threshold,
                    cooldown: Duration::from_secs(60),
                    half_open_max: 1,
                });

                for _ in 0..failures {
                    cb.record_failure("model");
                }

                if failures >= threshold {
                    prop_assert!(
                        cb.check("model").is_err(),
                        "should be open after {failures} >= {threshold} failures"
                    );
                } else {
                    prop_assert!(
                        cb.check("model").is_ok(),
                        "should be closed after {failures} < {threshold} failures"
                    );
                }
            }

            #[test]
            fn success_always_resets_to_closed(
                pre_failures in 0u32..10,
            ) {
                let cb = CircuitBreaker::new(CircuitBreakerConfig {
                    failure_threshold: 20, // high threshold so we stay closed
                    cooldown: Duration::from_secs(60),
                    half_open_max: 1,
                });

                for _ in 0..pre_failures {
                    cb.record_failure("model");
                }
                cb.record_success("model");

                // After success, a single failure should not trip threshold=20
                cb.record_failure("model");
                prop_assert!(
                    cb.check("model").is_ok(),
                    "circuit should be closed after success reset"
                );
            }

            #[test]
            fn independent_models_do_not_interfere(
                failures_a in 0u32..10,
                failures_b in 0u32..10,
            ) {
                let threshold = 5;
                let cb = CircuitBreaker::new(CircuitBreakerConfig {
                    failure_threshold: threshold,
                    cooldown: Duration::from_secs(60),
                    half_open_max: 1,
                });

                for _ in 0..failures_a {
                    cb.record_failure("model-a");
                }
                for _ in 0..failures_b {
                    cb.record_failure("model-b");
                }

                let a_expected_open = failures_a >= threshold;
                let b_expected_open = failures_b >= threshold;
                prop_assert_eq!(cb.check("model-a").is_err(), a_expected_open);
                prop_assert_eq!(cb.check("model-b").is_err(), b_expected_open);
            }
        }
    }
}
