//! LLM telemetry plugin aligned with OpenTelemetry GenAI Semantic Conventions.
//!
//! Captures per-inference and per-tool metrics via the Phase system,
//! forwarding them to a pluggable [`MetricsSink`].

mod batching;
mod composite;
mod metrics;
mod persistent;
mod plugin;
mod prometheus;
pub mod runtime_stats;
pub mod sampling;
mod sink;
mod stats;
pub mod trace_store;
mod wiring;

#[cfg(feature = "otel")]
pub mod otel;
#[cfg(feature = "otel")]
mod otel_config;

pub use batching::{BatchingConfig, BatchingSink};
pub use composite::{CompositeSink, CompositeSinkBuilder};
pub use metrics::{
    AgentMetrics, BackgroundTaskSpan, ContentCapture, DelegationSpan, EvaluationResultEvent,
    GenAISpan, HandoffSpan, MetricsEvent, SpanContext, SuspensionSpan, ToolIoCapture, ToolSpan,
};
pub use persistent::{PersistenceConfig, PersistentSink};
pub use plugin::{OBSERVABILITY_PLUGIN_ID, ObservabilityPlugin};
pub use prometheus::PrometheusSink;
pub use runtime_stats::{
    AgentRuntimeSnapshot, DEFAULT_BUCKET_COUNT, DEFAULT_BUCKET_WINDOW, DEFAULT_DURATION_BUCKETS_MS,
    HistogramBucket, RuntimeStatsRegistry, ToolRuntimeStats,
};
pub use sink::{InMemorySink, MetricsSink, SinkError};
pub use wiring::{
    WiringSettings, WiringSummary, install_default_sinks, install_default_sinks_from_env,
    observability_plugin_from, observability_plugin_from_env,
    observability_plugin_from_env_with_summary,
};

#[cfg(feature = "otel")]
pub use otel::OtelMetricsSink;
#[cfg(feature = "otel")]
pub use otel_config::{OtelConfig, OtelConfigBuilder, OtelProtocol};
pub use stats::{AgentToolStats, ModelStats, ToolStats};

// Make private helpers visible to the test module below.
#[cfg(test)]
use plugin::{extract_cache_tokens, extract_token_counts};

#[cfg(test)]
mod attribution_fills_tests;
#[cfg(test)]
mod genai_backfill_tests;

#[cfg(test)]
#[path = "lib_test.rs"]
mod tests;
