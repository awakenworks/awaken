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
    fn independent_models() {
        let cb = CircuitBreaker::new(fast_config());
        for _ in 0..3 {
            cb.record_failure("model-a");
        }
        assert!(cb.check("model-a").is_err());
        assert!(cb.check("model-b").is_ok());
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
}
