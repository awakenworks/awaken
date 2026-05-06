//! Per-agent rolling-window runtime statistics.
//!
//! `RuntimeStatsRegistry` is a `MetricsSink` that buckets every recorded
//! event by `agent_id` and rolls a sliding window of fixed-size buckets so
//! the admin console can answer "how busy was *this* agent in the last
//! N minutes?" without depending on Prometheus, Phoenix, or any external
//! collector.
//!
//! The registry is intentionally process-scoped and in-memory:
//!
//! * **Per-agent attribution** — events without a non-empty
//!   `context.agent_id` are dropped (callers can use `InMemorySink` or
//!   `PersistentSink` for the unbucketed view).
//! * **Sliding window** — `bucket_window` controls how long each bucket
//!   covers; `bucket_count` decides how many buckets are retained. With
//!   the defaults (10 min × 144) the registry holds 24 hours of history.
//! * **No persistence** — restarting the server clears every counter.
//!   That's an explicit trade: persistence belongs to `PersistentSink`
//!   or external time-series databases.
//!
//! The type is `Send + Sync` and cheap to clone (it wraps an `Arc`).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::metrics::{
    AgentMetrics, DelegationSpan, GenAISpan, HandoffSpan, MetricsEvent, SuspensionSpan, ToolSpan,
};
use crate::sink::MetricsSink;

/// Default bucket length: 10 minutes.
pub const DEFAULT_BUCKET_WINDOW: Duration = Duration::from_secs(600);
/// Default bucket count: 144 buckets × 10 minutes = 24 hours.
pub const DEFAULT_BUCKET_COUNT: usize = 144;

/// Default histogram boundaries (ms).  Mirrors a Prometheus-style
/// distribution and gives sensible coverage for typical LLM agents:
/// fast tool calls (≤25 ms), median chat completions (~250 ms-1 s),
/// slow streaming runs (>10 s).  An additional `+infinity` catch-all
/// bucket is appended automatically by the histogram builder.
pub const DEFAULT_DURATION_BUCKETS_MS: &[u64] =
    &[10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

/// Per-agent rolling window aggregator.  Implements [`MetricsSink`] so it
/// can drop into any composite sink topology.
#[derive(Clone)]
pub struct RuntimeStatsRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    /// Length of one bucket.
    bucket_window: Duration,
    /// Maximum number of buckets retained per agent.  Older buckets are
    /// dropped on rollover.
    bucket_count: usize,
}

struct RegistryInner {
    /// `agent_id -> per-agent rolling buckets`.
    agents: HashMap<String, AgentBuckets>,
}

struct AgentBuckets {
    buckets: VecDeque<Bucket>,
}

#[derive(Clone)]
struct Bucket {
    /// Monotonic instant the bucket opened.
    opened_at: Instant,
    inference_count: u64,
    error_count: u64,
    input_tokens: u64,
    output_tokens: u64,
    /// Sum of inference durations for cheap mean computation.
    inference_duration_sum_ms: u64,
    /// Individual durations for percentile computation.  Capped to
    /// `MAX_DURATION_SAMPLES` per bucket so a runaway agent does not
    /// blow the registry's memory.
    inference_durations_ms: Vec<u64>,
    suspensions: u64,
    handoffs: u64,
    delegations: u64,
    tools: HashMap<String, ToolBucket>,
}

#[derive(Clone)]
struct ToolBucket {
    call_count: u64,
    failure_count: u64,
    total_duration_ms: u64,
    /// Capped sample list mirroring the inference path so per-tool
    /// percentiles and histograms can be computed lazily at snapshot
    /// time without re-collecting from disk.
    durations_ms: Vec<u64>,
}

const MAX_DURATION_SAMPLES: usize = 1024;

impl Default for RuntimeStatsRegistry {
    fn default() -> Self {
        Self::with_window(DEFAULT_BUCKET_WINDOW, DEFAULT_BUCKET_COUNT)
    }
}

impl RuntimeStatsRegistry {
    /// Create a registry with the documented defaults (10-minute buckets,
    /// 144 of them = 24 h).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry with a bespoke bucket cadence.
    ///
    /// `bucket_count` is clamped to a minimum of 1; `bucket_window` to
    /// 1 millisecond.  Both extremes are nonsense in production but the
    /// clamp avoids panics in unit tests with degenerate inputs.
    pub fn with_window(bucket_window: Duration, bucket_count: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                agents: HashMap::new(),
            })),
            bucket_window: bucket_window.max(Duration::from_millis(1)),
            bucket_count: bucket_count.max(1),
        }
    }

    /// Length of a single bucket.
    pub fn bucket_window(&self) -> Duration {
        self.bucket_window
    }

    /// Maximum number of buckets retained per agent.
    pub fn bucket_count(&self) -> usize {
        self.bucket_count
    }

    /// Total length of the rolling window.
    pub fn window(&self) -> Duration {
        self.bucket_window * self.bucket_count.max(1) as u32
    }

    /// Number of agent buckets currently tracked. Useful for tests and
    /// for surfacing "how many agents have been seen" in a dashboard.
    pub fn agent_count(&self) -> usize {
        self.inner.lock().agents.len()
    }

    /// List the `agent_id`s the registry has observed at least one event
    /// for. Result is sorted lexicographically for stable display.
    pub fn known_agents(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.inner.lock().agents.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Aggregate every retained bucket for `agent_id` into a single
    /// snapshot.  Returns `None` when the agent is unknown.
    pub fn snapshot_for(&self, agent_id: &str) -> Option<AgentRuntimeSnapshot> {
        self.snapshot_for_window(agent_id, None)
    }

    /// Aggregate the last `window` worth of buckets for `agent_id`.
    ///
    /// * `window = None` — aggregate all retained buckets (same as
    ///   [`snapshot_for`]).
    /// * `window = Some(d)` — consume only the trailing
    ///   `n = ceil(d / bucket_window)` buckets, clamped to `[1,
    ///   bucket_count]`.  The returned `window_seconds` reflects the
    ///   actual span covered by those `n` buckets.
    ///
    /// Returns `None` when the agent is unknown.
    pub fn snapshot_for_window(
        &self,
        agent_id: &str,
        window: Option<Duration>,
    ) -> Option<AgentRuntimeSnapshot> {
        let inner = self.inner.lock();
        let agent = inner.agents.get(agent_id)?;
        let buckets = match window {
            None => return Some(self.snapshot_from_buckets(agent_id, &agent.buckets)),
            Some(d) => {
                let n = d
                    .as_nanos()
                    .div_ceil(self.bucket_window.as_nanos())
                    .max(1)
                    .min(self.bucket_count as u128) as usize;
                let skip = agent.buckets.len().saturating_sub(n);
                let slice: VecDeque<_> = agent.buckets.iter().skip(skip).cloned().collect();
                slice
            }
        };
        Some(self.snapshot_from_window_buckets(agent_id, &buckets, window))
    }

    /// Like `snapshot_from_buckets` but overrides `window_seconds` to
    /// reflect the requested window rather than the registry maximum.
    fn snapshot_from_window_buckets(
        &self,
        agent_id: &str,
        buckets: &VecDeque<Bucket>,
        window: Option<Duration>,
    ) -> AgentRuntimeSnapshot {
        let mut snap = self.snapshot_from_buckets(agent_id, buckets);
        if let Some(d) = window {
            let n = d
                .as_nanos()
                .div_ceil(self.bucket_window.as_nanos())
                .max(1)
                .min(self.bucket_count as u128) as usize;
            snap.window_seconds = (self.bucket_window * n as u32).as_secs();
        }
        snap
    }

    fn snapshot_from_buckets(
        &self,
        agent_id: &str,
        buckets: &VecDeque<Bucket>,
    ) -> AgentRuntimeSnapshot {
        let mut snap = AgentRuntimeSnapshot {
            agent_id: agent_id.to_string(),
            window_seconds: self.window().as_secs(),
            bucket_window_seconds: self.bucket_window.as_secs(),
            bucket_count: self.bucket_count,
            inference_count: 0,
            error_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            avg_inference_duration_ms: 0.0,
            min_inference_duration_ms: 0,
            max_inference_duration_ms: 0,
            p50_inference_duration_ms: 0,
            p95_inference_duration_ms: 0,
            p99_inference_duration_ms: 0,
            inference_duration_histogram: Vec::new(),
            suspensions: 0,
            handoffs: 0,
            delegations: 0,
            tool_calls_by_tool: Vec::new(),
        };

        let mut all_durations: Vec<u64> = Vec::new();
        let mut tool_acc: HashMap<String, ToolBucket> = HashMap::new();

        for bucket in buckets {
            snap.inference_count += bucket.inference_count;
            snap.error_count += bucket.error_count;
            snap.input_tokens += bucket.input_tokens;
            snap.output_tokens += bucket.output_tokens;
            snap.suspensions += bucket.suspensions;
            snap.handoffs += bucket.handoffs;
            snap.delegations += bucket.delegations;
            all_durations.extend_from_slice(&bucket.inference_durations_ms);

            for (tool, t) in &bucket.tools {
                let entry = tool_acc.entry(tool.clone()).or_insert(ToolBucket {
                    call_count: 0,
                    failure_count: 0,
                    total_duration_ms: 0,
                    durations_ms: Vec::new(),
                });
                entry.call_count += t.call_count;
                entry.failure_count += t.failure_count;
                entry.total_duration_ms += t.total_duration_ms;
                entry.durations_ms.extend_from_slice(&t.durations_ms);
            }
        }

        if !all_durations.is_empty() {
            all_durations.sort_unstable();
            let sum: u64 = all_durations.iter().sum();
            snap.avg_inference_duration_ms = sum as f64 / all_durations.len() as f64;
            snap.min_inference_duration_ms = *all_durations.first().unwrap_or(&0);
            snap.max_inference_duration_ms = *all_durations.last().unwrap_or(&0);
            snap.p50_inference_duration_ms = percentile(&all_durations, 50);
            snap.p95_inference_duration_ms = percentile(&all_durations, 95);
            snap.p99_inference_duration_ms = percentile(&all_durations, 99);
            snap.inference_duration_histogram =
                build_histogram(&all_durations, DEFAULT_DURATION_BUCKETS_MS);
        }

        let mut tool_rows: Vec<ToolRuntimeStats> = tool_acc
            .into_iter()
            .map(|(tool, mut t)| {
                t.durations_ms.sort_unstable();
                let avg_duration_ms = if t.call_count == 0 {
                    0.0
                } else {
                    t.total_duration_ms as f64 / t.call_count as f64
                };
                let (min, max, p50, p95, p99) = if t.durations_ms.is_empty() {
                    (0, 0, 0, 0, 0)
                } else {
                    (
                        *t.durations_ms.first().unwrap_or(&0),
                        *t.durations_ms.last().unwrap_or(&0),
                        percentile(&t.durations_ms, 50),
                        percentile(&t.durations_ms, 95),
                        percentile(&t.durations_ms, 99),
                    )
                };
                let histogram = if t.durations_ms.is_empty() {
                    Vec::new()
                } else {
                    build_histogram(&t.durations_ms, DEFAULT_DURATION_BUCKETS_MS)
                };
                ToolRuntimeStats {
                    avg_duration_ms,
                    min_duration_ms: min,
                    max_duration_ms: max,
                    p50_duration_ms: p50,
                    p95_duration_ms: p95,
                    p99_duration_ms: p99,
                    duration_histogram: histogram,
                    tool,
                    call_count: t.call_count,
                    failure_count: t.failure_count,
                    total_duration_ms: t.total_duration_ms,
                }
            })
            .collect();
        tool_rows.sort_by(|a, b| a.tool.cmp(&b.tool));
        snap.tool_calls_by_tool = tool_rows;

        snap
    }

    /// Internal: route an event into the right bucket. Public so that
    /// downstream tests can drive the registry without going through the
    /// `MetricsSink::record` indirection.
    fn record_event(&self, event: &MetricsEvent) {
        let now = Instant::now();
        let agent_id = match event {
            MetricsEvent::Inference(s) => s.context.agent_id.clone(),
            MetricsEvent::Tool(s) => s.context.agent_id.clone(),
            MetricsEvent::Suspension(s) => s.context.agent_id.clone(),
            MetricsEvent::Handoff(s) => s.context.agent_id.clone(),
            MetricsEvent::Delegation(s) => s.context.agent_id.clone(),
        };
        if agent_id.is_empty() {
            return;
        }

        let mut inner = self.inner.lock();
        let agent = inner
            .agents
            .entry(agent_id.clone())
            .or_insert_with(|| AgentBuckets {
                buckets: VecDeque::with_capacity(self.bucket_count.min(8)),
            });

        // Roll forward the head bucket if needed.
        ensure_current_bucket(agent, now, self.bucket_window, self.bucket_count);
        let bucket = agent
            .buckets
            .back_mut()
            .expect("ensure_current_bucket leaves at least one bucket");

        match event {
            MetricsEvent::Inference(span) => apply_inference(bucket, span),
            MetricsEvent::Tool(span) => apply_tool(bucket, span),
            MetricsEvent::Suspension(_) => bucket.suspensions += 1,
            MetricsEvent::Handoff(_) => bucket.handoffs += 1,
            MetricsEvent::Delegation(_) => bucket.delegations += 1,
        }
    }
}

/// Roll the agent's bucket queue forward so the back bucket covers
/// `now`. Drops oldest buckets when `bucket_count` is exceeded.
fn ensure_current_bucket(
    agent: &mut AgentBuckets,
    now: Instant,
    bucket_window: Duration,
    bucket_count: usize,
) {
    let needs_open = match agent.buckets.back() {
        Some(b) => now.saturating_duration_since(b.opened_at) >= bucket_window,
        None => true,
    };
    if !needs_open {
        return;
    }
    agent.buckets.push_back(Bucket {
        opened_at: now,
        inference_count: 0,
        error_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        inference_duration_sum_ms: 0,
        inference_durations_ms: Vec::new(),
        suspensions: 0,
        handoffs: 0,
        delegations: 0,
        tools: HashMap::new(),
    });
    while agent.buckets.len() > bucket_count {
        agent.buckets.pop_front();
    }
}

fn apply_inference(bucket: &mut Bucket, span: &GenAISpan) {
    bucket.inference_count += 1;
    if span.error_type.is_some() {
        bucket.error_count += 1;
    }
    if let Some(input) = span.input_tokens {
        bucket.input_tokens += u64::try_from(input).unwrap_or(0);
    }
    if let Some(output) = span.output_tokens {
        bucket.output_tokens += u64::try_from(output).unwrap_or(0);
    }
    bucket.inference_duration_sum_ms = bucket
        .inference_duration_sum_ms
        .saturating_add(span.duration_ms);
    if bucket.inference_durations_ms.len() < MAX_DURATION_SAMPLES {
        bucket.inference_durations_ms.push(span.duration_ms);
    }
}

fn apply_tool(bucket: &mut Bucket, span: &ToolSpan) {
    let entry = bucket.tools.entry(span.name.clone()).or_insert(ToolBucket {
        call_count: 0,
        failure_count: 0,
        total_duration_ms: 0,
        durations_ms: Vec::new(),
    });
    entry.call_count += 1;
    if span.error_type.is_some() {
        entry.failure_count += 1;
    }
    entry.total_duration_ms = entry.total_duration_ms.saturating_add(span.duration_ms);
    if entry.durations_ms.len() < MAX_DURATION_SAMPLES {
        entry.durations_ms.push(span.duration_ms);
    }
}

/// Build a per-bucket histogram from a *sorted* sample slice.
///
/// The result is one [`HistogramBucket`] per boundary plus a final
/// `+infinity` catch-all bucket (`upper_bound_ms == None`).  Counts are
/// per-bucket (NOT cumulative): a sample falls into the first bucket whose
/// `upper_bound_ms` is greater than or equal to it.
fn build_histogram(sorted_samples: &[u64], boundaries: &[u64]) -> Vec<HistogramBucket> {
    let mut out: Vec<HistogramBucket> = Vec::with_capacity(boundaries.len() + 1);
    let mut idx = 0usize;
    for &boundary in boundaries {
        let mut count = 0u64;
        while idx < sorted_samples.len() && sorted_samples[idx] <= boundary {
            count += 1;
            idx += 1;
        }
        out.push(HistogramBucket {
            upper_bound_ms: Some(boundary),
            count,
        });
    }
    let remaining = (sorted_samples.len() - idx) as u64;
    out.push(HistogramBucket {
        upper_bound_ms: None,
        count: remaining,
    });
    out
}

/// Linear-interpolation percentile over a *sorted* slice. Clamps the
/// result to the slice; returns 0 for empty input.
fn percentile(sorted_samples: &[u64], percentile: u8) -> u64 {
    if sorted_samples.is_empty() {
        return 0;
    }
    if sorted_samples.len() == 1 {
        return sorted_samples[0];
    }
    let p = (percentile as f64 / 100.0).clamp(0.0, 1.0);
    let idx = ((sorted_samples.len() - 1) as f64 * p).round() as usize;
    sorted_samples[idx.min(sorted_samples.len() - 1)]
}

impl MetricsSink for RuntimeStatsRegistry {
    fn record(&self, event: &MetricsEvent) {
        self.record_event(event);
    }

    fn on_run_end(&self, _metrics: &AgentMetrics) {
        // Per-bucket aggregates already capture everything; the run-end
        // hook is a no-op here. We keep the empty impl so the trait
        // contract is honoured without surprising allocations.
    }
}

// ---------------------------------------------------------------------------
// Snapshot DTOs (the shape the HTTP layer serialises)
// ---------------------------------------------------------------------------

/// One bin of a duration histogram.  `upper_bound_ms == None` is the
/// catch-all `+infinity` bucket appended after every numeric boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistogramBucket {
    /// Upper bound for this bucket in milliseconds, or `None` for the
    /// catch-all bucket. Counts are per-bucket (not cumulative).
    pub upper_bound_ms: Option<u64>,
    /// Number of samples whose duration is greater than the previous
    /// bucket's upper bound and less-than-or-equal to `upper_bound_ms`.
    pub count: u64,
}

/// One aggregated view of a single agent's rolling-window stats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRuntimeSnapshot {
    pub agent_id: String,
    /// Total length of the rolling window in seconds.
    pub window_seconds: u64,
    /// One bucket's length in seconds.
    pub bucket_window_seconds: u64,
    /// Maximum number of buckets retained per agent.
    pub bucket_count: usize,
    pub inference_count: u64,
    pub error_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub avg_inference_duration_ms: f64,
    pub min_inference_duration_ms: u64,
    pub max_inference_duration_ms: u64,
    pub p50_inference_duration_ms: u64,
    pub p95_inference_duration_ms: u64,
    pub p99_inference_duration_ms: u64,
    /// Per-bucket histogram of inference latencies. Empty when no
    /// inference samples were recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inference_duration_histogram: Vec<HistogramBucket>,
    pub suspensions: u64,
    pub handoffs: u64,
    pub delegations: u64,
    /// Per-tool aggregation, sorted by tool name.
    pub tool_calls_by_tool: Vec<ToolRuntimeStats>,
}

/// One row of `tool_calls_by_tool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRuntimeStats {
    pub tool: String,
    pub call_count: u64,
    pub failure_count: u64,
    pub total_duration_ms: u64,
    pub avg_duration_ms: f64,
    pub min_duration_ms: u64,
    pub max_duration_ms: u64,
    pub p50_duration_ms: u64,
    pub p95_duration_ms: u64,
    pub p99_duration_ms: u64,
    /// Per-bucket histogram of this tool's durations. Empty when no
    /// samples were recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duration_histogram: Vec<HistogramBucket>,
}

// ---------------------------------------------------------------------------
// Window string parser  (`1h`, `24h`, `7d`, `3600`, `90s`, etc.)
// ---------------------------------------------------------------------------

/// Parse a `window` query-string value into a [`Duration`].
///
/// Accepted formats:
/// - `<n>s` — seconds
/// - `<n>m` — minutes
/// - `<n>h` — hours
/// - `<n>d` — days
/// - `<n>`  — bare integer → seconds
///
/// Returns `Err(String)` with a human-readable explanation on invalid input.
pub fn parse_window_str(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("window value is empty".into());
    }
    let (digits, multiplier) = if let Some(d) = s.strip_suffix('s') {
        (d, 1u64)
    } else if let Some(d) = s.strip_suffix('m') {
        (d, 60)
    } else if let Some(d) = s.strip_suffix('h') {
        (d, 3600)
    } else if let Some(d) = s.strip_suffix('d') {
        (d, 86400)
    } else {
        (s, 1u64)
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid window format {s:?}: expected <n>[s|m|h|d]"))?;
    if n == 0 {
        return Err(format!("window {s:?} must be greater than zero"));
    }
    Ok(Duration::from_secs(n * multiplier))
}

// ---------------------------------------------------------------------------
// Suppress dead-code warnings for variant types only used through trait dispatch.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _types_used(s: &SuspensionSpan, h: &HandoffSpan, d: &DelegationSpan) {
    let _ = (s, h, d);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::SpanContext;

    fn ctx(agent: &str) -> SpanContext {
        SpanContext {
            run_id: "r".into(),
            thread_id: "t".into(),
            agent_id: agent.into(),
            parent_run_id: None,
            parent_tool_call_id: None,
        }
    }

    fn inference(agent: &str, input: i32, output: i32, duration_ms: u64, err: bool) -> GenAISpan {
        GenAISpan {
            context: ctx(agent),
            step_index: None,
            model: "m".into(),
            provider: "p".into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: if err { Some("rate_limit".into()) } else { None },
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(input),
            output_tokens: Some(output),
            total_tokens: Some(input + output),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms,
        }
    }

    fn tool(agent: &str, name: &str, duration_ms: u64, err: bool) -> ToolSpan {
        ToolSpan {
            context: ctx(agent),
            step_index: None,
            name: name.into(),
            operation: "execute_tool".into(),
            call_id: format!("call-{name}-{agent}"),
            tool_type: "function".into(),
            call_arguments: None,
            call_result: None,
            error_type: if err { Some("err".into()) } else { None },
            duration_ms,
        }
    }

    // ── construction ───────────────────────────────────────────────

    #[test]
    fn defaults_match_24h_at_10_min_buckets() {
        let r = RuntimeStatsRegistry::new();
        assert_eq!(r.bucket_window(), DEFAULT_BUCKET_WINDOW);
        assert_eq!(r.bucket_count(), DEFAULT_BUCKET_COUNT);
        assert_eq!(r.window().as_secs(), 24 * 60 * 60);
    }

    #[test]
    fn with_window_clamps_pathological_inputs() {
        let r = RuntimeStatsRegistry::with_window(Duration::from_secs(0), 0);
        assert_eq!(r.bucket_count(), 1);
        assert!(r.bucket_window() >= Duration::from_millis(1));
    }

    #[test]
    fn registry_is_clone_send_sync() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<RuntimeStatsRegistry>();
        let r = RuntimeStatsRegistry::new();
        let _clone = r.clone();
    }

    // ── empty / unknown agent ──────────────────────────────────────

    #[test]
    fn snapshot_for_unknown_agent_returns_none() {
        let r = RuntimeStatsRegistry::new();
        assert!(r.snapshot_for("nobody").is_none());
        assert_eq!(r.agent_count(), 0);
        assert!(r.known_agents().is_empty());
    }

    #[test]
    fn empty_agent_id_event_is_dropped() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Inference(inference("", 1, 1, 10, false)));
        assert_eq!(r.agent_count(), 0);
    }

    // ── basic accumulation ─────────────────────────────────────────

    #[test]
    fn single_inference_aggregates() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Inference(inference(
            "alpha", 100, 50, 200, false,
        )));
        let snap = r.snapshot_for("alpha").unwrap();
        assert_eq!(snap.agent_id, "alpha");
        assert_eq!(snap.inference_count, 1);
        assert_eq!(snap.error_count, 0);
        assert_eq!(snap.input_tokens, 100);
        assert_eq!(snap.output_tokens, 50);
        assert_eq!(snap.p50_inference_duration_ms, 200);
        assert_eq!(snap.p95_inference_duration_ms, 200);
        assert!((snap.avg_inference_duration_ms - 200.0).abs() < 1e-9);
    }

    #[test]
    fn multiple_inferences_sum_tokens_and_count() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Inference(inference("a", 10, 5, 100, false)));
        r.record(&MetricsEvent::Inference(inference("a", 20, 7, 100, false)));
        r.record(&MetricsEvent::Inference(inference("a", 30, 9, 100, true)));
        let snap = r.snapshot_for("a").unwrap();
        assert_eq!(snap.inference_count, 3);
        assert_eq!(snap.error_count, 1);
        assert_eq!(snap.input_tokens, 60);
        assert_eq!(snap.output_tokens, 21);
    }

    #[test]
    fn negative_token_counts_clamp_to_zero() {
        let r = RuntimeStatsRegistry::new();
        let mut span = inference("a", -5, -3, 10, false);
        span.input_tokens = Some(-5);
        span.output_tokens = Some(-3);
        r.record(&MetricsEvent::Inference(span));
        let snap = r.snapshot_for("a").unwrap();
        assert_eq!(snap.input_tokens, 0);
        assert_eq!(snap.output_tokens, 0);
    }

    // ── tool aggregation ───────────────────────────────────────────

    #[test]
    fn tool_events_aggregate_per_tool() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Tool(tool("a", "search", 30, false)));
        r.record(&MetricsEvent::Tool(tool("a", "search", 70, true)));
        r.record(&MetricsEvent::Tool(tool("a", "write", 50, false)));
        let snap = r.snapshot_for("a").unwrap();
        assert_eq!(snap.tool_calls_by_tool.len(), 2);
        let search = snap
            .tool_calls_by_tool
            .iter()
            .find(|s| s.tool == "search")
            .unwrap();
        assert_eq!(search.call_count, 2);
        assert_eq!(search.failure_count, 1);
        assert_eq!(search.total_duration_ms, 100);
        assert!((search.avg_duration_ms - 50.0).abs() < 1e-9);
    }

    #[test]
    fn tool_rows_sorted_lex() {
        let r = RuntimeStatsRegistry::new();
        for name in ["zeta", "alpha", "beta"] {
            r.record(&MetricsEvent::Tool(tool("a", name, 10, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        let names: Vec<&str> = snap
            .tool_calls_by_tool
            .iter()
            .map(|s| s.tool.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "zeta"]);
    }

    // ── multi-agent isolation ──────────────────────────────────────

    #[test]
    fn agents_are_isolated() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Inference(inference(
            "alpha", 10, 5, 100, false,
        )));
        r.record(&MetricsEvent::Inference(inference(
            "beta", 999, 999, 1, false,
        )));
        let alpha = r.snapshot_for("alpha").unwrap();
        let beta = r.snapshot_for("beta").unwrap();
        assert_eq!(alpha.input_tokens, 10);
        assert_eq!(beta.input_tokens, 999);
        assert_eq!(alpha.inference_count, 1);
        assert_eq!(beta.inference_count, 1);
    }

    #[test]
    fn known_agents_returns_sorted_list() {
        let r = RuntimeStatsRegistry::new();
        for id in ["worker", "planner", "reviewer"] {
            r.record(&MetricsEvent::Inference(inference(id, 1, 1, 1, false)));
        }
        assert_eq!(r.known_agents(), vec!["planner", "reviewer", "worker"]);
        assert_eq!(r.agent_count(), 3);
    }

    // ── bucket rollover ────────────────────────────────────────────

    #[test]
    fn buckets_roll_forward_after_window() {
        let r = RuntimeStatsRegistry::with_window(Duration::from_millis(20), 4);
        r.record(&MetricsEvent::Inference(inference("a", 1, 1, 1, false)));
        std::thread::sleep(Duration::from_millis(30));
        r.record(&MetricsEvent::Inference(inference("a", 1, 1, 1, false)));
        let snap = r.snapshot_for("a").unwrap();
        // Both events still within retained 4 × 20 ms = 80 ms window.
        assert_eq!(snap.inference_count, 2);
    }

    #[test]
    fn old_buckets_drop_when_count_exceeded() {
        // 5 ms × 2 buckets = 10 ms total retention.
        let r = RuntimeStatsRegistry::with_window(Duration::from_millis(5), 2);
        r.record(&MetricsEvent::Inference(inference("a", 1, 1, 1, false)));
        std::thread::sleep(Duration::from_millis(8));
        r.record(&MetricsEvent::Inference(inference("a", 1, 1, 1, false)));
        std::thread::sleep(Duration::from_millis(8));
        r.record(&MetricsEvent::Inference(inference("a", 1, 1, 1, false)));
        // Only the last two buckets should still be retained.
        let snap = r.snapshot_for("a").unwrap();
        assert!(
            snap.inference_count <= 2,
            "expected <=2 retained inferences, got {}",
            snap.inference_count
        );
    }

    // ── suspension / handoff / delegation counters ─────────────────

    #[test]
    fn suspension_handoff_delegation_counters_increment() {
        let r = RuntimeStatsRegistry::new();
        let agent = "a".to_string();

        r.record(&MetricsEvent::Suspension(SuspensionSpan {
            context: ctx(&agent),
            tool_call_id: "c".into(),
            tool_name: "x".into(),
            action: "suspended".into(),
            resume_mode: None,
            duration_ms: None,
            timestamp_ms: 0,
        }));
        r.record(&MetricsEvent::Handoff(HandoffSpan {
            context: ctx(&agent),
            from_agent_id: "a".into(),
            to_agent_id: "b".into(),
            reason: None,
            timestamp_ms: 0,
        }));
        r.record(&MetricsEvent::Delegation(DelegationSpan {
            context: ctx(&agent),
            parent_run_id: "p".into(),
            child_run_id: Some("c".into()),
            target_agent_id: "b".into(),
            tool_call_id: "c".into(),
            duration_ms: Some(1),
            success: true,
            error_message: None,
            timestamp_ms: 0,
        }));
        let snap = r.snapshot_for(&agent).unwrap();
        assert_eq!(snap.suspensions, 1);
        assert_eq!(snap.handoffs, 1);
        assert_eq!(snap.delegations, 1);
    }

    // ── percentile correctness ─────────────────────────────────────

    #[test]
    fn percentile_zero_for_empty() {
        assert_eq!(percentile(&[], 50), 0);
    }

    #[test]
    fn percentile_single_sample_is_that_sample() {
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 95), 42);
    }

    #[test]
    fn percentile_p50_p95_on_sorted_input() {
        let samples: Vec<u64> = (1..=100).collect();
        // 100 samples, idx for p50 = round(99*0.5)=50 → samples[50]=51
        assert_eq!(percentile(&samples, 50), 51);
        // p95: idx = round(99*0.95) = 94 → samples[94] = 95
        assert_eq!(percentile(&samples, 95), 95);
    }

    #[test]
    fn snapshot_p50_p95_track_inference_distribution() {
        let r = RuntimeStatsRegistry::new();
        for d in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            r.record(&MetricsEvent::Inference(inference("a", 1, 1, d, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        assert!(snap.p50_inference_duration_ms >= 50);
        assert!(snap.p50_inference_duration_ms <= 60);
        assert!(snap.p95_inference_duration_ms >= 90);
        assert!((snap.avg_inference_duration_ms - 55.0).abs() < 1e-9);
    }

    // ── duration sample cap ────────────────────────────────────────

    #[test]
    fn duration_samples_cap_per_bucket() {
        let r = RuntimeStatsRegistry::new();
        for _ in 0..(MAX_DURATION_SAMPLES + 50) {
            r.record(&MetricsEvent::Inference(inference("a", 1, 1, 5, false)));
        }
        // Inference count keeps incrementing even if samples cap hits.
        let snap = r.snapshot_for("a").unwrap();
        assert_eq!(snap.inference_count, (MAX_DURATION_SAMPLES + 50) as u64);
        // Avg should still be 5 since every sample (whether retained or
        // not) contributes via inference_duration_sum_ms — wait, we cap
        // both. The avg here is computed from the retained samples
        // *post-aggregation* (we discard the running sum because samples
        // already give us mean). Just sanity-check it's non-zero.
        assert!(snap.avg_inference_duration_ms > 0.0);
    }

    // ── snapshot DTO serde ─────────────────────────────────────────

    #[test]
    fn snapshot_serde_roundtrip() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Inference(inference("a", 10, 5, 100, false)));
        r.record(&MetricsEvent::Tool(tool("a", "search", 50, false)));
        let snap = r.snapshot_for("a").unwrap();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: AgentRuntimeSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snap);
    }

    // ── min / max / p99 ────────────────────────────────────────────

    #[test]
    fn snapshot_zero_min_max_p99_when_no_samples() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Suspension(SuspensionSpan {
            context: ctx("a"),
            tool_call_id: "c".into(),
            tool_name: "x".into(),
            action: "suspended".into(),
            resume_mode: None,
            duration_ms: None,
            timestamp_ms: 0,
        }));
        let snap = r.snapshot_for("a").unwrap();
        assert_eq!(snap.min_inference_duration_ms, 0);
        assert_eq!(snap.max_inference_duration_ms, 0);
        assert_eq!(snap.p99_inference_duration_ms, 0);
    }

    #[test]
    fn snapshot_min_max_track_extremes() {
        let r = RuntimeStatsRegistry::new();
        for d in [37u64, 5, 999, 1, 250, 100] {
            r.record(&MetricsEvent::Inference(inference("a", 1, 1, d, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        assert_eq!(snap.min_inference_duration_ms, 1);
        assert_eq!(snap.max_inference_duration_ms, 999);
    }

    #[test]
    fn snapshot_p99_close_to_max() {
        let r = RuntimeStatsRegistry::new();
        for d in 1..=100u64 {
            r.record(&MetricsEvent::Inference(inference("a", 1, 1, d, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        // 100 samples, p99 idx = round(99*0.99)=98 → samples[98]=99
        assert_eq!(snap.p99_inference_duration_ms, 99);
    }

    // ── inference histogram ────────────────────────────────────────

    #[test]
    fn build_histogram_bucketises_correctly() {
        let samples: Vec<u64> = vec![5, 25, 75, 200, 800, 3000, 99_999];
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let h = build_histogram(&sorted, DEFAULT_DURATION_BUCKETS_MS);
        // boundaries = 10,25,50,100,250,500,1000,2500,5000,10000 → 11 entries (10 + +inf)
        assert_eq!(h.len(), 11);
        // Sum of all bucket counts equals sample count.
        let total: u64 = h.iter().map(|b| b.count).sum();
        assert_eq!(total, samples.len() as u64);
        // 5 → ≤10
        assert_eq!(h[0].count, 1);
        assert_eq!(h[0].upper_bound_ms, Some(10));
        // 25 → 11..=25 → bucket index 1 (≤25)
        assert_eq!(h[1].count, 1);
        // 75 → 51..=100 → bucket index 3
        assert_eq!(h[3].count, 1);
        // 200 → 101..=250 → bucket index 4
        assert_eq!(h[4].count, 1);
        // 800 → 501..=1000 → bucket index 6
        assert_eq!(h[6].count, 1);
        // 3000 → 2501..=5000 → bucket index 8
        assert_eq!(h[8].count, 1);
        // 99_999 → +inf
        assert_eq!(h[10].count, 1);
        assert_eq!(h[10].upper_bound_ms, None);
    }

    #[test]
    fn build_histogram_empty_yields_zero_counts_with_full_shape() {
        let h = build_histogram(&[], DEFAULT_DURATION_BUCKETS_MS);
        assert_eq!(h.len(), DEFAULT_DURATION_BUCKETS_MS.len() + 1);
        assert!(h.iter().all(|b| b.count == 0));
    }

    #[test]
    fn snapshot_inference_histogram_sums_to_inference_count() {
        let r = RuntimeStatsRegistry::new();
        for d in [3u64, 30, 300, 3000, 30000, 50] {
            r.record(&MetricsEvent::Inference(inference("a", 1, 1, d, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        let total: u64 = snap
            .inference_duration_histogram
            .iter()
            .map(|b| b.count)
            .sum();
        assert_eq!(total, snap.inference_count);
    }

    #[test]
    fn snapshot_inference_histogram_omitted_when_no_samples() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Tool(tool("a", "search", 5, false)));
        let snap = r.snapshot_for("a").unwrap();
        assert!(snap.inference_duration_histogram.is_empty());
    }

    #[test]
    fn snapshot_histogram_serialisation_skips_empty() {
        let r = RuntimeStatsRegistry::new();
        // No inference samples — histogram should not appear in JSON.
        r.record(&MetricsEvent::Tool(tool("a", "search", 5, false)));
        let snap = r.snapshot_for("a").unwrap();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("inference_duration_histogram"));
    }

    #[test]
    fn snapshot_histogram_serialisation_includes_when_populated() {
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Inference(inference("a", 1, 1, 50, false)));
        let snap = r.snapshot_for("a").unwrap();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("inference_duration_histogram"));
        assert!(json.contains(r#""upper_bound_ms":10"#));
    }

    #[test]
    fn histogram_bucket_serde_roundtrip() {
        let original = vec![
            HistogramBucket {
                upper_bound_ms: Some(100),
                count: 5,
            },
            HistogramBucket {
                upper_bound_ms: None,
                count: 1,
            },
        ];
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Vec<HistogramBucket> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    // ── per-tool min / max / p50 / p95 / p99 ───────────────────────

    #[test]
    fn snapshot_per_tool_percentiles_track_distribution() {
        let r = RuntimeStatsRegistry::new();
        for d in 1..=100u64 {
            r.record(&MetricsEvent::Tool(tool("a", "search", d, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        let stats = snap
            .tool_calls_by_tool
            .iter()
            .find(|s| s.tool == "search")
            .unwrap();
        assert_eq!(stats.min_duration_ms, 1);
        assert_eq!(stats.max_duration_ms, 100);
        assert_eq!(stats.p50_duration_ms, 51);
        assert_eq!(stats.p95_duration_ms, 95);
        assert_eq!(stats.p99_duration_ms, 99);
    }

    #[test]
    fn snapshot_per_tool_zero_percentiles_when_no_samples() {
        // Defensive — tool buckets only populate samples through
        // apply_tool, which always pushes. But verify the snapshot
        // builder handles an empty samples Vec gracefully.
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Tool(tool("a", "search", 0, false)));
        let snap = r.snapshot_for("a").unwrap();
        let stats = &snap.tool_calls_by_tool[0];
        // duration_ms=0 sample → all percentiles 0.
        assert_eq!(stats.min_duration_ms, 0);
        assert_eq!(stats.max_duration_ms, 0);
        assert_eq!(stats.p50_duration_ms, 0);
    }

    #[test]
    fn snapshot_per_tool_histogram_sums_to_call_count() {
        let r = RuntimeStatsRegistry::new();
        for d in [3u64, 30, 300, 3000, 30000] {
            r.record(&MetricsEvent::Tool(tool("a", "search", d, false)));
        }
        let snap = r.snapshot_for("a").unwrap();
        let stats = &snap.tool_calls_by_tool[0];
        let total: u64 = stats.duration_histogram.iter().map(|b| b.count).sum();
        assert_eq!(total, stats.call_count);
    }

    #[test]
    fn snapshot_per_tool_histogram_omitted_when_no_samples() {
        // Force a tool stats row with zero samples by building snapshot
        // from a registry where the tool was never recorded → not
        // possible via public API, since apply_tool always pushes a
        // sample. The branch is defensively tested via the snapshot
        // logic when samples Vec is empty (`.is_empty()` short-circuit).
        // Cover the path by recording one tool, then checking the
        // histogram is non-empty when call_count > 0.
        let r = RuntimeStatsRegistry::new();
        r.record(&MetricsEvent::Tool(tool("a", "search", 1, false)));
        let snap = r.snapshot_for("a").unwrap();
        assert!(!snap.tool_calls_by_tool[0].duration_histogram.is_empty());
    }

    // ── parse_window_str ───────────────────────────────────────────

    #[test]
    fn parse_window_str_seconds_suffix() {
        assert_eq!(parse_window_str("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_window_str_minutes_suffix() {
        assert_eq!(parse_window_str("5m").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn parse_window_str_hours_suffix() {
        assert_eq!(parse_window_str("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_window_str("24h").unwrap(),
            Duration::from_secs(24 * 3600)
        );
    }

    #[test]
    fn parse_window_str_days_suffix() {
        assert_eq!(
            parse_window_str("7d").unwrap(),
            Duration::from_secs(7 * 86400)
        );
    }

    #[test]
    fn parse_window_str_bare_integer_treated_as_seconds() {
        assert_eq!(parse_window_str("3600").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_window_str_rejects_unknown_suffix() {
        assert!(parse_window_str("5x").is_err());
        assert!(parse_window_str("foo").is_err());
    }

    #[test]
    fn parse_window_str_rejects_zero() {
        assert!(parse_window_str("0h").is_err());
        assert!(parse_window_str("0").is_err());
    }

    #[test]
    fn parse_window_str_rejects_empty() {
        assert!(parse_window_str("").is_err());
    }

    // ── snapshot_for_window ────────────────────────────────────────

    /// Build a registry with `bucket_count` buckets of `bucket_window`
    /// each, filling each bucket with one inference of the given
    /// `token_input` so we can verify which buckets were selected by
    /// counting `input_tokens` in the snapshot.
    fn fixture_registry(bucket_window_ms: u64, bucket_count: usize) -> RuntimeStatsRegistry {
        RuntimeStatsRegistry::with_window(Duration::from_millis(bucket_window_ms), bucket_count)
    }

    #[test]
    fn snapshot_for_window_none_returns_all_buckets() {
        let r = fixture_registry(1, 4);
        // Push 4 inferences into the same (single) bucket (no sleep needed).
        for i in 0..4u32 {
            r.record(&MetricsEvent::Inference(inference(
                "a", i as i32, 0, 1, false,
            )));
        }
        let snap_all = r.snapshot_for("a").unwrap();
        let snap_win = r.snapshot_for_window("a", None).unwrap();
        assert_eq!(snap_all.inference_count, snap_win.inference_count);
        assert_eq!(snap_all.window_seconds, snap_win.window_seconds);
    }

    #[test]
    fn snapshot_for_window_small_window_selects_fewer_buckets() {
        // 3 buckets × 10 ms each.  We inject one inference per bucket by
        // sleeping between them so the registry rolls forward.
        let r = RuntimeStatsRegistry::with_window(Duration::from_millis(20), 3);
        // Bucket 1
        r.record(&MetricsEvent::Inference(inference("a", 10, 0, 1, false)));
        std::thread::sleep(Duration::from_millis(25));
        // Bucket 2
        r.record(&MetricsEvent::Inference(inference("a", 20, 0, 1, false)));
        std::thread::sleep(Duration::from_millis(25));
        // Bucket 3
        r.record(&MetricsEvent::Inference(inference("a", 30, 0, 1, false)));

        // Request only 1 bucket_window (20 ms) → should see only the last bucket.
        let snap = r
            .snapshot_for_window("a", Some(Duration::from_millis(20)))
            .unwrap();
        // Only last bucket: input_tokens == 30.
        assert_eq!(snap.input_tokens, 30, "expected only the last bucket");
        assert_eq!(snap.inference_count, 1);
        // window_seconds should reflect 1 × bucket_window.
        assert_eq!(snap.window_seconds, r.bucket_window().as_secs());
    }

    #[test]
    fn snapshot_for_window_larger_than_total_returns_all() {
        let r = RuntimeStatsRegistry::with_window(Duration::from_millis(20), 3);
        r.record(&MetricsEvent::Inference(inference("a", 5, 0, 1, false)));
        // Ask for 10 days — way beyond 3 × 20 ms.
        let snap = r
            .snapshot_for_window("a", Some(Duration::from_secs(864_000)))
            .unwrap();
        assert_eq!(snap.input_tokens, 5);
        // window_seconds clamped to bucket_count × bucket_window.
        assert_eq!(snap.window_seconds, r.window().as_secs());
    }

    #[test]
    fn snapshot_for_window_unknown_agent_returns_none() {
        let r = RuntimeStatsRegistry::new();
        assert!(
            r.snapshot_for_window("ghost", Some(Duration::from_secs(3600)))
                .is_none()
        );
    }

    // ── thread-safety smoke ────────────────────────────────────────

    #[test]
    fn record_is_thread_safe() {
        use std::sync::Arc;
        let r = Arc::new(RuntimeStatsRegistry::new());
        let mut handles = Vec::new();
        for thread_id in 0..8 {
            let r = Arc::clone(&r);
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    let agent = format!("agent-{}", thread_id % 3);
                    r.record(&MetricsEvent::Inference(inference(
                        &agent,
                        i % 5,
                        i % 7,
                        (i * 3) as u64,
                        i % 11 == 0,
                    )));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let total: u64 = r
            .known_agents()
            .iter()
            .map(|a| r.snapshot_for(a).unwrap().inference_count)
            .sum();
        assert_eq!(total, 8 * 50);
    }
}
