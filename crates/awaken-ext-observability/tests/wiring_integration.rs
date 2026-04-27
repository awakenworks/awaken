//! Integration tests for the env-driven sink wiring helpers.
//!
//! Unit tests in `src/wiring.rs` cover composition logic in isolation;
//! these tests exercise the public API across crate boundaries: building a
//! plugin from [`WiringSettings`], assembling composite sinks via
//! [`install_default_sinks`], and verifying the assembled sinks accept
//! events without panicking.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_ext_observability::{
    AgentMetrics, DelegationSpan, GenAISpan, HandoffSpan, MetricsEvent, MetricsSink,
    OBSERVABILITY_PLUGIN_ID, ObservabilityPlugin, SinkError, SpanContext, SuspensionSpan, ToolSpan,
    WiringSettings, install_default_sinks, observability_plugin_from,
};
use awaken_runtime::plugins::Plugin;

fn temp_dir(suffix: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("awaken-wiring-int-{suffix}-{now}"))
}

fn sample_inference() -> GenAISpan {
    GenAISpan {
        context: SpanContext::default(),
        step_index: None,
        model: "wiring-int-model".into(),
        provider: "wiring-int".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: Vec::new(),
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: Vec::new(),
        duration_ms: 1,
    }
}

fn sample_tool() -> ToolSpan {
    ToolSpan {
        context: SpanContext::default(),
        step_index: None,
        name: "wiring-int-tool".into(),
        operation: "execute_tool".into(),
        call_id: "call-int".into(),
        tool_type: "function".into(),
        error_type: None,
        duration_ms: 1,
    }
}

fn sample_suspension() -> SuspensionSpan {
    SuspensionSpan {
        context: SpanContext::default(),
        tool_call_id: "c1".into(),
        tool_name: "search".into(),
        action: "suspended".into(),
        resume_mode: None,
        duration_ms: None,
        timestamp_ms: 1,
    }
}

fn sample_handoff() -> HandoffSpan {
    HandoffSpan {
        context: SpanContext::default(),
        from_agent_id: "a".into(),
        to_agent_id: "b".into(),
        reason: None,
        timestamp_ms: 1,
    }
}

fn sample_delegation() -> DelegationSpan {
    DelegationSpan {
        context: SpanContext::default(),
        parent_run_id: "p".into(),
        child_run_id: Some("c".into()),
        target_agent_id: "worker".into(),
        tool_call_id: "c1".into(),
        duration_ms: Some(1),
        success: true,
        error_message: None,
        timestamp_ms: 1,
    }
}

// ── Plugin construction paths ─────────────────────────────────────────

#[test]
fn plugin_from_minimal_settings_exposes_observability_descriptor() {
    let (plugin, summary) = observability_plugin_from(&WiringSettings::default());
    let plugin = plugin.expect("plugin built");
    assert!(summary.in_memory);
    assert!(!summary.disabled);
    assert_eq!(plugin.descriptor().name, OBSERVABILITY_PLUGIN_ID);
}

#[test]
fn plugin_from_disabled_settings_returns_none() {
    let (plugin, summary) = observability_plugin_from(&WiringSettings {
        disabled: true,
        ..WiringSettings::default()
    });
    assert!(plugin.is_none(), "disabled wiring must not yield a plugin");
    assert!(summary.disabled);
}

#[test]
fn plugin_from_prometheus_settings_exposes_descriptor() {
    let (plugin, summary) = observability_plugin_from(&WiringSettings {
        prometheus: true,
        ..WiringSettings::default()
    });
    let plugin = plugin.expect("plugin built");
    assert!(summary.prometheus);
    assert_eq!(plugin.descriptor().name, OBSERVABILITY_PLUGIN_ID);
}

#[test]
fn plugin_from_settings_can_chain_with_model_provider() {
    let (plugin, _) = observability_plugin_from(&WiringSettings::default());
    let plugin = plugin
        .expect("plugin built")
        .with_model("gpt-4o-mini")
        .with_provider("openai")
        .with_temperature(0.4)
        .with_max_tokens(2048);
    assert_eq!(plugin.descriptor().name, OBSERVABILITY_PLUGIN_ID);
}

// ── Composite fan-out via install_default_sinks ──────────────────────

#[test]
fn install_minimal_topology_is_just_in_memory() {
    let (sink, summary) = install_default_sinks(&WiringSettings::default());
    assert!(summary.in_memory);
    assert!(!summary.prometheus);
    sink.record(&MetricsEvent::Inference(sample_inference()));
    sink.flush().unwrap();
    sink.shutdown().unwrap();
}

#[test]
fn install_with_persistent_dir_creates_writable_directory() {
    let dir = temp_dir("persistent-int");
    let (sink, summary) = install_default_sinks(&WiringSettings {
        persistent_sink_dir: Some(dir.clone()),
        ..WiringSettings::default()
    });
    assert_eq!(summary.persistent_dir.as_ref(), Some(&dir));
    assert!(dir.exists());
    sink.flush().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn install_persistent_plus_prometheus_marks_both_in_summary() {
    let dir = temp_dir("persistent-prom-int");
    let (_sink, summary) = install_default_sinks(&WiringSettings {
        prometheus: true,
        persistent_sink_dir: Some(dir.clone()),
        ..WiringSettings::default()
    });
    assert!(summary.prometheus);
    assert_eq!(summary.persistent_dir.as_ref(), Some(&dir));
    assert!(summary.in_memory);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn install_persistent_with_invalid_dir_falls_back_gracefully() {
    let bad = PathBuf::from("\u{0}/not/a/real/dir");
    let (sink, summary) = install_default_sinks(&WiringSettings {
        persistent_sink_dir: Some(bad),
        ..WiringSettings::default()
    });
    assert!(summary.persistent_dir.is_none());
    assert!(summary.in_memory);
    sink.flush().unwrap();
}

#[test]
fn install_default_sinks_accepts_all_event_kinds() {
    let (sink, _) = install_default_sinks(&WiringSettings {
        prometheus: true,
        ..WiringSettings::default()
    });
    sink.record(&MetricsEvent::Inference(sample_inference()));
    sink.record(&MetricsEvent::Tool(sample_tool()));
    sink.record(&MetricsEvent::Suspension(sample_suspension()));
    sink.record(&MetricsEvent::Handoff(sample_handoff()));
    sink.record(&MetricsEvent::Delegation(sample_delegation()));
    sink.on_run_end(&AgentMetrics {
        session_duration_ms: 42,
        ..Default::default()
    });
    sink.flush().unwrap();
    sink.shutdown().unwrap();
}

// ── ObservabilityPlugin ↔ user-supplied sink ─────────────────────────

#[test]
fn plugin_built_with_counting_sink_does_not_drop_it() {
    let counter = Arc::new(CountingSink::default());
    let counter_clone = Arc::clone(&counter);

    // Construct a plugin directly with the counting sink. The plugin owns
    // the sink internally — the external Arc keeps a parallel handle so
    // we can observe state changes without touching plugin internals.
    let plugin = ObservabilityPlugin::new(SinkProxy(counter as Arc<dyn MetricsSink>));
    assert_eq!(plugin.descriptor().name, OBSERVABILITY_PLUGIN_ID);

    // No events have flowed through phase hooks (those require the runtime
    // engine), so the counter should still be zero.
    assert_eq!(counter_clone.inference.load(Ordering::SeqCst), 0);
    assert_eq!(counter_clone.tool.load(Ordering::SeqCst), 0);

    // The plugin holding the sink must keep the counter alive.
    assert!(Arc::strong_count(&counter_clone) >= 2);
}

#[test]
fn install_default_sinks_keeps_in_memory_path_alive() {
    let (sink, summary) = install_default_sinks(&WiringSettings::default());
    assert!(summary.in_memory);
    // Drop the returned Arc and confirm sink still does not panic on a
    // fresh clone path: this exercises the Arc cloning the wiring helper
    // performs internally for composites.
    let cloned = Arc::clone(&sink);
    drop(sink);
    cloned.record(&MetricsEvent::Inference(sample_inference()));
    cloned.on_run_end(&AgentMetrics::default());
}

#[derive(Default)]
struct CountingSink {
    inference: AtomicUsize,
    tool: AtomicUsize,
}

impl MetricsSink for CountingSink {
    fn record(&self, event: &MetricsEvent) {
        match event {
            MetricsEvent::Inference(_) => {
                self.inference.fetch_add(1, Ordering::SeqCst);
            }
            MetricsEvent::Tool(_) => {
                self.tool.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    fn on_run_end(&self, _metrics: &AgentMetrics) {}
}

/// Adapter to pass an `Arc<dyn MetricsSink>` into
/// `ObservabilityPlugin::new(impl MetricsSink + 'static)`.
struct SinkProxy(Arc<dyn MetricsSink>);

impl MetricsSink for SinkProxy {
    fn record(&self, event: &MetricsEvent) {
        self.0.record(event);
    }
    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.0.on_run_end(metrics);
    }
    fn flush(&self) -> Result<(), SinkError> {
        self.0.flush()
    }
    fn shutdown(&self) -> Result<(), SinkError> {
        self.0.shutdown()
    }
}
