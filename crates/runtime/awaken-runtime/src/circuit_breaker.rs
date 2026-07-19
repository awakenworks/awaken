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
use std::time::{Duration, Instant};

use awaken_runtime_contract::metrics::MetricsRecorder;
use parking_lot::Mutex;

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
    probe_generation: u64,
}

impl Default for ModelCircuit {
    fn default() -> Self {
        Self {
            state: State::Closed,
            consecutive_failures: 0,
            probe_generation: 0,
        }
    }
}

/// Per-model breaker state shared by every run on one `Runtime`.
#[derive(Debug, Default)]
pub(crate) struct CircuitBreaker {
    config: CircuitBreakerConfig,
    circuits: Mutex<HashMap<String, ModelCircuit>>,
}

/// Admission capability for exactly one inference attempt. A half-open permit
/// carries the generation of the probe cycle that admitted it, so a late result
/// from an older cycle cannot mutate a newer one. Dropping an unfinished probe
/// is fail-closed and reopens that same cycle automatically.
pub(crate) struct CircuitPermit<'a> {
    breaker: &'a CircuitBreaker,
    metrics: &'a dyn MetricsRecorder,
    model: String,
    probe_generation: Option<u64>,
    finished: bool,
}

impl CircuitPermit<'_> {
    pub(crate) fn success(mut self) {
        self.breaker
            .finish_success(&self.model, self.probe_generation);
        self.finished = true;
    }

    pub(crate) fn failure(mut self, retryable: bool) {
        if retryable {
            self.breaker
                .finish_retryable_failure(&self.model, self.probe_generation);
        } else {
            self.breaker.finish_inconclusive_probe(
                &self.model,
                self.probe_generation,
                self.metrics,
            );
        }
        self.finished = true;
    }
}

impl Drop for CircuitPermit<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.breaker.finish_inconclusive_probe(
                &self.model,
                self.probe_generation,
                self.metrics,
            );
        }
    }
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
    pub(crate) fn check<'a>(
        &'a self,
        model: &str,
        metrics: &'a dyn MetricsRecorder,
    ) -> Result<CircuitPermit<'a>, String> {
        let mut circuits = self.circuits.lock();
        let circuit = circuits.entry(model.to_string()).or_default();
        let probe_generation = match circuit.state {
            State::Closed => None,
            State::Open { since } => {
                if since.elapsed() >= self.config.cooldown {
                    circuit.probe_generation = circuit.probe_generation.wrapping_add(1);
                    circuit.state = State::HalfOpen { probes: 1 };
                    Some(circuit.probe_generation)
                } else {
                    return Err(format!(
                        "circuit breaker open for model '{model}': recent consecutive failures; \
                         retry after cooldown"
                    ));
                }
            }
            State::HalfOpen { probes } => {
                if probes < self.config.half_open_max_probes {
                    circuit.state = State::HalfOpen { probes: probes + 1 };
                    Some(circuit.probe_generation)
                } else {
                    return Err(format!(
                        "circuit breaker half-open for model '{model}': probe already in flight"
                    ));
                }
            }
        };
        Ok(CircuitPermit {
            breaker: self,
            metrics,
            model: model.to_string(),
            probe_generation,
            finished: false,
        })
    }

    fn finish_success(&self, model: &str, generation: Option<u64>) {
        let mut circuits = self.circuits.lock();
        let circuit = circuits.entry(model.to_string()).or_default();
        if generation.is_some_and(|generation| {
            !matches!(circuit.state, State::HalfOpen { .. })
                || circuit.probe_generation != generation
        }) {
            return;
        }
        circuit.state = State::Closed;
        circuit.consecutive_failures = 0;
    }

    fn finish_retryable_failure(&self, model: &str, generation: Option<u64>) {
        let mut circuits = self.circuits.lock();
        let circuit = circuits.entry(model.to_string()).or_default();
        if generation.is_some_and(|generation| {
            !matches!(circuit.state, State::HalfOpen { .. })
                || circuit.probe_generation != generation
        }) {
            return;
        }
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

    fn finish_inconclusive_probe(
        &self,
        model: &str,
        generation: Option<u64>,
        metrics: &dyn MetricsRecorder,
    ) {
        let Some(generation) = generation else {
            return;
        };
        let mut circuits = self.circuits.lock();
        let circuit = circuits.entry(model.to_string()).or_default();
        if matches!(circuit.state, State::HalfOpen { .. }) && circuit.probe_generation == generation
        {
            circuit.state = State::Open {
                since: Instant::now(),
            };
            drop(circuits);
            metrics.record_circuit_transition(model, "open");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use awaken_runtime_contract::metrics::NoopRecorder;

    use super::*;

    /// Records every circuit transition it is told about, for D9 assertions.
    #[derive(Default)]
    struct SpyRecorder {
        transitions: StdMutex<Vec<(String, String)>>,
    }
    impl MetricsRecorder for SpyRecorder {
        fn record_inference(&self, _metric: awaken_runtime_contract::metrics::InferenceMetric<'_>) {
        }
        fn record_tool(&self, _tool: &str, _outcome: &str, _duration: Duration) {}
        fn record_circuit_transition(&self, model: &str, to_state: &str) {
            self.transitions
                .lock()
                .unwrap()
                .push((model.to_string(), to_state.to_string()));
        }
    }

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
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        assert!(
            cb.check("m", &NoopRecorder).is_ok(),
            "below threshold still admits"
        );
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        let rejection = cb
            .check("m", &NoopRecorder)
            .err()
            .expect("open circuit rejects");
        assert!(rejection.contains("circuit breaker open for model 'm'"));
    }

    #[test]
    fn success_closes_and_resets_the_count() {
        let cb = breaker(2, Duration::from_secs(60));
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        cb.check("m", &NoopRecorder).unwrap().success();
        // The count restarted: one more failure stays below the threshold.
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        assert!(cb.check("m", &NoopRecorder).is_ok());
    }

    #[test]
    fn cooldown_admits_one_probe_then_rejects_concurrent_probes() {
        let cb = breaker(1, Duration::from_millis(0));
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        // Cooldown already elapsed: the circuit half-opens for one probe.
        let _probe = cb.check("m", &NoopRecorder).expect("probe admitted");
        let rejection = cb
            .check("m", &NoopRecorder)
            .err()
            .expect("second concurrent probe rejects");
        assert!(rejection.contains("half-open"));
    }

    #[test]
    fn probe_failure_reopens_probe_success_closes() {
        let cb = breaker(1, Duration::from_millis(0));
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        cb.check("m", &NoopRecorder)
            .expect("probe admitted")
            .failure(true);
        // Reopened with a fresh cooldown (0ms here), so the next check probes
        // again rather than staying closed.
        cb.check("m", &NoopRecorder).unwrap().success();
        assert!(cb.check("m", &NoopRecorder).is_ok());
        assert!(
            cb.check("m", &NoopRecorder).is_ok(),
            "closed circuit admits freely"
        );
    }

    #[test]
    fn abandoned_probe_reopens_without_counting_a_failure() {
        let cb = breaker(2, Duration::from_millis(0));
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        drop(
            cb.check("m", &NoopRecorder)
                .expect("probe admitted after cooldown"),
        );
        // Reopened: after the (zero) cooldown the next check probes again.
        cb.check("m", &NoopRecorder).unwrap().success();
        // The failure count did not grow: one success closes and a single new
        // failure stays below the threshold of 2.
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        assert!(cb.check("m", &NoopRecorder).is_ok());
    }

    #[test]
    fn an_abandoned_probe_on_a_closed_circuit_is_a_no_op() {
        // Abandoning a probe is only meaningful half-open. On a healthy, closed
        // circuit it must not open anything — the reopen guard is `HalfOpen`-only.
        let cb = breaker(2, Duration::from_secs(60));
        let spy = SpyRecorder::default();
        drop(cb.check("m", &spy).expect("closed admission"));
        assert!(cb.check("m", &spy).is_ok(), "a closed circuit stays closed");
        assert!(
            spy.transitions.lock().unwrap().is_empty(),
            "no transition on a closed circuit → no metric"
        );
    }

    #[test]
    fn an_abandoned_probe_reopen_is_surfaced_on_metrics() {
        // D9 observability: a cancelled half-open probe reopens the circuit with no
        // inference outcome to explain it, so the reopen is emitted as a transition
        // metric an operator can see.
        let cb = breaker(1, Duration::from_millis(0));
        let spy = SpyRecorder::default();
        cb.check("m", &spy).unwrap().failure(true); // opens
        drop(
            cb.check("m", &spy)
                .expect("cooldown elapsed → half-open probe"),
        );
        assert_eq!(
            *spy.transitions.lock().unwrap(),
            vec![("m".to_string(), "open".to_string())],
            "the abandoned reopen is surfaced exactly once, as a transition to open"
        );
    }

    #[test]
    fn models_are_isolated() {
        let cb = breaker(1, Duration::from_secs(60));
        cb.check("down", &NoopRecorder).unwrap().failure(true);
        assert!(cb.check("down", &NoopRecorder).is_err());
        assert!(cb.check("healthy", &NoopRecorder).is_ok());
    }

    #[test]
    fn late_probe_result_cannot_mutate_a_new_generation() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::ZERO,
            half_open_max_probes: 2,
        });
        cb.check("m", &NoopRecorder).unwrap().failure(true);
        let first = cb.check("m", &NoopRecorder).expect("first probe");
        let stale = cb.check("m", &NoopRecorder).expect("second probe");
        first.failure(true);

        let current = cb.check("m", &NoopRecorder).expect("new generation probe");
        stale.success();
        let peer = cb
            .check("m", &NoopRecorder)
            .expect("one peer allowed in the current generation");
        assert!(
            cb.check("m", &NoopRecorder).is_err(),
            "the stale success did not close the current half-open cycle"
        );
        peer.success();
        drop(current);
    }
}
