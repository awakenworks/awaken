//! Trace + eval-run store wiring for the starter demo backend.
//!
//! Both stores persist to the same `--storage-dir` the rest of the
//! starter uses. Kept here so the boot path in `mod.rs` stays focused
//! on agent / runtime wiring; this file owns the integration with
//! `awaken_ext_observability` (traces) and `awaken_eval` (runs).

use std::path::Path;
use std::sync::Arc;

use awaken_eval::{EvalRunStore, FileEvalRunStore};
use awaken_ext_observability::trace_store::TraceStore;
use awaken_ext_observability::trace_store::TraceStoreSink;
use awaken_ext_observability::trace_store::file::FileTraceStore;
use awaken_ext_observability::{
    CompositeSink, ContentCapture, InMemorySink, MetricsSink, ObservabilityPlugin,
    RuntimeStatsRegistry,
};

/// Build the starter demo's observability stack: composite sink with
/// in-memory + runtime-stats + trace store sinks, and an
/// `ObservabilityPlugin` configured for content capture so trace →
/// fixture curation succeeds. Returns the `TraceStore` separately so
/// `ServerState.trace` can read what the sink wrote.
pub fn build_observability(
    storage_dir: &Path,
    runtime_stats: Arc<RuntimeStatsRegistry>,
    model: &str,
) -> (Arc<dyn TraceStore>, ObservabilityPlugin) {
    let dir = storage_dir.join("traces");
    let trace_store: Arc<dyn TraceStore> =
        Arc::new(FileTraceStore::new(&dir).expect("FileTraceStore::new"));
    let sinks: Vec<Arc<dyn MetricsSink>> = vec![
        Arc::new(InMemorySink::new()),
        runtime_stats,
        Arc::new(TraceStoreSink::new(trace_store.clone())),
    ];
    let plugin = ObservabilityPlugin::new(CompositeSink::new(sinks))
        .with_model(model)
        .with_provider("default")
        .with_content_capture(ContentCapture::Enabled);
    (trace_store, plugin)
}

/// Build a file-backed `EvalRunStore` rooted at `<storage_dir>`
/// (`FileEvalRunStore` shards into `<root>/eval_runs/<yyyy-mm>/...`).
pub fn build_eval_run_store(storage_dir: &Path) -> Arc<dyn EvalRunStore> {
    Arc::new(FileEvalRunStore::new(storage_dir).expect("FileEvalRunStore::new"))
}

/// Admin API config that opts the starter demo into the trace-query
/// routes. **Off by default** — trace payloads contain prompts, tool
/// arguments, and assistant responses (plus anything secret a user
/// pasted into a prompt). The starter only enables them when
/// `AWAKEN_EXPOSE_TRACE_ROUTES=true` is set, so a casually-shared
/// admin demo doesn't end up exposing user content. Operators who
/// need the Recent Traces drawer flip the env var explicitly.
pub fn admin_api_config_with_traces() -> awaken_server::app::AdminApiConfig {
    let expose_trace_routes = std::env::var("AWAKEN_EXPOSE_TRACE_ROUTES")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if expose_trace_routes {
        tracing::warn!(
            "AWAKEN_EXPOSE_TRACE_ROUTES=true → /v1/traces/* exposed. Prompts \
             and tool args are now visible to anyone with the admin token."
        );
    }
    awaken_server::app::AdminApiConfig {
        expose_trace_routes,
        ..awaken_server::app::AdminApiConfig::default()
    }
}
