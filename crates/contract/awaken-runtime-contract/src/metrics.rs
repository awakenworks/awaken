//! Cross-cutting metrics port for the runtime's GenAI chokepoints (#2).
//!
//! A neutral sink so the execution core stays free of any metrics library: the
//! engine records at the `chat` and tool-exec seams through this trait, the real
//! OpenTelemetry `Meter` implementation lives in `awaken-observability`, and the
//! default is a no-op. Only **structure** is recorded — model id, tool id,
//! outcome code, latency, token counts — never prompt/completion content
//! (metrics are the structure data class under GDPR, ADR-0050). Labels must
//! never carry PII: no prompt-derived values, no raw subject id.

use std::time::Duration;

/// The structure-only facts of one completed inference (`chat`) call. `outcome`
/// is `"ok"` or the error's stable snake_case classification code; token counts
/// are present only when the provider reported usage.
#[derive(Debug, Clone, Copy)]
pub struct InferenceMetric<'a> {
    /// The bound model id — the routing identity, never content.
    pub model: &'a str,
    /// `"ok"` on success, else the `llm::Error::code()` classification.
    pub outcome: &'a str,
    /// Wall-clock time the whole call (including retries) took.
    pub duration: Duration,
    /// Prompt tokens, when the provider reported usage.
    pub input_tokens: Option<u64>,
    /// Completion tokens, when the provider reported usage.
    pub output_tokens: Option<u64>,
}

/// Records structure-only metrics for the runtime's model/tool chokepoints. The
/// **one** injection seam: swap the impl to change where metrics go. Held by the
/// `Runtime`; consulted right where the `chat` span is emitted so metrics and
/// traces share one instrumentation point (no double-instrumentation).
pub trait MetricsRecorder: Send + Sync {
    /// One completed model-inference (`chat`) call.
    fn record_inference(&self, metric: InferenceMetric<'_>);

    /// One completed tool execution: `tool` id, `outcome` (`"ok"` or an error
    /// class), and the wall-clock `duration`.
    fn record_tool(&self, tool: &str, outcome: &str, duration: Duration);

    /// One durable dispatch claimed for execution by a worker (the
    /// `enqueue → claim → drive` seam). Default no-op so an impl that only cares
    /// about model/tool metrics needs no change; the durable dispatch worker
    /// consults the same injected recorder, so its counters export on the one
    /// OTLP pipeline with no new wiring.
    fn record_dispatch_claimed(&self) {}

    /// One durable dispatch settled by a worker: `outcome` is `"done"` (the run
    /// ended) or `"awaiting"` (it carries a resume ticket). Default
    /// no-op.
    fn record_dispatch_settled(&self, outcome: &str) {
        let _ = outcome;
    }

    /// Wall-clock `duration` a worker spent driving one claimed dispatch to a
    /// settled outcome. Default no-op.
    fn record_dispatch_drive(&self, duration: Duration) {
        let _ = duration;
    }

    /// Exact claimable backlog observed from the dispatch authority.
    fn record_dispatch_queue_depth(&self, depth: u64) {
        let _ = depth;
    }

    /// One expired lease reclaimed by a replacement worker.
    fn record_dispatch_recovered(&self) {}

    /// Result and duration of committing a dispatch settlement. `outcome` is
    /// `"applied"`, `"fenced"`, or `"error"`.
    fn record_dispatch_commit(&self, outcome: &str, duration: Duration) {
        let _ = (outcome, duration);
    }

    /// One stale settlement rejected by the monotone claim epoch.
    fn record_dispatch_fenced(&self) {}

    /// Change in currently driven claims (`+1` on entry, `-1` on every exit).
    fn record_dispatch_in_flight(&self, delta: i64) {
        let _ = delta;
    }

    /// A per-model circuit-breaker state transition: `to_state` is `"open"`,
    /// `"half_open"`, or `"closed"`. Emitted for a transition an operator cannot
    /// otherwise see — notably an abandoned half-open probe reopening the circuit,
    /// which produces no inference outcome. Default no-op.
    fn record_circuit_transition(&self, model: &str, to_state: &str) {
        let _ = (model, to_state);
    }
}

/// Null-object recorder: records nothing. The default for the single-machine /
/// open build and every test that does not assert on metrics; real emission
/// (`OtelMetricsRecorder`) lives in `awaken-observability`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRecorder;

impl MetricsRecorder for NoopRecorder {
    fn record_inference(&self, _metric: InferenceMetric<'_>) {}
    fn record_tool(&self, _tool: &str, _outcome: &str, _duration: Duration) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_records_nothing_and_is_a_zst() {
        assert_eq!(std::mem::size_of::<NoopRecorder>(), 0);
        // Exercising both methods must not panic and returns unit.
        NoopRecorder.record_inference(InferenceMetric {
            model: "m",
            outcome: "ok",
            duration: Duration::from_millis(1),
            input_tokens: Some(3),
            output_tokens: Some(4),
        });
        NoopRecorder.record_tool("t", "ok", Duration::from_millis(1));
    }
}
