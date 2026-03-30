use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Instant;

use awaken_contract::contract::inference::TokenUsage;
use tokio::sync::Mutex;

use crate::metrics::AgentMetrics;
use crate::metrics::SpanContext;
use crate::sink::MetricsSink;

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
    pub(crate) inference_start: Mutex<Option<Instant>>,
    pub(crate) tool_start: Mutex<HashMap<String, Instant>>,
    pub(crate) model: Mutex<String>,
    pub(crate) provider: Mutex<String>,
    pub(crate) operation: String,
    pub(crate) temperature: Mutex<Option<f64>>,
    pub(crate) top_p: Mutex<Option<f64>>,
    pub(crate) max_tokens: Mutex<Option<u32>>,
    pub(crate) stop_sequences: Mutex<Vec<String>>,
    pub(crate) inference_tracing_span: Mutex<Option<tracing::Span>>,
    pub(crate) tool_tracing_span: Mutex<HashMap<String, tracing::Span>>,
    /// Execution context captured from RunIdentity at RunStart.
    pub(crate) span_context: Mutex<SpanContext>,
    /// Step counter incremented per inference (0-based).
    pub(crate) step_counter: AtomicU32,
}
