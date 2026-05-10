use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Instant;

use awaken_contract::contract::inference::TokenUsage;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::metrics::AgentMetrics;
use crate::metrics::SpanContext;
use crate::metrics::TOOL_PAYLOAD_TRUNCATED_MARKER;
use crate::metrics::ToolIoCapture;
use crate::sink::MetricsSink;

pub(crate) const DEFAULT_TOOL_IO_MAX_PAYLOAD_BYTES: usize = 8 * 1024;
pub(crate) const REDACTED_TOOL_FIELD_VALUE: &str = "***";

pub(crate) type ToolIoRedactor = dyn Fn(Value) -> Value + Send + Sync;

pub(crate) fn extract_token_counts(
    usage: Option<&TokenUsage>,
) -> (Option<i32>, Option<i32>, Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (
            u.prompt_tokens,
            u.completion_tokens,
            u.total_tokens,
            u.thinking_tokens,
        ),
        None => (None, None, None, None),
    }
}

pub(crate) fn extract_cache_tokens(usage: Option<&TokenUsage>) -> (Option<i32>, Option<i32>) {
    match usage {
        Some(u) => (u.cache_read_tokens, u.cache_creation_tokens),
        None => (None, None),
    }
}

pub(crate) fn default_tool_io_redactor(value: Value) -> Value {
    redact_sensitive_fields(value)
}

pub(crate) fn identity_tool_io_redactor(value: Value) -> Value {
    value
}

fn redact_sensitive_fields(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(&key) {
                        Value::String(REDACTED_TOOL_FIELD_VALUE.to_string())
                    } else {
                        redact_sensitive_fields(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(redact_sensitive_fields).collect())
        }
        value => value,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    // Substring match against a normalized copy of the key (lowercased,
    // hyphens collapsed to underscores) so `Cookie`, `set-cookie`,
    // `x-api-key`, and `refresh_token` all hit. The list intentionally
    // stays narrow — we want to catch the obvious cases without redacting
    // business fields by accident. Callers needing a tighter contract can
    // layer `with_tool_io_allowed_fields` on top.
    let lower = key.to_ascii_lowercase().replace('-', "_");
    [
        "secret",
        "token", // catches refresh_token, id_token, access_token
        "password",
        "api_key",
        "apikey",
        "authorization",
        "auth", // bare `auth: ...` headers / payloads
        "credential",
        "bearer",
        "private_key",
        "access_key",
        "cookie",
        "session",
        "jwt",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn apply_field_allowlist(value: Value, allowlist: &HashSet<String>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(key, value)| {
                    allowlist
                        .contains(&key)
                        .then(|| (key, apply_field_allowlist(value, allowlist)))
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| apply_field_allowlist(value, allowlist))
                .collect(),
        ),
        value => value,
    }
}

fn enforce_payload_limit(value: Value, max_payload_bytes: usize) -> Value {
    let Ok(serialized) = serde_json::to_vec(&value) else {
        return json!({
            TOOL_PAYLOAD_TRUNCATED_MARKER: true,
            "reason": "serialization_failed",
            "max_payload_bytes": max_payload_bytes,
        });
    };
    if serialized.len() <= max_payload_bytes {
        return value;
    }
    json!({
        TOOL_PAYLOAD_TRUNCATED_MARKER: true,
        "original_size_bytes": serialized.len(),
        "max_payload_bytes": max_payload_bytes,
    })
}

/// Test-only compatibility wrapper: acquires a `tokio::sync::Mutex` lock
/// using `try_lock()` so that existing synchronous test assertions
/// continue to compile unchanged. Safe because tests hold locks only briefly
/// with no contention.
#[cfg(test)]
pub(crate) fn lock_unpoison<T>(m: &Mutex<T>) -> tokio::sync::MutexGuard<'_, T> {
    m.try_lock().expect("no contention in test")
}

/// Shared mutable state between the plugin and its phase hooks.
pub(crate) struct Inner {
    pub(crate) sink: Arc<dyn MetricsSink>,
    pub(crate) run_start: Mutex<Option<Instant>>,
    pub(crate) metrics: Mutex<AgentMetrics>,
    /// Inference start: monotonic instant for duration + epoch ms for wall
    /// clock anchor. The pair is set together so the absolute end time is
    /// `started_at_ms + duration_ms` and immune to clock drift.
    pub(crate) inference_start: Mutex<Option<(Instant, u64)>>,
    /// Tool start: see [`Self::inference_start`] for the pair semantics.
    pub(crate) tool_start: Mutex<HashMap<String, (Instant, u64)>>,
    pub(crate) model: Mutex<String>,
    pub(crate) provider: Mutex<String>,
    pub(crate) operation: String,
    pub(crate) temperature: Mutex<Option<f64>>,
    pub(crate) top_p: Mutex<Option<f64>>,
    pub(crate) max_tokens: Mutex<Option<u32>>,
    pub(crate) stop_sequences: Mutex<Vec<String>>,
    pub(crate) tool_io_capture: ToolIoCapture,
    pub(crate) tool_io_max_payload_bytes: usize,
    pub(crate) tool_io_allowed_fields: Option<Arc<HashSet<String>>>,
    pub(crate) tool_io_redactor: Arc<ToolIoRedactor>,
    pub(crate) inference_tracing_span: Mutex<Option<tracing::Span>>,
    pub(crate) tool_tracing_span: Mutex<HashMap<String, tracing::Span>>,
    /// Execution context captured from RunIdentity at RunStart.
    pub(crate) span_context: Mutex<SpanContext>,
    /// Last exported background task status keyed by (owner_thread_id, task_id).
    /// Pairing the thread id avoids false dedup hits when independent task
    /// managers reuse the same `bg_N` counter.
    pub(crate) background_task_statuses:
        Mutex<HashMap<(String, String), awaken_runtime::extensions::background::TaskStatus>>,
    /// Step counter incremented per inference (0-based).
    pub(crate) step_counter: AtomicU32,
}

impl Inner {
    pub(crate) fn sanitize_tool_payload(&self, value: &Value) -> Value {
        let mut sanitized = value.clone();
        if let Some(allowlist) = &self.tool_io_allowed_fields {
            sanitized = apply_field_allowlist(sanitized, allowlist);
        }
        // Run the user redactor first, then enforce the default redactor as the
        // last step so a custom redactor cannot reintroduce sensitive fields.
        sanitized = (self.tool_io_redactor)(sanitized);
        sanitized = default_tool_io_redactor(sanitized);
        enforce_payload_limit(sanitized, self.tool_io_max_payload_bytes)
    }
}
