//! Per-model circuit breaker for inference calls.
//!
//! Consecutive retryable failures against one model open its circuit; while
//! open, calls fail fast instead of burning a full retry budget against a
//! provider that is already down. After a cooldown one half-open probe is let
//! through: success closes the circuit, failure reopens it. Only retryable
//! errors count — a permanent error (bad key, overlong prompt) says nothing
//! about provider health, and counting it would trip the breaker on caller
//! mistakes. The counted set must therefore stay exactly
//! `llm::Error::is_retryable()`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Tuning for [`CircuitBreaker`]. The defaults mirror the goal runtime:
/// 5 consecutive failures open the circuit for a 30s cooldown, then one
/// half-open probe decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Consecutive counted failures that open the circuit.
    pub failure_threshold: u32,
    /// How long an open circuit rejects calls before probing.
    pub cooldown: Duration,
    /// Concurrent probes allowed while half-open.
    pub half_open_max_probes: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(30),
            half_open_max_probes: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open { since: Instant },
    HalfOpen { probes: u32 },
}

#[derive(Debug)]
struct ModelCircuit {
    state: State,
    consecutive_failures: u32,
}

impl Default for ModelCircuit {
    fn default() -> Self {
        Self {
            state: State::Closed,
            consecutive_failures: 0,
        }
    }
}

/// Per-model breaker state shared by every run on one `Runtime`.
#[derive(Debug, Default)]
pub(crate) struct CircuitBreaker {
    config: CircuitBreakerConfig,
    circuits: Mutex<HashMap<String, ModelCircuit>>,
}

impl CircuitBreaker {
    pub(crate) fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            circuits: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a call to `model` may proceed. An open circuit past its
    /// cooldown transitions to half-open and admits up to the configured
    /// number of probes; otherwise the rejection names the model.
    pub(crate) fn check(&self, model: &str) -> Result<(), String> {
        let mut circuits = match self.circuits.lock() {
            Ok(circuits) => circuits,
            // A poisoned registry fails open: refusing all inference because a
            // panic hit mid-update would be worse than losing breaker state.
            Err(_) => return Ok(()),
        };
        let circuit = circuits.entry(model.to_string()).or_default();
        match circuit.state {
            State::Closed => Ok(()),
            State::Open { since } => {
                if since.elapsed() >= self.config.cooldown {
                    circuit.state = State::HalfOpen { probes: 1 };
                    Ok(())
                } else {
                    Err(format!(
                        "circuit breaker open for model '{model}': recent consecutive failures; \
                         retry after cooldown"
                    ))
                }
            }
            State::HalfOpen { probes } => {
                if probes < self.config.half_open_max_probes {
                    circuit.state = State::HalfOpen { probes: probes + 1 };
                    Ok(())
                } else {
                    Err(format!(
                        "circuit breaker half-open for model '{model}': probe already in flight"
                    ))
                }
            }
        }
    }

    /// A call succeeded: close the circuit and reset the failure count.
    pub(crate) fn record_success(&self, model: &str) {
        if let Ok(mut circuits) = self.circuits.lock() {
            let circuit = circuits.entry(model.to_string()).or_default();
            circuit.state = State::Closed;
            circuit.consecutive_failures = 0;
        }
    }

    /// A counted (retryable) failure: a failing half-open probe reopens the
    /// circuit immediately; otherwise the consecutive count grows and opens
    /// the circuit at the threshold.
    pub(crate) fn record_failure(&self, model: &str) {
        if let Ok(mut circuits) = self.circuits.lock() {
            let circuit = circuits.entry(model.to_string()).or_default();
            circuit.consecutive_failures = circuit.consecutive_failures.saturating_add(1);
            match circuit.state {
                State::HalfOpen { .. } => {
                    circuit.state = State::Open {
                        since: Instant::now(),
                    };
                }
                _ => {
                    if circuit.consecutive_failures >= self.config.failure_threshold {
                        circuit.state = State::Open {
                            since: Instant::now(),
                        };
                    }
                }
            }
        }
    }

    /// A half-open probe was dropped before finishing (typically a user
    /// cancel): reopen the circuit so the next cooldown re-probes, but do not
    /// grow the failure count — an abandoned probe says nothing about health.
    pub(crate) fn record_abandoned_probe(&self, model: &str) {
        if let Ok(mut circuits) = self.circuits.lock() {
            let circuit = circuits.entry(model.to_string()).or_default();
            if let State::HalfOpen { .. } = circuit.state {
                circuit.state = State::Open {
                    since: Instant::now(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker(threshold: u32, cooldown: Duration) -> CircuitBreaker {
        CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: threshold,
            cooldown,
            half_open_max_probes: 1,
        })
    }

    #[test]
    fn opens_after_threshold_and_rejects() {
        let cb = breaker(2, Duration::from_secs(60));
        assert!(cb.check("m").is_ok());
        cb.record_failure("m");
        assert!(cb.check("m").is_ok(), "below threshold still admits");
        cb.record_failure("m");
        let rejection = cb.check("m").expect_err("open circuit rejects");
        assert!(rejection.contains("circuit breaker open for model 'm'"));
    }

    #[test]
    fn success_closes_and_resets_the_count() {
        let cb = breaker(2, Duration::from_secs(60));
        cb.record_failure("m");
        cb.record_success("m");
        // The count restarted: one more failure stays below the threshold.
        cb.record_failure("m");
        assert!(cb.check("m").is_ok());
    }

    #[test]
    fn cooldown_admits_one_probe_then_rejects_concurrent_probes() {
        let cb = breaker(1, Duration::from_millis(0));
        cb.record_failure("m");
        // Cooldown already elapsed: the circuit half-opens for one probe.
        assert!(cb.check("m").is_ok());
        let rejection = cb.check("m").expect_err("second concurrent probe rejects");
        assert!(rejection.contains("half-open"));
    }

    #[test]
    fn probe_failure_reopens_probe_success_closes() {
        let cb = breaker(1, Duration::from_millis(0));
        cb.record_failure("m");
        assert!(cb.check("m").is_ok(), "probe admitted");
        cb.record_failure("m");
        // Reopened with a fresh cooldown (0ms here), so the next check probes
        // again rather than staying closed.
        assert!(cb.check("m").is_ok());
        cb.record_success("m");
        assert!(cb.check("m").is_ok());
        assert!(cb.check("m").is_ok(), "closed circuit admits freely");
    }

    #[test]
    fn abandoned_probe_reopens_without_counting_a_failure() {
        let cb = breaker(2, Duration::from_millis(0));
        cb.record_failure("m");
        cb.record_failure("m");
        assert!(cb.check("m").is_ok(), "probe admitted after cooldown");
        cb.record_abandoned_probe("m");
        // Reopened: after the (zero) cooldown the next check probes again.
        assert!(cb.check("m").is_ok());
        // The failure count did not grow: one success closes and a single new
        // failure stays below the threshold of 2.
        cb.record_success("m");
        cb.record_failure("m");
        assert!(cb.check("m").is_ok());
    }

    #[test]
    fn models_are_isolated() {
        let cb = breaker(1, Duration::from_secs(60));
        cb.record_failure("down");
        assert!(cb.check("down").is_err());
        assert!(cb.check("healthy").is_ok());
    }
}
