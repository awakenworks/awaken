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

use crate::metrics::{AgentMetrics, GenAISpan, MetricsEvent, ToolSpan};
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
            MetricsEvent::EvaluationResult(e) => e.context.agent_id.clone(),
            MetricsEvent::BackgroundTask(s) => s.context.agent_id.clone(),
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
            // Evaluations and background tasks are not aggregated into the
            // per-agent runtime windows yet.
            MetricsEvent::EvaluationResult(_) | MetricsEvent::BackgroundTask(_) => {}
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

#[cfg(test)]
#[path = "runtime_stats_test.rs"]
mod tests;
