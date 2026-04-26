//! Test helpers for verifying OTLP/HTTP traces against a running
//! [Arize Phoenix](https://github.com/Arize-ai/phoenix) instance.
//!
//! Usage shape:
//!
//! ```ignore
//! let cfg = PhoenixConfig::from_env();
//! if !ensure_phoenix_healthy(&cfg.base_url).await { return; }
//! let provider = setup_otel_provider(&cfg.otlp_traces_endpoint, "awaken-e2e")
//!     .expect("init OTLP provider");
//! let tracer = tracer_for(&provider, "awaken-e2e");
//! let sink = awaken_ext_observability::OtelMetricsSink::new(tracer);
//! // … record events, then …
//! provider.force_flush().ok();
//! let span = wait_for_span_with_model(&cfg.project_spans_url, &model_id).await
//!     .expect("phoenix returned the span we just exported");
//! ```
//!
//! All helpers are async-friendly but use plain `reqwest::Client` polling so
//! they work in both `tokio::test` and synchronous contexts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use serde_json::Value;

/// Default Phoenix UI / REST port.
pub const DEFAULT_PHOENIX_BASE_URL: &str = "http://127.0.0.1:6006";
/// Default Phoenix OTLP/HTTP traces endpoint (matches `arizephoenix/phoenix` image).
pub const DEFAULT_PHOENIX_OTLP_TRACES_ENDPOINT: &str = "http://127.0.0.1:6006/v1/traces";

/// Resolved Phoenix endpoints, sourced from the environment.
///
/// Recognised variables:
///
/// | Variable                              | Purpose                                | Default |
/// |---------------------------------------|----------------------------------------|---------|
/// | `PHOENIX_BASE_URL`                    | UI / REST root for span queries        | `http://127.0.0.1:6006` |
/// | `PHOENIX_OTLP_TRACES_ENDPOINT`        | OTLP/HTTP `/v1/traces` ingestion URL   | `{base}/v1/traces` |
/// | `OTEL_EXPORTER_OTLP_ENDPOINT`         | Falls back to `<endpoint>/v1/traces`   | — |
/// | `PHOENIX_COLLECTOR_ENDPOINT` (legacy) | Same fallback as above                 | — |
///
/// The legacy variables are kept solely so tests written against earlier
/// awaken versions keep working unchanged.
#[derive(Debug, Clone)]
pub struct PhoenixConfig {
    /// Phoenix UI base, e.g. `http://127.0.0.1:6006`.
    pub base_url: String,
    /// OTLP/HTTP `/v1/traces` ingestion endpoint.
    pub otlp_traces_endpoint: String,
    /// REST endpoint that returns spans for the default project, used by
    /// `wait_for_*` helpers.
    pub project_spans_url: String,
}

impl PhoenixConfig {
    /// Parse the configuration from process environment variables, applying
    /// the defaults documented on [`PhoenixConfig`].
    pub fn from_env() -> Self {
        let base_url = std::env::var("PHOENIX_BASE_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_PHOENIX_BASE_URL.to_string());

        let otlp_traces_endpoint = std::env::var("PHOENIX_OTLP_TRACES_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| {
                        if v.ends_with("/v1/traces") {
                            v
                        } else {
                            format!("{}/v1/traces", v.trim_end_matches('/'))
                        }
                    })
            })
            .or_else(|| {
                std::env::var("PHOENIX_COLLECTOR_ENDPOINT")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| {
                        if v.ends_with("/v1/traces") {
                            v
                        } else {
                            format!("{}/v1/traces", v.trim_end_matches('/'))
                        }
                    })
            })
            .unwrap_or_else(|| format!("{}/v1/traces", base_url.trim_end_matches('/')));

        let project_spans_url = format!("{}/v1/spans", base_url.trim_end_matches('/'));

        Self {
            base_url,
            otlp_traces_endpoint,
            project_spans_url,
        }
    }

    /// Whether the configuration looks usable. Currently always true — the
    /// defaults work out of the box for `docker run arizephoenix/phoenix`.
    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.otlp_traces_endpoint.is_empty()
    }
}

/// Initialise an OTLP/HTTP `SdkTracerProvider` pointing at Phoenix.
///
/// The returned provider uses a batch span exporter; callers are expected to
/// `force_flush()` and/or `shutdown()` before assertions to ensure spans
/// reach Phoenix.
///
/// # Errors
///
/// Returns the underlying OTLP exporter build error (typically a malformed
/// endpoint URL).
pub fn setup_otel_provider(
    otlp_traces_endpoint: &str,
    service_name: &str,
) -> Result<SdkTracerProvider, Box<dyn std::error::Error + Send + Sync>> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(otlp_traces_endpoint)
        .build()?;

    let resource = Resource::builder()
        .with_attributes(vec![KeyValue::new(
            "service.name",
            service_name.to_string(),
        )])
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    Ok(provider)
}

/// Construct an `SdkTracer` from a previously-built provider.
pub fn tracer_for(provider: &SdkTracerProvider, name: &'static str) -> SdkTracer {
    provider.tracer(name)
}

/// Returns `true` when Phoenix's `/v1/projects` endpoint responds with a 2xx
/// status. Tests should treat `false` as a signal to skip rather than fail.
pub async fn ensure_phoenix_healthy(base_url: &str) -> bool {
    let url = format!("{}/v1/projects", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };

    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Suffix derived from the wall clock, useful for generating per-test-run
/// model identifiers so polling helpers can isolate just-emitted spans.
///
/// Falls back to `0` when the system clock is somehow before `UNIX_EPOCH`.
pub fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// Extract a string attribute from a Phoenix span JSON payload.
///
/// Phoenix wraps OTel attributes under `attributes.{key}` (flat dotted keys),
/// `attributes.<key>` (nested), and sometimes inlines `name` / `status_code`
/// at the top level. This helper checks all three.
pub fn attr_str<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    if let Some(value) = span.get(key).and_then(Value::as_str) {
        return Some(value);
    }
    if let Some(attrs) = span.get("attributes")
        && let Some(value) = attrs.get(key).and_then(Value::as_str)
    {
        return Some(value);
    }
    None
}

/// Default polling: 50 attempts × 200 ms = up to 10 s.
const DEFAULT_POLL_ATTEMPTS: u32 = 50;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Poll Phoenix's spans REST endpoint until `predicate` returns `Some(span)`
/// or attempts run out.
///
/// `spans_url` is typically `{base}/v1/spans`. The body is expected to be a
/// JSON array (or `{"data": [...]}`); both shapes are unwrapped.
pub async fn wait_for_span<F>(spans_url: &str, predicate: F) -> Option<Value>
where
    F: Fn(&Value) -> bool,
{
    wait_for_span_with(
        spans_url,
        predicate,
        DEFAULT_POLL_ATTEMPTS,
        DEFAULT_POLL_INTERVAL,
    )
    .await
}

/// Same as [`wait_for_span`] but with custom polling cadence.
pub async fn wait_for_span_with<F>(
    spans_url: &str,
    predicate: F,
    attempts: u32,
    interval: Duration,
) -> Option<Value>
where
    F: Fn(&Value) -> bool,
{
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    for _ in 0..attempts {
        if let Ok(resp) = client.get(spans_url).send().await
            && resp.status().is_success()
            && let Ok(payload) = resp.json::<Value>().await
        {
            let spans = match &payload {
                Value::Array(_) => payload.clone(),
                Value::Object(map) => map
                    .get("data")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
                _ => Value::Array(Vec::new()),
            };
            if let Value::Array(items) = spans {
                for span in items {
                    if predicate(&span) {
                        return Some(span);
                    }
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
    None
}

/// Convenience wrapper that filters by `gen_ai.request.model`.
pub async fn wait_for_span_with_model(spans_url: &str, model: &str) -> Option<Value> {
    wait_for_span(spans_url, |span| {
        attr_str(span, "gen_ai.request.model") == Some(model)
    })
    .await
}

/// Convenience wrapper that filters chat-style inference spans by model.
pub async fn wait_for_chat_span(spans_url: &str, model: &str) -> Option<Value> {
    wait_for_span(spans_url, |span| {
        let model_match = attr_str(span, "gen_ai.request.model") == Some(model);
        let op_match = attr_str(span, "gen_ai.operation.name") == Some("chat");
        let name_match = span
            .get("name")
            .and_then(Value::as_str)
            .map(|name| name.starts_with("chat"))
            .unwrap_or(false);
        model_match && (op_match || name_match)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phoenix_config_default_endpoints() {
        // Note: cannot mutate env in tests because `unsafe_code = "forbid"`.
        // We assert structural shape only.
        let cfg = PhoenixConfig::from_env();
        assert!(cfg.is_configured());
        assert!(cfg.project_spans_url.ends_with("/v1/spans"));
        assert!(cfg.otlp_traces_endpoint.contains("/v1/traces"));
    }

    #[test]
    fn attr_str_reads_top_level_and_nested() {
        let span = serde_json::json!({
            "name": "chat gpt-4-x",
            "attributes": {
                "gen_ai.request.model": "gpt-4-x",
                "gen_ai.operation.name": "chat",
            }
        });
        assert_eq!(attr_str(&span, "name"), Some("chat gpt-4-x"));
        assert_eq!(attr_str(&span, "gen_ai.request.model"), Some("gpt-4-x"));
        assert_eq!(attr_str(&span, "gen_ai.operation.name"), Some("chat"));
        assert_eq!(attr_str(&span, "missing"), None);
    }

    #[test]
    fn unique_suffix_is_monotonic_ish() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert!(b >= a);
    }
}
