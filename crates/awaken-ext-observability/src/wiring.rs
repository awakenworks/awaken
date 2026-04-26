//! Environment-driven sink assembly.
//!
//! These helpers are additive on top of the 0.4.0 `MetricsSink` /
//! `ObservabilityPlugin` API and never change the signatures or behaviour
//! of pre-existing types.  Embedding applications that previously built
//! their own sink topology continue to work unchanged.
//!
//! ## Recognised environment variables
//!
//! | Variable                              | Effect                                                                 |
//! |---------------------------------------|------------------------------------------------------------------------|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` /       | When set (and crate built with `otel`), an [`OtelMetricsSink`] is added |
//! | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`  |                                                                        |
//! | `AWAKEN_PROMETHEUS=1`                 | Adds [`PrometheusSink`]                                                |
//! | `AWAKEN_PERSISTENT_SINK_DIR=<dir>`    | Wraps the composite in a [`PersistentSink`] writing NDJSON to `<dir>` |
//! | `AWAKEN_OBSERVABILITY_DISABLE=1`      | Suppresses all auto-wired sinks (caller-provided sinks unaffected)     |
//!
//! Composition rules:
//!
//! * An [`InMemorySink`] is *always* part of the assembly — its overhead is
//!   negligible and downstream tooling (eval, replay) relies on it.
//! * If `AWAKEN_PERSISTENT_SINK_DIR` is set, every other sink is folded
//!   beneath the persistent wrapper so failed flushes spill to disk.
//! * If nothing else is configured, `from_env()` still returns the in-memory
//!   sink so plugin construction does not fail silently.

use std::path::PathBuf;
use std::sync::Arc;

use crate::composite::CompositeSink;
use crate::persistent::{PersistenceConfig, PersistentSink};
use crate::plugin::ObservabilityPlugin;
use crate::prometheus::PrometheusSink;
use crate::sink::{InMemorySink, MetricsSink};

#[cfg(feature = "otel")]
use crate::otel::{OtelMetricsSink, init_otlp_tracer};
#[cfg(feature = "otel")]
use crate::otel_config::OtelConfig;

/// What `from_env` produced and *why*, useful for surfacing diagnostics in a
/// startup banner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WiringSummary {
    /// `true` when the `InMemorySink` was added (always currently).
    pub in_memory: bool,
    /// `true` when an OTLP sink was configured.
    pub otel: bool,
    /// `true` when a Prometheus sink was configured.
    pub prometheus: bool,
    /// `Some(path)` when a `PersistentSink` was configured.
    pub persistent_dir: Option<PathBuf>,
    /// `true` when `AWAKEN_OBSERVABILITY_DISABLE=1` short-circuited the wiring.
    pub disabled: bool,
}

/// Read environment variables and assemble the auto-wired sink list.
///
/// Returns a tuple of:
///
/// * the composed [`MetricsSink`] (always non-null — falls back to a bare
///   in-memory sink when nothing else is configured), and
/// * a [`WiringSummary`] describing what was added.
///
/// Callers that want to *replace* the wiring should not use this helper;
/// they should build sinks manually with [`CompositeSink`].
pub fn install_default_sinks_from_env() -> (Arc<dyn MetricsSink>, WiringSummary) {
    let mut summary = WiringSummary::default();

    if env_truthy("AWAKEN_OBSERVABILITY_DISABLE") {
        summary.disabled = true;
        let in_memory: Arc<dyn MetricsSink> = Arc::new(InMemorySink::new());
        summary.in_memory = true;
        return (in_memory, summary);
    }

    let mut sinks: Vec<Arc<dyn MetricsSink>> = Vec::new();

    let in_memory: Arc<dyn MetricsSink> = Arc::new(InMemorySink::new());
    summary.in_memory = true;
    sinks.push(Arc::clone(&in_memory));

    #[cfg(feature = "otel")]
    {
        if let Some(sink) = build_otel_sink_from_env() {
            sinks.push(sink);
            summary.otel = true;
        }
    }

    if env_truthy("AWAKEN_PROMETHEUS") {
        sinks.push(Arc::new(PrometheusSink::new()));
        summary.prometheus = true;
    }

    let composite: Arc<dyn MetricsSink> = if sinks.len() == 1 {
        Arc::clone(&sinks[0])
    } else {
        Arc::new(CompositeSink::new(sinks))
    };

    if let Some(dir) = env_path("AWAKEN_PERSISTENT_SINK_DIR") {
        let config = PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        };
        match PersistentSink::new(Arc::clone(&composite), config) {
            Ok(persistent) => {
                summary.persistent_dir = Some(dir);
                return (Arc::new(persistent), summary);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    dir = %dir.display(),
                    "AWAKEN_PERSISTENT_SINK_DIR set but PersistentSink could not be created; \
                     falling back to non-persistent wiring"
                );
            }
        }
    }

    (composite, summary)
}

/// Build an [`ObservabilityPlugin`] from the env-driven sink wiring.
///
/// Returns `Some(plugin)` whenever wiring is *not* disabled via
/// `AWAKEN_OBSERVABILITY_DISABLE=1`.  When disabled, returns `None` so
/// embedders can decide whether to fall back to a bespoke topology.
///
/// The returned plugin uses the assembled composite sink directly; subsequent
/// `with_model` / `with_provider` chaining still works:
///
/// ```ignore
/// if let Some(plugin) = observability_plugin_from_env() {
///     builder.with_plugin("observability", Arc::new(
///         plugin.with_model("gpt-4o-mini").with_provider("openai")
///     ));
/// }
/// ```
pub fn observability_plugin_from_env() -> Option<ObservabilityPlugin> {
    let (sink, summary) = install_default_sinks_from_env();
    if summary.disabled {
        return None;
    }
    Some(ObservabilityPlugin::new(ArcSink(sink)))
}

/// Same as [`observability_plugin_from_env`] but also returns the wiring
/// summary so callers can log a startup banner without re-reading env vars.
pub fn observability_plugin_from_env_with_summary() -> (Option<ObservabilityPlugin>, WiringSummary)
{
    let (sink, summary) = install_default_sinks_from_env();
    if summary.disabled {
        return (None, summary);
    }
    (Some(ObservabilityPlugin::new(ArcSink(sink))), summary)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Newtype that lets us pass an `Arc<dyn MetricsSink>` into
/// `ObservabilityPlugin::new(impl MetricsSink + 'static)` while keeping the
/// caller-side type inference simple.
struct ArcSink(Arc<dyn MetricsSink>);

impl MetricsSink for ArcSink {
    fn record(&self, event: &crate::metrics::MetricsEvent) {
        self.0.record(event);
    }

    fn on_run_end(&self, metrics: &crate::metrics::AgentMetrics) {
        self.0.on_run_end(metrics);
    }

    fn flush(&self) -> Result<(), crate::sink::SinkError> {
        self.0.flush()
    }

    fn shutdown(&self) -> Result<(), crate::sink::SinkError> {
        self.0.shutdown()
    }
}

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .map(|v| v.trim().to_ascii_lowercase()),
        Some(ref v) if v == "1" || v == "true" || v == "yes" || v == "on"
    )
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(feature = "otel")]
fn build_otel_sink_from_env() -> Option<Arc<dyn MetricsSink>> {
    let cfg = OtelConfig::from_env();
    if !cfg.is_configured() {
        return None;
    }
    match init_otlp_tracer(&cfg) {
        Ok((provider, tracer)) => {
            // Provider must outlive the sink — leak it intentionally so
            // batched spans flush at process exit.
            Box::leak(Box::new(provider));
            Some(Arc::new(OtelMetricsSink::new(tracer)))
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "OTEL_EXPORTER_OTLP_ENDPOINT set but tracer init failed; OTLP sink omitted"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiring_summary_default_is_empty() {
        let summary = WiringSummary::default();
        assert!(!summary.in_memory);
        assert!(!summary.otel);
        assert!(!summary.prometheus);
        assert!(!summary.disabled);
        assert!(summary.persistent_dir.is_none());
    }

    #[test]
    fn install_returns_in_memory_when_unconfigured() {
        // Cannot mutate env (`unsafe_code = "forbid"`) — assert structural
        // properties only: a sink is always returned and `in_memory` is set
        // unless explicitly disabled.
        let (sink, summary) = install_default_sinks_from_env();
        // Smoke: recording should not panic.
        sink.record(&crate::metrics::MetricsEvent::Inference(
            crate::metrics::GenAISpan {
                context: crate::metrics::SpanContext::default(),
                step_index: None,
                model: "m".into(),
                provider: "p".into(),
                operation: "chat".into(),
                response_model: None,
                response_id: None,
                finish_reasons: Vec::new(),
                error_type: None,
                error_class: None,
                thinking_tokens: None,
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: Some(3),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                temperature: None,
                top_p: None,
                max_tokens: None,
                stop_sequences: Vec::new(),
                duration_ms: 1,
            },
        ));
        // Either path leaves `in_memory` set: disabled mode still returns
        // a bare in-memory sink so plugin construction never observes None.
        assert!(summary.in_memory);
    }

    #[test]
    fn observability_plugin_from_env_smoke() {
        // Always returns Some unless AWAKEN_OBSERVABILITY_DISABLE is set in
        // the ambient CI environment. Either way the call should not panic.
        let _ = observability_plugin_from_env();
    }
}
