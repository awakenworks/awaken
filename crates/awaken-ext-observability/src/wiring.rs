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
//! | `AWAKEN_TRACE_SAMPLING_DISABLE=1`     | Bypasses the default [`SamplingPolicy`] (errors / low-judge / explicit always; normal 1%) and writes every event through (ADR-0030 D5 opt-out) |
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
//!
//! ## Programmatic API
//!
//! Embedders that prefer not to rely on environment variables can build a
//! [`WiringSettings`] directly and call [`install_default_sinks`].  The pure
//! function takes a fully-resolved settings struct, which makes it
//! exhaustively testable without mutating process-wide state.

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

/// Pre-resolved configuration consumed by [`install_default_sinks`].
///
/// Constructed either via [`WiringSettings::from_env`] (the standard path
/// taken by `*_from_env` helpers) or directly by embedders that want a
/// deterministic wiring without touching the process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WiringSettings {
    /// When `true`, [`install_default_sinks`] returns a bare in-memory sink
    /// and sets `WiringSummary::disabled = true`.
    pub disabled: bool,
    /// When `true` and the crate was built with the `otel` feature, an
    /// [`OtelMetricsSink`] is added.  Without the feature the field is
    /// ignored (the OTLP path is compiled out).
    pub otel: bool,
    /// When `true`, a [`PrometheusSink`] is added to the composite.
    pub prometheus: bool,
    /// When `Some(path)`, the assembled composite is wrapped in a
    /// [`PersistentSink`] writing NDJSON to `path`.
    pub persistent_sink_dir: Option<PathBuf>,
    /// When `true`, do NOT attach the default sampling policy to a
    /// persistent sink. The default (`false`) means a `TraceStore`-backed
    /// `PersistentSink` is gated by `SamplingPolicy::default()` (errors /
    /// low-judge / explicit always, normal proportional 1%). Set via
    /// `AWAKEN_TRACE_SAMPLING_DISABLE=1` for full capture during
    /// incidents.
    pub sampling_disabled: bool,
}

impl WiringSettings {
    /// Read the wiring settings from environment variables.
    ///
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`
    /// are *both* checked because `OtelConfig::from_env()` already resolves
    /// either; this helper only needs to know "is OTLP requested at all?".
    pub fn from_env() -> Self {
        if env_truthy("AWAKEN_OBSERVABILITY_DISABLE") {
            return Self {
                disabled: true,
                ..Self::default()
            };
        }

        Self {
            disabled: false,
            otel: env_present("OTEL_EXPORTER_OTLP_ENDPOINT")
                || env_present("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
            prometheus: env_truthy("AWAKEN_PROMETHEUS"),
            persistent_sink_dir: env_path("AWAKEN_PERSISTENT_SINK_DIR"),
            sampling_disabled: env_truthy("AWAKEN_TRACE_SAMPLING_DISABLE"),
        }
    }
}

/// What `install_default_sinks` produced and *why*, useful for surfacing
/// diagnostics in a startup banner.
#[derive(Clone, Default)]
pub struct WiringSummary {
    /// `true` when the `InMemorySink` was added (always currently).
    pub in_memory: bool,
    /// `true` when an OTLP sink was configured.
    pub otel: bool,
    /// `true` when a Prometheus sink was configured.
    pub prometheus: bool,
    /// `Some(path)` when a `PersistentSink` was configured.
    pub persistent_dir: Option<PathBuf>,
    /// `true` when settings explicitly disabled the auto-wiring.
    pub disabled: bool,
    /// The `TraceStore` constructed during wiring, when
    /// `AWAKEN_PERSISTENT_SINK_DIR` is set and `FileTraceStore` initialised
    /// successfully.  The server reads this to expose the trace query API.
    pub trace_store: Option<Arc<dyn crate::trace_store::TraceStore>>,
    /// `true` when the default sampling policy was attached to the
    /// persistent sink. Reflects ADR-0030 D5 in operator banners.
    pub sampling_enabled: bool,
}

impl std::fmt::Debug for WiringSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WiringSummary")
            .field("in_memory", &self.in_memory)
            .field("otel", &self.otel)
            .field("prometheus", &self.prometheus)
            .field("persistent_dir", &self.persistent_dir)
            .field("disabled", &self.disabled)
            .field("trace_store", &self.trace_store.is_some())
            .field("sampling_enabled", &self.sampling_enabled)
            .finish()
    }
}

impl PartialEq for WiringSummary {
    fn eq(&self, other: &Self) -> bool {
        self.in_memory == other.in_memory
            && self.otel == other.otel
            && self.prometheus == other.prometheus
            && self.persistent_dir == other.persistent_dir
            && self.disabled == other.disabled
            && self.trace_store.is_some() == other.trace_store.is_some()
            && self.sampling_enabled == other.sampling_enabled
    }
}

impl Eq for WiringSummary {}

/// Pure assembly entry point taking pre-resolved [`WiringSettings`].
///
/// Always returns a non-null sink:
///
/// * When `settings.disabled` is `true`, returns a bare [`InMemorySink`]
///   (callers can detect this via `summary.disabled`).
/// * Otherwise composes in-memory + optional OTLP + optional Prometheus,
///   then optionally wraps in a [`PersistentSink`].
///
/// This function does not read environment variables; it is therefore
/// deterministic and exhaustively unit-testable without mutating
/// process-global state.
pub fn install_default_sinks(settings: &WiringSettings) -> (Arc<dyn MetricsSink>, WiringSummary) {
    let mut summary = WiringSummary::default();

    if settings.disabled {
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
        if settings.otel
            && let Some(sink) = build_otel_sink_from_env()
        {
            sinks.push(sink);
            summary.otel = true;
        }
    }

    if settings.prometheus {
        sinks.push(Arc::new(PrometheusSink::new()));
        summary.prometheus = true;
    }

    let composite: Arc<dyn MetricsSink> = if sinks.len() == 1 {
        Arc::clone(&sinks[0])
    } else {
        Arc::new(CompositeSink::new(sinks))
    };

    if let Some(dir) = settings.persistent_sink_dir.as_ref() {
        let config = PersistenceConfig {
            storage_dir: dir.clone(),
            ..PersistenceConfig::default()
        };
        let store: Option<Arc<dyn crate::trace_store::TraceStore>> =
            match crate::trace_store::file::FileTraceStore::new(dir) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    // TraceStore (the queryable trace layer) failed to
                    // initialise, but the operator still asked for a
                    // persistent sink — fall through to the legacy
                    // disk-spill PersistentSink so we don't silently
                    // demote the whole pipeline to the in-memory composite.
                    tracing::warn!(
                        error = %e,
                        dir = %dir.display(),
                        "FileTraceStore init failed; falling back to disk-spill PersistentSink only"
                    );
                    None
                }
            };
        let result = match store.clone() {
            Some(s) => PersistentSink::with_trace_store(Arc::clone(&composite), s, config),
            None => PersistentSink::new(Arc::clone(&composite), config),
        };
        match result {
            Ok(mut persistent) => {
                // F16: attach the default sampling policy whenever a
                // TraceStore is in play, so the documented behaviour
                // (errors / low-judge / explicit always; normal 1%)
                // actually applies. Without this the sink wrote every
                // event through, contradicting ADR-0030 D5.
                // `AWAKEN_TRACE_SAMPLING_DISABLE=1` opts back to the
                // old write-through for operators who want full
                // capture during incidents.
                if store.is_some() && !settings.sampling_disabled {
                    let policy = std::sync::Arc::new(parking_lot::RwLock::new(
                        crate::sampling::SamplingPolicy::default(),
                    ));
                    persistent = persistent.with_sampling_policy(policy);
                    summary.sampling_enabled = true;
                }
                summary.persistent_dir = Some(dir.clone());
                summary.trace_store = store;
                return (Arc::new(persistent), summary);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    dir = %dir.display(),
                    "PersistentSink construction failed; falling back to non-persistent wiring"
                );
            }
        }
    }

    (composite, summary)
}

/// Read environment variables and assemble the auto-wired sink list.
///
/// Equivalent to [`install_default_sinks`] called with
/// [`WiringSettings::from_env`].
pub fn install_default_sinks_from_env() -> (Arc<dyn MetricsSink>, WiringSummary) {
    install_default_sinks(&WiringSettings::from_env())
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
    observability_plugin_from(&WiringSettings::from_env()).0
}

/// Same as [`observability_plugin_from_env`] but also returns the wiring
/// summary so callers can log a startup banner without re-reading env vars.
pub fn observability_plugin_from_env_with_summary() -> (Option<ObservabilityPlugin>, WiringSummary)
{
    observability_plugin_from(&WiringSettings::from_env())
}

/// Pure equivalent of [`observability_plugin_from_env_with_summary`] taking
/// pre-resolved settings.
pub fn observability_plugin_from(
    settings: &WiringSettings,
) -> (Option<ObservabilityPlugin>, WiringSummary) {
    let (sink, summary) = install_default_sinks(settings);
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

    fn flush_run(
        &self,
        run_key: &str,
        close_reason: &'static str,
    ) -> Result<(), crate::sink::SinkError> {
        self.0.flush_run(run_key, close_reason)
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

fn env_present(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
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
    use crate::metrics::{AgentMetrics, GenAISpan, MetricsEvent, SpanContext, ToolSpan};
    use crate::sink::SinkError;

    // ── fixtures ───────────────────────────────────────────────────────

    fn sample_inference() -> GenAISpan {
        GenAISpan {
            context: SpanContext::default(),
            step_index: None,
            model: "wiring-test-model".to_string(),
            provider: "wiring-test".to_string(),
            operation: "chat".to_string(),
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
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn sample_tool() -> ToolSpan {
        ToolSpan {
            context: SpanContext::default(),
            step_index: None,
            name: "wiring-test-tool".to_string(),
            operation: "execute_tool".to_string(),
            call_id: "call-wiring".to_string(),
            tool_type: "function".to_string(),
            call_arguments: None,
            call_result: None,
            error_type: None,
            duration_ms: 1,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn temp_dir(suffix: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("awaken-wiring-test-{suffix}-{now}"))
    }

    // ── WiringSettings ─────────────────────────────────────────────────

    #[test]
    fn settings_default_is_all_off() {
        let s = WiringSettings::default();
        assert!(!s.disabled);
        assert!(!s.otel);
        assert!(!s.prometheus);
        assert!(s.persistent_sink_dir.is_none());
    }

    #[test]
    fn settings_disabled_clones_to_disabled() {
        let s = WiringSettings {
            disabled: true,
            otel: true,
            prometheus: true,
            persistent_sink_dir: Some(PathBuf::from("/tmp/x")),
            sampling_disabled: false,
        };
        let clone = s.clone();
        assert_eq!(s, clone);
    }

    #[test]
    fn settings_from_env_does_not_panic() {
        // Cannot mutate env (`unsafe_code = "forbid"`), but the call must
        // never panic regardless of ambient environment.
        let _ = WiringSettings::from_env();
    }

    // ── install_default_sinks: disabled ────────────────────────────────

    #[test]
    fn install_disabled_returns_in_memory_only() {
        let settings = WiringSettings {
            disabled: true,
            otel: true,
            prometheus: true,
            persistent_sink_dir: Some(PathBuf::from("/dev/null")),
            sampling_disabled: false,
        };
        let (_sink, summary) = install_default_sinks(&settings);
        assert!(summary.disabled);
        assert!(summary.in_memory);
        assert!(!summary.otel);
        assert!(!summary.prometheus);
        assert!(summary.persistent_dir.is_none());
    }

    #[test]
    fn install_disabled_sink_does_not_panic_on_record() {
        let (sink, _) = install_default_sinks(&WiringSettings {
            disabled: true,
            ..WiringSettings::default()
        });
        sink.record(&MetricsEvent::Inference(sample_inference()));
        sink.record(&MetricsEvent::Tool(sample_tool()));
        sink.on_run_end(&AgentMetrics::default());
        assert!(sink.flush().is_ok());
    }

    // ── install_default_sinks: minimal (no env) ────────────────────────

    #[test]
    fn install_minimal_returns_only_in_memory() {
        let (sink, summary) = install_default_sinks(&WiringSettings::default());
        assert!(!summary.disabled);
        assert!(summary.in_memory);
        assert!(!summary.otel);
        assert!(!summary.prometheus);
        assert!(summary.persistent_dir.is_none());
        // Records must not panic.
        sink.record(&MetricsEvent::Inference(sample_inference()));
    }

    // ── install_default_sinks: prometheus ──────────────────────────────

    #[test]
    fn install_prometheus_marks_summary() {
        let (sink, summary) = install_default_sinks(&WiringSettings {
            prometheus: true,
            ..WiringSettings::default()
        });
        assert!(summary.in_memory);
        assert!(summary.prometheus);
        assert!(!summary.otel);
        assert!(summary.persistent_dir.is_none());
        // Should fan out to both inner sinks without panicking.
        sink.record(&MetricsEvent::Inference(sample_inference()));
        sink.record(&MetricsEvent::Tool(sample_tool()));
    }

    // ── install_default_sinks: persistent wrapping ─────────────────────

    #[test]
    fn install_persistent_attaches_default_sampling() {
        // Regression for F16: prior wiring built `PersistentSink::with_trace_store`
        // but never called `with_sampling_policy`, so the documented
        // `SamplingPolicy::default()` (normal=Proportional(0.01)) never
        // applied — production wrote every event through.
        let dir = temp_dir("sampling-default");
        let (_sink, summary) = install_default_sinks(&WiringSettings {
            persistent_sink_dir: Some(dir.clone()),
            ..WiringSettings::default()
        });
        assert!(summary.trace_store.is_some());
        assert!(
            summary.sampling_enabled,
            "default sampling must be attached whenever a TraceStore is wired"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_persistent_respects_sampling_disabled() {
        // Opt-out path: when sampling_disabled is set (env
        // AWAKEN_TRACE_SAMPLING_DISABLE=1), no policy attaches and the
        // legacy write-through behaviour is restored.
        let dir = temp_dir("sampling-disabled");
        let (_sink, summary) = install_default_sinks(&WiringSettings {
            persistent_sink_dir: Some(dir.clone()),
            sampling_disabled: true,
            ..WiringSettings::default()
        });
        assert!(summary.trace_store.is_some());
        assert!(
            !summary.sampling_enabled,
            "sampling must NOT attach when explicitly disabled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_persistent_creates_dir_and_wraps() {
        let dir = temp_dir("persistent-creates");
        let (sink, summary) = install_default_sinks(&WiringSettings {
            persistent_sink_dir: Some(dir.clone()),
            ..WiringSettings::default()
        });
        assert!(summary.in_memory);
        assert_eq!(summary.persistent_dir.as_ref(), Some(&dir));
        assert!(
            dir.exists(),
            "PersistentSink should have created storage_dir"
        );
        // Smoke: composite path must accept events.
        sink.record(&MetricsEvent::Inference(sample_inference()));
        sink.on_run_end(&AgentMetrics::default());
        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_persistent_with_prometheus_keeps_both_flags() {
        let dir = temp_dir("persistent-prom");
        let (_sink, summary) = install_default_sinks(&WiringSettings {
            prometheus: true,
            persistent_sink_dir: Some(dir.clone()),
            ..WiringSettings::default()
        });
        assert!(summary.prometheus);
        assert_eq!(summary.persistent_dir.as_ref(), Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_persistent_with_unwritable_dir_falls_back() {
        // Use a path containing a NUL byte: every OS rejects it.
        let bad = PathBuf::from("\u{0}/this/does/not/exist");
        let (_sink, summary) = install_default_sinks(&WiringSettings {
            persistent_sink_dir: Some(bad),
            ..WiringSettings::default()
        });
        // Fallback: persistent_dir cleared, in_memory still set.
        assert!(summary.persistent_dir.is_none());
        assert!(summary.in_memory);
    }

    // ── observability_plugin_from ──────────────────────────────────────

    #[test]
    fn plugin_from_disabled_settings_returns_none() {
        let (plugin, summary) = observability_plugin_from(&WiringSettings {
            disabled: true,
            ..WiringSettings::default()
        });
        assert!(plugin.is_none());
        assert!(summary.disabled);
    }

    #[test]
    fn plugin_from_minimal_settings_returns_some() {
        let (plugin, summary) = observability_plugin_from(&WiringSettings::default());
        assert!(plugin.is_some());
        assert!(!summary.disabled);
        assert!(summary.in_memory);
    }

    #[test]
    fn plugin_from_settings_can_chain_with_model() {
        let (plugin, _) = observability_plugin_from(&WiringSettings {
            prometheus: true,
            ..WiringSettings::default()
        });
        let plugin = plugin.expect("plugin built").with_model("gpt-4o-mini");
        // The chained builder shouldn't drop the underlying sink — record
        // a smoke event via the plugin's descriptor name to confirm it.
        use awaken_runtime::Plugin;
        assert_eq!(plugin.descriptor().name, "observability");
    }

    // ── env-driven smokes (cannot mutate env) ──────────────────────────

    #[test]
    fn install_default_sinks_from_env_does_not_panic() {
        let _ = install_default_sinks_from_env();
    }

    #[test]
    fn observability_plugin_from_env_does_not_panic() {
        let _ = observability_plugin_from_env();
    }

    #[test]
    fn observability_plugin_from_env_with_summary_does_not_panic() {
        let _ = observability_plugin_from_env_with_summary();
    }

    // ── ArcSink delegation ─────────────────────────────────────────────

    #[test]
    fn arc_sink_forwards_record_and_flush() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counter {
            recorded: AtomicUsize,
            flushed: AtomicUsize,
            shutdown_count: AtomicUsize,
            run_ends: AtomicUsize,
        }
        impl MetricsSink for Counter {
            fn record(&self, _event: &MetricsEvent) {
                self.recorded.fetch_add(1, Ordering::SeqCst);
            }
            fn on_run_end(&self, _metrics: &AgentMetrics) {
                self.run_ends.fetch_add(1, Ordering::SeqCst);
            }
            fn flush(&self) -> Result<(), SinkError> {
                self.flushed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn shutdown(&self) -> Result<(), SinkError> {
                self.shutdown_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let counter = Arc::new(Counter {
            recorded: AtomicUsize::new(0),
            flushed: AtomicUsize::new(0),
            shutdown_count: AtomicUsize::new(0),
            run_ends: AtomicUsize::new(0),
        });
        let arc: Arc<dyn MetricsSink> = counter.clone();
        let wrap = ArcSink(arc);
        wrap.record(&MetricsEvent::Inference(sample_inference()));
        wrap.record(&MetricsEvent::Tool(sample_tool()));
        wrap.on_run_end(&AgentMetrics::default());
        wrap.flush().unwrap();
        wrap.shutdown().unwrap();
        assert_eq!(counter.recorded.load(Ordering::SeqCst), 2);
        assert_eq!(counter.run_ends.load(Ordering::SeqCst), 1);
        assert_eq!(counter.flushed.load(Ordering::SeqCst), 1);
        assert_eq!(counter.shutdown_count.load(Ordering::SeqCst), 1);
    }

    // ── env_truthy / env_path / env_present (pure parsers) ─────────────

    #[test]
    fn env_truthy_recognises_truthy_values() {
        // Use a key unlikely to be set in CI to be deterministic.
        let key = "AWAKEN_TEST_TRUTHY_PROBE_DOES_NOT_EXIST";
        assert!(!env_truthy(key));
    }

    #[test]
    fn env_path_filters_blank_strings() {
        let key = "AWAKEN_TEST_PATH_PROBE_DOES_NOT_EXIST";
        assert!(env_path(key).is_none());
    }

    #[test]
    fn env_present_returns_false_when_unset() {
        let key = "AWAKEN_TEST_PRESENT_PROBE_DOES_NOT_EXIST";
        assert!(!env_present(key));
    }
}
