use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

use awaken_runtime::extensions::background::TaskStatus;

use super::metrics::{AgentMetrics, BackgroundTaskSpan, EvaluationResultEvent, MetricsEvent};
// `BackgroundTaskSpan` and `EvaluationResultEvent` are still referenced by the
// legacy `PersistedLine` variants kept around so previously-spilled NDJSON
// files keep deserialising after the trait API was simplified.
use super::sink::{MetricsSink, SinkError};
use crate::sampling::{RunOutcome, SamplingPolicy, should_persist};
use crate::trace_store::TraceStore;

/// Maximum events buffered per run while a sampling policy is installed.
/// At ~1 KiB per `MetricsEvent` this caps a single misbehaving run that
/// never fires `on_run_end` to ~10 MiB before its buffer is dropped.
const MAX_BUFFERED_EVENTS_PER_RUN: usize = 10_000;

/// Configuration for [`PersistentSink`].
pub struct PersistenceConfig {
    /// Directory where failed event files are stored.
    pub storage_dir: PathBuf,
    /// Maximum number of retry attempts per file (default: 8).
    pub max_retry_attempts: u32,
    /// Base backoff delay between retries (default: 500ms).
    pub base_backoff: Duration,
    /// Maximum backoff delay (default: 30s).
    pub max_backoff: Duration,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            storage_dir: std::env::temp_dir().join("awaken-persistent-sink"),
            max_retry_attempts: 8,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Envelope for run-end events persisted to disk (session duration only).
///
/// `MetricsEvent` covers the five span types; this wrapper adds the run-end
/// case so that all persisted lines share a consistent tagged JSON format.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum PersistedLine {
    RunEnd {
        #[serde(rename = "type")]
        line_type: RunEndMarker,
        session_duration_ms: u64,
    },
    EvaluationResult {
        #[serde(rename = "type")]
        line_type: EvaluationResultMarker,
        #[serde(flatten)]
        event: Box<EvaluationResultEvent>,
    },
    BackgroundTask {
        #[serde(rename = "type")]
        line_type: BackgroundTaskMarker,
        #[serde(flatten)]
        span: Box<BackgroundTaskSpan>,
    },
    Event(Box<MetricsEvent>),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum RunEndMarker {
    #[serde(rename = "run_end")]
    RunEnd,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum EvaluationResultMarker {
    #[serde(rename = "evaluation_result")]
    EvaluationResult,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum BackgroundTaskMarker {
    #[serde(rename = "background_task")]
    BackgroundTask,
}

/// A [`MetricsSink`] wrapper that persists events to disk on flush failure.
///
/// All `record` calls are forwarded to the inner sink immediately and also
/// buffered locally. On a successful [`MetricsSink::flush`] the buffer is
/// cleared. On failure the buffer is written as NDJSON to `storage_dir`.
/// [`retry_persisted`](PersistentSink::retry_persisted) reads those files
/// back and replays them through the inner sink.
///
/// When constructed via [`PersistentSink::with_trace_store`], events are
/// buffered per `run_id` in memory.  On [`MetricsSink::on_run_end`] the
/// sampling policy is consulted: if the run should be persisted the buffer
/// is flushed to the [`TraceStore`]; otherwise the buffer is dropped.  This
/// is a best-effort write — failures are logged but never surface to callers
/// (ADR-0030).
///
/// The inner sink and disk-spill paths are unaffected by the sampling
/// decision: they fire immediately on every `record` / `on_run_end` call as
/// before.
/// State of a single run's TraceStore buffer under the sampling path.
///
/// Once a run produces more events than `MAX_BUFFERED_EVENTS_PER_RUN`,
/// its slot transitions to `Overflowed` and stays there until
/// `on_run_end` clears it. **No further events are appended** — without
/// this enum the previous `clear()`-then-keep-buffering implementation
/// would silently flush a tail-fragment that didn't include the head of
/// the run.
enum RunBuffer {
    Events(Vec<MetricsEvent>),
    Overflowed,
}

impl RunBuffer {
    fn new() -> Self {
        Self::Events(Vec::new())
    }
}

pub struct PersistentSink {
    inner: Arc<dyn MetricsSink>,
    trace_store: Option<Arc<dyn TraceStore>>,
    /// Optional sampling policy.  When `None`, all events are written to the
    /// trace store (same behaviour as before T10).
    sampling: Option<Arc<RwLock<SamplingPolicy>>>,
    /// Per-run event buffer for the trace store path.  Keyed by `run_id`.
    /// Events whose `run_id` is empty (test fixtures, boot-time spans) are
    /// never buffered — they are skipped as before.
    trace_buffer: Mutex<HashMap<String, RunBuffer>>,
    /// Count of runs that exceeded `MAX_BUFFERED_EVENTS_PER_RUN` and were
    /// therefore dropped at `on_run_end` regardless of their later error
    /// or judge-score outcome. Embedders that hold a typed reference to
    /// the sink read this via [`PersistentSink::overflow_count`] to
    /// detect "where did my error trace go?" scenarios. Each overflow
    /// also emits a `tracing::warn!` at the transition point, so log
    /// aggregation catches the signal even when the sink is wrapped
    /// behind an `Arc<dyn MetricsSink>`. Incremented exactly once per
    /// overflowing run — at the transition point — so the gauge
    /// reflects distinct runs, not lost events.
    overflow_count: AtomicU64,
    config: PersistenceConfig,
    pending: Arc<Mutex<Vec<PersistedLine>>>,
}

impl PersistentSink {
    /// Create a new `PersistentSink` wrapping `inner`.
    ///
    /// Creates `config.storage_dir` if it does not exist.
    pub fn new(inner: Arc<dyn MetricsSink>, config: PersistenceConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.storage_dir)?;
        Ok(Self {
            inner,
            trace_store: None,
            sampling: None,
            trace_buffer: Mutex::new(HashMap::new()),
            overflow_count: AtomicU64::new(0),
            config,
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Create a `PersistentSink` that routes events through both the inner sink
    /// and the supplied `TraceStore`.
    ///
    /// The legacy disk-spill behaviour is unchanged — the trace store is an
    /// additional write target, not a replacement.  Creates `config.storage_dir`
    /// if it does not exist.
    ///
    /// Without a sampling policy (see [`with_sampling_policy`]) every event is
    /// written to the trace store immediately on `record`.  With a policy,
    /// events are buffered per `run_id` and flushed (or dropped) on
    /// `on_run_end`.
    pub fn with_trace_store(
        inner: Arc<dyn MetricsSink>,
        trace_store: Arc<dyn TraceStore>,
        config: PersistenceConfig,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.storage_dir)?;
        Ok(Self {
            inner,
            trace_store: Some(trace_store),
            sampling: None,
            trace_buffer: Mutex::new(HashMap::new()),
            overflow_count: AtomicU64::new(0),
            config,
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Number of runs dropped because their per-run sampling buffer
    /// exceeded `MAX_BUFFERED_EVENTS_PER_RUN`. Counted at the
    /// transition into `RunBuffer::Overflowed`, so each overflowing
    /// run contributes exactly one — independent of how many events
    /// came after. A non-zero value means later error / low-judge
    /// outcomes on those runs were **not** honoured; they were
    /// dropped along with the rest of the trace.
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
    }

    /// Attach a sampling policy.  Once set, trace-store writes are deferred
    /// until `on_run_end` and gated by [`should_persist`].
    ///
    /// The policy is wrapped in an `Arc<RwLock<_>>` so callers can swap it at
    /// runtime (e.g., from a config-reload hook) without rebuilding the sink.
    pub fn with_sampling_policy(mut self, policy: Arc<RwLock<SamplingPolicy>>) -> Self {
        self.sampling = Some(policy);
        self
    }

    /// Write the given lines to an NDJSON file in `storage_dir`.
    fn persist_to_disk(&self, lines: &[PersistedLine]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let filename = format!("failed_events_{}.ndjson", uuid::Uuid::now_v7().hyphenated());
        let path = self.config.storage_dir.join(filename);
        let mut file = std::fs::File::create(&path)?;
        for line in lines {
            let json = serde_json::to_string(line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(file, "{json}")?;
        }
        file.flush()?;
        Ok(())
    }

    /// Replay a single [`PersistedLine`] through the inner sink.
    fn replay_line(&self, line: &PersistedLine) {
        match line {
            PersistedLine::Event(event) => self.inner.record(event.as_ref()),
            PersistedLine::EvaluationResult { event, .. } => {
                self.inner
                    .record(&MetricsEvent::EvaluationResult(event.as_ref().clone()));
            }
            PersistedLine::BackgroundTask { span, .. } => {
                self.inner
                    .record(&MetricsEvent::BackgroundTask(span.as_ref().clone()));
            }
            PersistedLine::RunEnd {
                session_duration_ms,
                ..
            } => {
                let metrics = AgentMetrics {
                    session_duration_ms: *session_duration_ms,
                    ..Default::default()
                };
                self.inner.on_run_end(&metrics);
            }
        }
    }

    /// Load persisted NDJSON files from `storage_dir`, replay events through
    /// the inner sink, and delete files that were fully replayed.
    ///
    /// Returns the total number of events replayed.
    pub fn retry_persisted(&self) -> std::io::Result<usize> {
        let mut total = 0usize;
        let entries: Vec<_> = std::fs::read_dir(&self.config.storage_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();

        for entry in entries {
            let path = entry.path();
            let file = std::fs::File::open(&path)?;
            let reader = std::io::BufReader::new(file);
            let mut lines = Vec::new();

            for raw_line in reader.lines() {
                let raw_line = raw_line?;
                if raw_line.trim().is_empty() {
                    continue;
                }
                let line: PersistedLine = serde_json::from_str(&raw_line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                lines.push(line);
            }

            // Replay all lines, then attempt flush.
            for line in &lines {
                self.replay_line(line);
            }

            match self.inner.flush() {
                Ok(()) => {
                    std::fs::remove_file(&path)?;
                    total += lines.len();
                }
                Err(_) => {
                    // Leave the file for a future retry attempt.
                }
            }
        }

        Ok(total)
    }

    /// Number of events buffered since the last successful flush.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().len()
    }
}

impl MetricsSink for PersistentSink {
    fn record(&self, event: &MetricsEvent) {
        self.inner.record(event);
        self.pending
            .lock()
            .push(PersistedLine::Event(Box::new(event.clone())));

        // ADR-0030: route events to the trace store (best-effort; failures are
        // logged but never panic). Trace data loss is real data loss, so
        // failures land at `error!` rather than `warn!` — operators
        // running at ERROR-level filtering still see them. Events whose
        // SpanContext was constructed without a run_id (test fixtures,
        // boot-time synthetic spans) are skipped because TraceStore needs
        // a non-empty key to shard by.
        //
        // With a sampling policy the event is buffered per run_id and the
        // TraceStore write is deferred until `on_run_end` — see the comment
        // there.  Without a policy we write through immediately (legacy path).
        if let Some(store) = &self.trace_store {
            let run_id = run_id_of(event);
            if !run_id.is_empty() {
                if self.sampling.is_some() {
                    // Deferred path: buffer for the per-run decision at run end.
                    // The buffer is bounded per-run: if the run misbehaves and
                    // never fires `on_run_end`, the cap stops a single run from
                    // exhausting memory. When a run hits the cap the slot
                    // transitions to `Overflowed` and stays there for the
                    // lifetime of the run — subsequent events drop silently so
                    // we never flush a partial tail-fragment as if it were a
                    // complete trace.
                    let mut buf = self.trace_buffer.lock();
                    let entry = buf.entry(run_id.clone()).or_insert_with(RunBuffer::new);
                    match entry {
                        RunBuffer::Events(events) => {
                            if events.len() >= MAX_BUFFERED_EVENTS_PER_RUN {
                                tracing::warn!(
                                    run_id,
                                    cap = MAX_BUFFERED_EVENTS_PER_RUN,
                                    "TraceStore buffer cap hit; dropping run from sampling buffer"
                                );
                                *entry = RunBuffer::Overflowed;
                                // Bump the lifetime counter exactly once
                                // per overflowing run so the gauge reflects
                                // distinct dropped runs, not dropped events.
                                self.overflow_count.fetch_add(1, Ordering::Relaxed);
                            } else {
                                events.push(event.clone());
                            }
                        }
                        RunBuffer::Overflowed => {
                            // Already overflowed — no-op. The inner sink and
                            // disk-spill path above still received this event.
                        }
                    }
                } else {
                    // Immediate path (no policy): write through as before.
                    if let Err(e) = store.append(&run_id, event) {
                        tracing::error!(error = %e, run_id, "TraceStore append failed");
                    }
                }
            }
        }
    }

    fn on_run_end(&self, metrics: &AgentMetrics) {
        self.inner.on_run_end(metrics);
        self.pending.lock().push(PersistedLine::RunEnd {
            line_type: RunEndMarker::RunEnd,
            session_duration_ms: metrics.session_duration_ms,
        });

        let Some(store) = self.trace_store.as_ref() else {
            return;
        };
        let Some(run_id) = run_id_from_metrics(metrics) else {
            // No run_id — nothing was buffered under a real key; skip both
            // the flush path and the index write.
            return;
        };

        // Sampling gate: when a policy is installed we either flush the
        // buffered events to the TraceStore or drop them. Without a policy
        // events were already appended immediately by `record` (legacy
        // write-through), so we only need to write the index here.
        let mut persisted = self.sampling.is_none();
        if let Some(sampling) = self.sampling.as_ref() {
            // Match the broader error definition that `derive_run_summary`
            // uses for `final_status`. Without this, a run that only
            // failed via a delegation / background-task / evaluation
            // error would land at `had_error = false` and miss the
            // `error_traces` policy — even though the index would later
            // record it as `final_status = "error"`.
            let had_error = run_had_error(metrics);
            // F14: derive `judge_score` from any `EvaluationResultEvent`
            // recorded for this run. We pick the **minimum** score so a
            // single low-scoring judge call promotes the run via the
            // `low_judge_score` policy even when other judges scored it
            // higher. `None` only when no judge fired.
            let judge_score = metrics
                .evaluations
                .iter()
                .filter_map(|e| e.score_value)
                .map(|v| v as f32)
                .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a| a.min(v))));
            // `explicit_flag` is set by callers that want to force-keep a
            // run (HITL reject, thumbs-down). It is not derivable from
            // span data alone — when a higher-level API surfaces the
            // signal it can fold it into the run lifecycle and the sink
            // will honour it via this field. Until then, hardcoded false
            // is correct (errors and judge already cover the
            // common-case promotion).
            let outcome = RunOutcome {
                had_error,
                explicit_flag: false,
                judge_score,
            };
            let decision = {
                let policy = sampling.read();
                should_persist(&policy, &run_id, &outcome)
            };
            if decision {
                let slot = self.trace_buffer.lock().remove(&run_id);
                match slot {
                    Some(RunBuffer::Events(events)) => {
                        for event in &events {
                            if let Err(e) = store.append(&run_id, event) {
                                tracing::error!(error = %e, run_id, "TraceStore append failed");
                            }
                        }
                        persisted = true;
                    }
                    Some(RunBuffer::Overflowed) => {
                        tracing::warn!(
                            run_id,
                            "run overflowed sampling buffer; trace dropped at run_end"
                        );
                    }
                    None => {}
                }
            } else {
                // Drop the buffer — run did not meet the sampling threshold.
                self.trace_buffer.lock().remove(&run_id);
            }
        }

        // ADR-0030 D7: emit the RunSummary index so `GET /v1/traces` can
        // list this run. The index is colocated with the events thanks to
        // FileTraceStore's pinned shard directory (T-fix F6); we only
        // skip the write when the events themselves were not persisted
        // (sampling-dropped or buffer-overflowed) so the index never
        // points at a non-existent shard.
        if persisted {
            let summary = derive_run_summary(&run_id, metrics);
            if let Err(e) = store.write_index_for_run(&run_id, &summary) {
                tracing::error!(error = %e, run_id, "TraceStore index write failed");
            }
        }
    }

    fn flush(&self) -> Result<(), SinkError> {
        match self.inner.flush() {
            Ok(()) => {
                self.pending.lock().clear();
                Ok(())
            }
            Err(e) => {
                let pending: Vec<_> = self.pending.lock().drain(..).collect();
                if !pending.is_empty() {
                    let _ = self.persist_to_disk(&pending);
                }
                Err(e)
            }
        }
    }

    fn shutdown(&self) -> Result<(), SinkError> {
        let flush_result = self.flush();
        let _ = self.inner.shutdown();
        flush_result
    }

    fn flush_run(&self, run_key: &str, close_reason: &'static str) -> Result<(), SinkError> {
        self.inner.flush_run(run_key, close_reason)
    }
}

fn run_id_of(event: &MetricsEvent) -> String {
    match event {
        MetricsEvent::Inference(s) => s.context.run_id.clone(),
        MetricsEvent::Tool(s) => s.context.run_id.clone(),
        MetricsEvent::Suspension(s) => s.context.run_id.clone(),
        MetricsEvent::Handoff(s) => s.context.run_id.clone(),
        MetricsEvent::Delegation(s) => s.context.run_id.clone(),
        MetricsEvent::EvaluationResult(e) => e.context.run_id.clone(),
        MetricsEvent::BackgroundTask(s) => s.context.run_id.clone(),
    }
}

/// Extract the run_id by inspecting `AgentMetrics` span collections in
/// priority order. Returns `None` for an empty `AgentMetrics` (no spans
/// → no run identity available).
///
/// F21: background tasks count too. A run that produces only background
/// task spans (e.g. a long-running scheduler that never reaches an
/// inference) still has identity worth indexing — without this branch
/// the run's events would land in `.ndjson` but `on_run_end` would skip
/// `write_index_for_run` and `/v1/traces` could not surface the run.
fn run_id_from_metrics(metrics: &AgentMetrics) -> Option<String> {
    metrics
        .inferences
        .first()
        .map(|s| &s.context.run_id)
        .or_else(|| metrics.tools.first().map(|s| &s.context.run_id))
        .or_else(|| metrics.evaluations.first().map(|s| &s.context.run_id))
        .or_else(|| metrics.suspensions.first().map(|s| &s.context.run_id))
        .or_else(|| metrics.handoffs.first().map(|s| &s.context.run_id))
        .or_else(|| metrics.delegations.first().map(|s| &s.context.run_id))
        .or_else(|| metrics.background_tasks.first().map(|s| &s.context.run_id))
        .filter(|id| !id.is_empty())
        .cloned()
}

/// Iterate every `SpanContext` recorded for the run, across all span
/// kinds. Used by `derive_run_summary` to aggregate `agent_id`,
/// `prompt_ids`, and the ADR-0031 experiment fields without privileging
/// inference spans — an evaluation-only / handoff-only / background-only
/// run still carries its attribution on the SpanContext, and listing /
/// filtering by `prompt_id` or `experiment_id` must surface those runs
/// too.
fn iter_span_contexts(
    metrics: &AgentMetrics,
) -> impl Iterator<Item = &crate::metrics::SpanContext> + '_ {
    metrics
        .inferences
        .iter()
        .map(|s| &s.context)
        .chain(metrics.tools.iter().map(|s| &s.context))
        .chain(metrics.evaluations.iter().map(|e| &e.context))
        .chain(metrics.suspensions.iter().map(|s| &s.context))
        .chain(metrics.handoffs.iter().map(|s| &s.context))
        .chain(metrics.delegations.iter().map(|s| &s.context))
        .chain(metrics.background_tasks.iter().map(|s| &s.context))
}

/// Run-level error flag: any inference, tool, background-task,
/// evaluation, or delegation error flips it to `true`. A background
/// task with `status == Failed` counts even if `error_message` is
/// unset, because the producer is not contractually required to fill
/// the message field. Suspensions, handoffs, and `Cancelled` background
/// tasks are status transitions rather than failure signals, so they
/// don't contribute. Shared between the sampling gate (so the
/// `error_traces` policy fires on the same definition that the index
/// uses) and `derive_run_summary` (so `final_status` mirrors it).
fn run_had_error(metrics: &AgentMetrics) -> bool {
    metrics.inferences.iter().any(|s| s.error_type.is_some())
        || metrics.tools.iter().any(|s| s.error_type.is_some())
        || metrics
            .background_tasks
            .iter()
            .any(|s| s.error_message.is_some() || s.status == TaskStatus::Failed)
        || metrics.evaluations.iter().any(|e| e.error_type.is_some())
        || metrics.delegations.iter().any(|d| !d.success)
}

/// Build a `RunSummary` for the index file written at `on_run_end`.
/// Pulls agent_id, started_at/ended_at, prompt_ids, experiment
/// attribution, `judge_score`, and `final_status` from the recorded
/// spans. The score uses the same min-aggregation as the sampling path
/// so listings, sampling rationale, and the policy threshold all agree
/// on a single number per run.
///
/// The time bracket and the `final_status` derivation cover every span
/// kind that can stand on its own (inference, tool, background task,
/// evaluation, suspension, handoff, delegation). Without this an
/// evaluation-only or handoff-only run — which `run_id_from_metrics`
/// happily recognises — would land at `UNIX_EPOCH` on the index and
/// silently show `final_status = "ok"` even if it had a delegation
/// failure or a background-task error.
fn derive_run_summary(run_id: &str, metrics: &AgentMetrics) -> crate::trace_store::RunSummary {
    use std::time::{Duration, UNIX_EPOCH};

    // `agent_id` falls back through every span kind that carries a
    // populated `SpanContext.agent_id` via the same iterator the
    // attribution aggregation uses. A standalone evaluation /
    // suspension / handoff / delegation run would otherwise land with
    // an empty agent_id on the index, which breaks `agent_id`
    // filtering on the list endpoint for that run.
    let agent_id = iter_span_contexts(metrics)
        .map(|c| c.agent_id.clone())
        .find(|s| !s.is_empty())
        .unwrap_or_default();

    // started_at / ended_at: bracket the run from every span kind that
    // carries timestamps. Handoff and evaluation are instantaneous —
    // their `timestamp_ms` contributes to both bounds. Suspension and
    // delegation carry an optional `duration_ms`, so the end bound is
    // `timestamp_ms + duration_ms.unwrap_or(0)`. Background tasks use
    // their `created_at_ms` / `completed_at_ms` pair.
    let mut starts: Vec<u64> = metrics.inferences.iter().map(|s| s.started_at_ms).collect();
    starts.extend(metrics.tools.iter().map(|s| s.started_at_ms));
    starts.extend(metrics.background_tasks.iter().map(|s| s.created_at_ms));
    starts.extend(metrics.evaluations.iter().map(|e| e.timestamp_ms));
    starts.extend(metrics.suspensions.iter().map(|s| s.timestamp_ms));
    starts.extend(metrics.handoffs.iter().map(|s| s.timestamp_ms));
    starts.extend(metrics.delegations.iter().map(|s| s.timestamp_ms));

    let mut ends: Vec<u64> = metrics.inferences.iter().map(|s| s.ended_at_ms).collect();
    ends.extend(metrics.tools.iter().map(|s| s.ended_at_ms));
    ends.extend(
        metrics
            .background_tasks
            .iter()
            .filter_map(|s| s.completed_at_ms),
    );
    ends.extend(metrics.evaluations.iter().map(|e| e.timestamp_ms));
    ends.extend(
        metrics
            .suspensions
            .iter()
            .map(|s| s.timestamp_ms.saturating_add(s.duration_ms.unwrap_or(0))),
    );
    ends.extend(metrics.handoffs.iter().map(|s| s.timestamp_ms));
    ends.extend(
        metrics
            .delegations
            .iter()
            .map(|s| s.timestamp_ms.saturating_add(s.duration_ms.unwrap_or(0))),
    );

    let started_at = starts
        .iter()
        .filter(|t| **t > 0)
        .min()
        .copied()
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap_or(UNIX_EPOCH);
    let ended_at = ends
        .iter()
        .filter(|t| **t > 0)
        .max()
        .copied()
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms));

    // Attribution fields fall back across every span kind via
    // `iter_span_contexts`. Reading only from inference spans would
    // leave non-inference-only runs (handoff-only / evaluation-only /
    // background-only) without prompt_id / experiment attribution on
    // the index, so filter queries like `/v1/traces?prompt_id=…` would
    // silently miss them even though the SpanContext records the value.
    let mut prompt_ids: Vec<String> = iter_span_contexts(metrics)
        .filter_map(|c| c.prompt_id.clone())
        .collect();
    prompt_ids.sort();
    prompt_ids.dedup();

    let experiment_id = iter_span_contexts(metrics).find_map(|c| c.experiment_id.clone());
    let variant_name = iter_span_contexts(metrics).find_map(|c| c.variant_name.clone());

    let had_error = run_had_error(metrics);
    let final_status = Some(if had_error { "error" } else { "ok" }.to_string());

    // F18: derive judge_score using the same min-aggregation the sampling
    // path uses (see `on_run_end` above). Surfacing it on the index lets
    // `/v1/traces` list explain why a low-scoring run was kept and lets
    // operators sort by score.
    let judge_score = metrics
        .evaluations
        .iter()
        .filter_map(|e| e.score_value)
        .map(|v| v as f32)
        .fold(None::<f32>, |acc, v| Some(acc.map_or(v, |a| a.min(v))));

    crate::trace_store::RunSummary {
        run_id: run_id.to_string(),
        agent_id,
        started_at,
        ended_at,
        prompt_ids,
        experiment_id,
        variant_name,
        final_status,
        judge_score,
    }
}

#[cfg(test)]
mod tracestore_adapter_tests {
    use super::*;
    use crate::metrics::{GenAISpan, MetricsEvent, SpanContext};
    use crate::trace_store::file::FileTraceStore;

    fn span(run_id: &str) -> GenAISpan {
        GenAISpan {
            context: SpanContext {
                run_id: run_id.into(),
                ..Default::default()
            },
            step_index: None,
            model: "m".into(),
            provider: "p".into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: vec![],
            error_type: None,
            error_class: None,
            input_tokens: Some(1),
            output_tokens: Some(2),
            total_tokens: Some(3),
            thinking_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: vec![],
            duration_ms: 0,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    #[test]
    fn persistent_sink_writes_through_trace_store() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-adapter-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = std::sync::Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: std::sync::Arc<dyn MetricsSink> =
            std::sync::Arc::new(crate::sink::InMemorySink::new());
        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap();

        sink.record(&MetricsEvent::Inference(span("01HXTRACE")));
        sink.record(&MetricsEvent::Inference(span("01HXTRACE")));

        let events = store.read("01HXTRACE").unwrap();
        assert_eq!(
            events.len(),
            2,
            "events appended through adapter must round-trip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper: build a metrics struct from a single inference span.
    fn metrics_from_span(s: GenAISpan) -> AgentMetrics {
        AgentMetrics {
            inferences: vec![s],
            ..Default::default()
        }
    }

    /// Helper: build a metrics struct combining a single inference and a
    /// judge evaluation event with the given score. Used by F14 tests.
    fn metrics_with_judge(s: GenAISpan, judge_score: f64) -> AgentMetrics {
        AgentMetrics {
            inferences: vec![s.clone()],
            evaluations: vec![EvaluationResultEvent {
                context: s.context,
                name: "test-judge".into(),
                score_value: Some(judge_score),
                score_label: None,
                explanation: None,
                response_id: None,
                error_type: None,
                timestamp_ms: 0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn sampling_policy_always_flushes_on_run_end() {
        use crate::sampling::SamplingMode;
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-sampling-always-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
            normal_traces: SamplingMode::Always,
            ..SamplingPolicy::default()
        }));

        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap()
        .with_sampling_policy(policy);

        let s = span("01HXSAMPL1");
        sink.record(&MetricsEvent::Inference(s.clone()));
        sink.record(&MetricsEvent::Inference(s.clone()));

        // Before run_end: nothing written yet.
        assert!(
            store.read("01HXSAMPL1").is_err(),
            "buffered events must not appear before on_run_end"
        );

        sink.on_run_end(&metrics_from_span(s));

        let events = store.read("01HXSAMPL1").unwrap();
        assert_eq!(events.len(), 2, "both events flushed after on_run_end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sampling_policy_never_drops_buffer_on_run_end() {
        use crate::sampling::SamplingMode;
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-sampling-never-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
            normal_traces: SamplingMode::Never,
            error_traces: SamplingMode::Never,
            ..SamplingPolicy::default()
        }));

        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap()
        .with_sampling_policy(policy);

        let s = span("01HXSAMPL2");
        sink.record(&MetricsEvent::Inference(s.clone()));
        sink.on_run_end(&metrics_from_span(s));

        // Policy says Never: run should not appear in the store.
        let result = store.read("01HXSAMPL2");
        assert!(
            result.is_err(),
            "events must be dropped when policy is Never"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_run_persists_despite_low_normal_sampling() {
        use crate::sampling::SamplingMode;
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-error-run-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        // normal_traces=Never but error_traces=Always — the error span must
        // still make it through.
        let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
            normal_traces: SamplingMode::Never,
            error_traces: SamplingMode::Always,
            ..SamplingPolicy::default()
        }));

        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap()
        .with_sampling_policy(policy);

        let error_span = GenAISpan {
            context: SpanContext {
                run_id: "01HXERRORRUN".into(),
                ..Default::default()
            },
            error_type: Some("rate_limited".into()),
            ..span("01HXERRORRUN")
        };
        sink.record(&MetricsEvent::Inference(error_span.clone()));

        let metrics = AgentMetrics {
            inferences: vec![error_span],
            ..Default::default()
        };
        sink.on_run_end(&metrics);

        let events = store.read("01HXERRORRUN").unwrap();
        assert_eq!(events.len(), 1, "error run must be flushed to trace store");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_only_run_surfaces_on_list() {
        // Regression for F21: a run with no inference/tool spans (only
        // BackgroundTask events) used to be silently skipped at
        // on_run_end — `run_id_from_metrics` returned None, no index was
        // written, and `/v1/traces` couldn't surface the run. Including
        // background_tasks in the fallback chain fixes this.
        use crate::metrics::SpanContext;
        use awaken_runtime::extensions::background::TaskStatus;
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-bg-only-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());
        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap();

        let bg = BackgroundTaskSpan {
            context: SpanContext {
                run_id: "01HXBGRUN".into(),
                agent_id: "bg-agent".into(),
                ..SpanContext::default()
            },
            task_id: "bg".into(),
            task_type: "summarise".into(),
            task_name: None,
            description: "x".into(),
            status: TaskStatus::Running,
            parent_task_id: None,
            error_message: None,
            created_at_ms: 1_000,
            completed_at_ms: None,
        };
        sink.record(&MetricsEvent::BackgroundTask(bg.clone()));
        let metrics = AgentMetrics {
            background_tasks: vec![bg],
            ..Default::default()
        };
        sink.on_run_end(&metrics);

        let runs = store
            .list(&crate::trace_store::TraceFilter::default())
            .unwrap();
        let entry = runs.iter().find(|r| r.run_id == "01HXBGRUN");
        assert!(
            entry.is_some(),
            "background-only run must produce an index entry; got: {:?}",
            runs
        );
        assert_eq!(entry.unwrap().agent_id, "bg-agent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_summary_brackets_evaluation_only_run_at_event_timestamp() {
        // Round 7 #2: a standalone evaluation event (no inference/tool/bg)
        // must surface a real `started_at`/`ended_at` on the index instead
        // of falling back to UNIX_EPOCH. Without the timestamp coverage
        // `run_id_from_metrics` would still find the run, but list ordering
        // and retention would treat it as 1970-01-01.
        use crate::metrics::{EvaluationResultEvent, SpanContext};
        use std::time::UNIX_EPOCH;

        let event = EvaluationResultEvent {
            context: SpanContext {
                run_id: "01HXEVALONLY".into(),
                agent_id: "judge-agent".into(),
                ..SpanContext::default()
            },
            name: "exact_match".into(),
            score_label: None,
            score_value: Some(0.9),
            explanation: None,
            response_id: None,
            error_type: None,
            timestamp_ms: 1_700_000_000_000,
        };
        let metrics = AgentMetrics {
            evaluations: vec![event],
            ..Default::default()
        };
        let summary = derive_run_summary("01HXEVALONLY", &metrics);
        assert_eq!(summary.agent_id, "judge-agent");
        assert_ne!(
            summary.started_at, UNIX_EPOCH,
            "evaluation-only run must not land at UNIX_EPOCH on the index"
        );
        assert!(summary.ended_at.is_some());
        assert_eq!(summary.final_status.as_deref(), Some("ok"));
    }

    #[test]
    fn run_summary_brackets_handoff_and_delegation_runs() {
        // Round 7 #2: standalone handoff and delegation events also
        // contribute to the time bracket. Delegation's `duration_ms`
        // extends the end bound past `timestamp_ms`.
        use crate::metrics::{DelegationSpan, HandoffSpan, SpanContext};
        use std::time::UNIX_EPOCH;

        let handoff = HandoffSpan {
            context: SpanContext {
                run_id: "01HXHANDOFF".into(),
                agent_id: "agent-b".into(),
                ..SpanContext::default()
            },
            from_agent_id: "agent-a".into(),
            to_agent_id: "agent-b".into(),
            reason: None,
            timestamp_ms: 1_700_000_000_000,
        };
        let delegation = DelegationSpan {
            context: SpanContext {
                run_id: "01HXHANDOFF".into(),
                agent_id: "agent-b".into(),
                ..SpanContext::default()
            },
            parent_run_id: "01HXHANDOFF".into(),
            child_run_id: None,
            target_agent_id: "sub-agent".into(),
            tool_call_id: "tc1".into(),
            duration_ms: Some(5_000),
            success: true,
            error_message: None,
            timestamp_ms: 1_700_000_001_000,
        };
        let metrics = AgentMetrics {
            handoffs: vec![handoff],
            delegations: vec![delegation],
            ..Default::default()
        };
        let summary = derive_run_summary("01HXHANDOFF", &metrics);
        assert_ne!(summary.started_at, UNIX_EPOCH);
        // ended_at must include the delegation's timestamp + duration.
        let ended = summary.ended_at.expect("ended_at populated");
        let ended_ms = ended.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        assert_eq!(ended_ms, 1_700_000_006_000);
    }

    #[test]
    fn run_summary_aggregates_attribution_from_non_inference_spans() {
        // Round 8 #1: prompt_id / experiment_id / variant_name must
        // surface on the index for inference-less runs too. Previously
        // these were read only from `metrics.inferences`, so an
        // evaluation-only / handoff-only / background-only run would
        // land with empty attribution and `/v1/traces?prompt_id=…`
        // would silently miss it.
        use crate::metrics::{EvaluationResultEvent, HandoffSpan, SpanContext};
        use awaken_runtime::extensions::background::TaskStatus;

        let ctx = |label: &str| SpanContext {
            run_id: "01HXAGGR".into(),
            agent_id: "agent-x".into(),
            prompt_id: Some(format!("prompt-{label}")),
            experiment_id: Some(format!("exp-{label}")),
            variant_name: Some(format!("variant-{label}")),
            ..SpanContext::default()
        };

        // Evaluation, handoff, and a background task all carry their
        // own SpanContext with attribution. No inference span at all.
        let evaluation = EvaluationResultEvent {
            context: ctx("eval"),
            name: "judge".into(),
            score_label: None,
            score_value: Some(0.7),
            explanation: None,
            response_id: None,
            error_type: None,
            timestamp_ms: 1_000,
        };
        let handoff = HandoffSpan {
            context: ctx("handoff"),
            from_agent_id: "agent-a".into(),
            to_agent_id: "agent-x".into(),
            reason: None,
            timestamp_ms: 1_500,
        };
        let bg = BackgroundTaskSpan {
            context: ctx("bg"),
            task_id: "t".into(),
            task_type: "x".into(),
            task_name: None,
            description: "x".into(),
            status: TaskStatus::Running,
            parent_task_id: None,
            error_message: None,
            created_at_ms: 2_000,
            completed_at_ms: Some(3_000),
        };

        let metrics = AgentMetrics {
            evaluations: vec![evaluation],
            handoffs: vec![handoff],
            background_tasks: vec![bg],
            ..Default::default()
        };
        let summary = derive_run_summary("01HXAGGR", &metrics);
        assert_eq!(summary.agent_id, "agent-x");
        // All three contexts contributed distinct prompt_ids (sorted/deduped).
        let mut expected = ["prompt-eval", "prompt-handoff", "prompt-bg"]
            .map(String::from)
            .to_vec();
        expected.sort();
        assert_eq!(summary.prompt_ids, expected);
        // First-non-None wins: iter order is inferences→tools→
        // evaluations→…, so the evaluation context's experiment id is
        // picked. Any of the three would be correct; the assertion
        // pins the iteration order so an accidental reordering shows
        // up here.
        assert_eq!(summary.experiment_id.as_deref(), Some("exp-eval"));
        assert_eq!(summary.variant_name.as_deref(), Some("variant-eval"));
    }

    #[test]
    fn run_summary_final_status_treats_failed_task_without_message_as_error() {
        // Round 8 #2: a background task with `status == Failed` but
        // `error_message == None` must still flip `final_status` to
        // "error" — the producer is not contractually required to fill
        // the message, so relying on it alone hides real failures from
        // the index and the sampling gate.
        use crate::metrics::SpanContext;
        use awaken_runtime::extensions::background::TaskStatus;

        let bg = BackgroundTaskSpan {
            context: SpanContext {
                run_id: "01HXBGFAIL".into(),
                agent_id: "a".into(),
                ..SpanContext::default()
            },
            task_id: "t".into(),
            task_type: "x".into(),
            task_name: None,
            description: "x".into(),
            status: TaskStatus::Failed,
            parent_task_id: None,
            error_message: None,
            created_at_ms: 1,
            completed_at_ms: Some(2),
        };
        let m = AgentMetrics {
            background_tasks: vec![bg],
            ..Default::default()
        };
        assert_eq!(
            derive_run_summary("01HXBGFAIL", &m).final_status.as_deref(),
            Some("error"),
            "Failed background task without error_message must still flip final_status to error"
        );
        // And the sampling gate must see the same error signal.
        assert!(
            run_had_error(&m),
            "Failed background task must satisfy the sampling-gate error definition"
        );
    }

    #[test]
    fn run_summary_final_status_covers_non_inference_errors() {
        // Round 7 #2: delegation failure, background-task error, and
        // evaluation error must all flip `final_status` to "error".
        // Suspensions and handoffs are status transitions, not failures,
        // and so are not asserted to influence the flag.
        use crate::metrics::{DelegationSpan, EvaluationResultEvent, SpanContext};
        use awaken_runtime::extensions::background::TaskStatus;

        let ctx = || SpanContext {
            run_id: "01HXFAIL".into(),
            agent_id: "a".into(),
            ..SpanContext::default()
        };

        let delegation_fail = DelegationSpan {
            context: ctx(),
            parent_run_id: "01HXFAIL".into(),
            child_run_id: None,
            target_agent_id: "child".into(),
            tool_call_id: "tc".into(),
            duration_ms: Some(1),
            success: false,
            error_message: Some("denied".into()),
            timestamp_ms: 1,
        };
        let m = AgentMetrics {
            delegations: vec![delegation_fail],
            ..Default::default()
        };
        assert_eq!(
            derive_run_summary("01HXFAIL", &m).final_status.as_deref(),
            Some("error"),
            "delegation !success must flip final_status to error"
        );

        let bg_error = BackgroundTaskSpan {
            context: ctx(),
            task_id: "t".into(),
            task_type: "x".into(),
            task_name: None,
            description: "x".into(),
            status: TaskStatus::Running,
            parent_task_id: None,
            error_message: Some("oom".into()),
            created_at_ms: 1,
            completed_at_ms: Some(2),
        };
        let m = AgentMetrics {
            background_tasks: vec![bg_error],
            ..Default::default()
        };
        assert_eq!(
            derive_run_summary("01HXFAIL", &m).final_status.as_deref(),
            Some("error"),
            "background-task error_message must flip final_status to error"
        );

        let eval_error = EvaluationResultEvent {
            context: ctx(),
            name: "x".into(),
            score_label: None,
            score_value: None,
            explanation: None,
            response_id: None,
            error_type: Some("timeout".into()),
            timestamp_ms: 1,
        };
        let m = AgentMetrics {
            evaluations: vec![eval_error],
            ..Default::default()
        };
        assert_eq!(
            derive_run_summary("01HXFAIL", &m).final_status.as_deref(),
            Some("error"),
            "evaluation error_type must flip final_status to error"
        );
    }

    #[test]
    fn run_summary_index_carries_judge_score() {
        // Regression for F18: prior `derive_run_summary` hardcoded
        // judge_score=None even when the sampling path read it from
        // EvaluationResultEvent. Surface it on the index so list() can
        // explain a low-score retention and operators can sort/filter.
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-summary-score-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        // No sampling policy → immediate path; index written at on_run_end.
        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap();

        let s = span("01HXJUDGESUM");
        sink.record(&MetricsEvent::Inference(s.clone()));
        sink.on_run_end(&metrics_with_judge(s, 0.3));

        let runs = store
            .list(&crate::trace_store::TraceFilter::default())
            .unwrap();
        let entry = runs.iter().find(|r| r.run_id == "01HXJUDGESUM").unwrap();
        assert_eq!(
            entry.judge_score.map(|v| (v * 100.0).round() / 100.0),
            Some(0.3),
            "RunSummary index must carry the derived judge_score, not None"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn low_judge_score_promotes_run_under_normal_never_policy() {
        // Regression for F14: prior `on_run_end` hardcoded judge_score
        // to None, so the `low_judge_score` policy never fired. With
        // F14 the sink derives judge_score from the recorded
        // EvaluationResultEvent — a low-scoring run is now persisted
        // even when the normal-traces policy is `Never`.
        use crate::sampling::SamplingMode;
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-judge-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
            normal_traces: SamplingMode::Never,
            // Defaults: low_judge_score = Always, threshold = 0.5
            ..SamplingPolicy::default()
        }));

        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap()
        .with_sampling_policy(policy);

        let s = span("01HXJUDGE");
        sink.record(&MetricsEvent::Inference(s.clone()));
        // Low score (< default threshold 0.5) should override the
        // Never normal_traces policy.
        sink.on_run_end(&metrics_with_judge(s, 0.2));

        let events = store.read("01HXJUDGE").unwrap();
        assert_eq!(events.len(), 1, "low-judge run must persist");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn on_run_end_writes_run_summary_index_for_immediate_path() {
        // Regression for F3: prior `on_run_end` only flushed events. Without
        // a `write_index_for_run` call, `list()` could not surface the run
        // even though its `.ndjson` existed. This test pins the index path.
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-index-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        // No sampling policy → immediate write-through path.
        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap();

        let s = span("01HXINDEX");
        sink.record(&MetricsEvent::Inference(s.clone()));
        sink.on_run_end(&metrics_from_span(s));

        let runs = store
            .list(&crate::trace_store::TraceFilter::default())
            .unwrap();
        assert_eq!(
            runs.len(),
            1,
            "list() must surface the run via its index file"
        );
        assert_eq!(runs[0].run_id, "01HXINDEX");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overflowed_run_is_dropped_not_flushed_as_tail_fragment() {
        // Regression: prior implementation called `entry.clear()` on overflow
        // and kept buffering, which produced a tail-fragment trace at run_end.
        // The `RunBuffer::Overflowed` enum state must prevent any further
        // events from being written for the overflowing run.
        use crate::sampling::SamplingMode;
        use std::sync::Arc;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("awaken-ts-overflow-{now}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileTraceStore::new(&dir).unwrap());
        let inner: Arc<dyn MetricsSink> = Arc::new(crate::sink::InMemorySink::new());

        let policy = Arc::new(parking_lot::RwLock::new(SamplingPolicy {
            normal_traces: SamplingMode::Always,
            ..SamplingPolicy::default()
        }));

        let sink = PersistentSink::with_trace_store(
            inner,
            store.clone(),
            PersistenceConfig {
                storage_dir: dir.clone(),
                ..PersistenceConfig::default()
            },
        )
        .unwrap()
        .with_sampling_policy(policy);

        let s = span("01HXOVERFLOW");
        assert_eq!(sink.overflow_count(), 0, "starts at zero");
        // Push MAX + 1 events to force the overflow transition.
        for _ in 0..=MAX_BUFFERED_EVENTS_PER_RUN {
            sink.record(&MetricsEvent::Inference(s.clone()));
        }
        assert_eq!(
            sink.overflow_count(),
            1,
            "overflow_count must increment exactly once when the buffer cap is hit"
        );

        sink.on_run_end(&metrics_from_span(s));

        // Overflowed run must not be persisted at all — never a partial
        // tail-fragment.
        assert!(
            store.read("01HXOVERFLOW").is_err(),
            "overflowed run must be dropped at on_run_end, not flushed as a fragment"
        );
        // Counter survives `on_run_end` so wiring banners can read it.
        assert_eq!(sink.overflow_count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemorySink;
    use crate::metrics::{DelegationSpan, GenAISpan, HandoffSpan, SuspensionSpan, ToolSpan};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A sink whose `flush()` can be toggled to fail.
    struct FailableSink {
        inner: InMemorySink,
        fail_flush: Arc<AtomicBool>,
    }

    impl FailableSink {
        fn new(fail_flush: bool) -> Self {
            Self {
                inner: InMemorySink::new(),
                fail_flush: Arc::new(AtomicBool::new(fail_flush)),
            }
        }
    }

    impl MetricsSink for FailableSink {
        fn record(&self, event: &MetricsEvent) {
            self.inner.record(event);
        }
        fn on_run_end(&self, metrics: &AgentMetrics) {
            self.inner.on_run_end(metrics);
        }
        fn flush(&self) -> Result<(), SinkError> {
            if self.fail_flush.load(Ordering::Relaxed) {
                Err(SinkError::new("flush failed"))
            } else {
                Ok(())
            }
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("awaken-persistent-sink-test")
            .join(name)
            .join(uuid::Uuid::now_v7().hyphenated().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_genai_span() -> GenAISpan {
        GenAISpan {
            context: crate::metrics::SpanContext::default(),
            step_index: None,
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
            operation: "chat".to_string(),
            response_model: None,
            response_id: None,
            finish_reasons: vec!["end_turn".to_string()],
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 200,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn sample_tool_span() -> ToolSpan {
        ToolSpan {
            context: crate::metrics::SpanContext::default(),
            step_index: None,
            name: "read_file".to_string(),
            operation: "execute".to_string(),
            call_id: "call_1".to_string(),
            tool_type: "function".to_string(),
            call_arguments: None,
            call_result: None,
            error_type: None,
            duration_ms: 50,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    fn sample_suspension_span() -> SuspensionSpan {
        SuspensionSpan {
            context: crate::metrics::SpanContext::default(),
            tool_call_id: "c1".to_string(),
            tool_name: "search".to_string(),
            action: "suspended".to_string(),
            resume_mode: None,
            duration_ms: None,
            timestamp_ms: 1000,
        }
    }

    fn sample_handoff_span() -> HandoffSpan {
        HandoffSpan {
            context: crate::metrics::SpanContext::default(),
            from_agent_id: "agent-a".to_string(),
            to_agent_id: "agent-b".to_string(),
            reason: Some("escalation".to_string()),
            timestamp_ms: 2000,
        }
    }

    fn sample_delegation_span() -> DelegationSpan {
        DelegationSpan {
            context: crate::metrics::SpanContext::default(),
            parent_run_id: "run-1".to_string(),
            child_run_id: Some("run-2".to_string()),
            target_agent_id: "worker".to_string(),
            tool_call_id: "c1".to_string(),
            duration_ms: Some(500),
            success: true,
            error_message: None,
            timestamp_ms: 3000,
        }
    }

    #[test]
    fn persistent_sink_delegates_to_inner() {
        let inner = Arc::new(InMemorySink::new());
        let config = PersistenceConfig {
            storage_dir: test_dir("delegates"),
            ..Default::default()
        };
        let sink = PersistentSink::new(Arc::clone(&inner) as Arc<dyn MetricsSink>, config).unwrap();

        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));
        sink.record(&MetricsEvent::Suspension(sample_suspension_span()));
        sink.record(&MetricsEvent::Handoff(sample_handoff_span()));
        sink.record(&MetricsEvent::Delegation(sample_delegation_span()));
        sink.on_run_end(&AgentMetrics {
            session_duration_ms: 5000,
            ..Default::default()
        });

        let metrics = inner.metrics();
        assert_eq!(metrics.inferences.len(), 1);
        assert_eq!(metrics.tools.len(), 1);
        assert_eq!(metrics.suspensions.len(), 1);
        assert_eq!(metrics.handoffs.len(), 1);
        assert_eq!(metrics.delegations.len(), 1);
        assert_eq!(metrics.session_duration_ms, 5000);
    }

    #[test]
    fn persistent_sink_persists_on_flush_failure() {
        let failable = Arc::new(FailableSink::new(true));
        let dir = test_dir("flush-fail");
        let config = PersistenceConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };
        let sink =
            PersistentSink::new(Arc::clone(&failable) as Arc<dyn MetricsSink>, config).unwrap();

        sink.record(&MetricsEvent::Inference(sample_genai_span()));
        sink.record(&MetricsEvent::Tool(sample_tool_span()));

        let result = sink.flush();
        assert!(result.is_err());

        // Verify an NDJSON file was created
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "ndjson"))
            .collect();
        assert_eq!(files.len(), 1);

        // Verify file has 2 lines (one per event)
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn persistent_sink_retry_replays_persisted() {
        let inner = Arc::new(InMemorySink::new());
        let dir = test_dir("retry-replay");
        let config = PersistenceConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };
        let sink = PersistentSink::new(Arc::clone(&inner) as Arc<dyn MetricsSink>, config).unwrap();

        // Manually create an NDJSON file with events
        let lines = vec![
            PersistedLine::Event(Box::new(MetricsEvent::Inference(sample_genai_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Tool(sample_tool_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Suspension(sample_suspension_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Handoff(sample_handoff_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Delegation(sample_delegation_span()))),
            PersistedLine::RunEnd {
                line_type: RunEndMarker::RunEnd,
                session_duration_ms: 9000,
            },
        ];
        let path = dir.join("failed_events_manual.ndjson");
        let mut file = std::fs::File::create(&path).unwrap();
        for line in &lines {
            writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
        drop(file);

        let replayed = sink.retry_persisted().unwrap();
        assert_eq!(replayed, 6);

        let metrics = inner.metrics();
        assert_eq!(metrics.inferences.len(), 1);
        assert_eq!(metrics.tools.len(), 1);
        assert_eq!(metrics.suspensions.len(), 1);
        assert_eq!(metrics.handoffs.len(), 1);
        assert_eq!(metrics.delegations.len(), 1);
        assert_eq!(metrics.session_duration_ms, 9000);
    }

    #[test]
    fn persistent_sink_retry_deletes_file_on_success() {
        let inner = Arc::new(InMemorySink::new());
        let dir = test_dir("retry-delete");
        let config = PersistenceConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };
        let sink = PersistentSink::new(Arc::clone(&inner) as Arc<dyn MetricsSink>, config).unwrap();

        // Create an NDJSON file
        let path = dir.join("failed_events_delete_test.ndjson");
        let line = PersistedLine::Event(Box::new(MetricsEvent::Inference(sample_genai_span())));
        std::fs::write(&path, serde_json::to_string(&line).unwrap() + "\n").unwrap();

        assert!(path.exists());
        sink.retry_persisted().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn persisted_line_serde_roundtrip() {
        let lines = vec![
            PersistedLine::Event(Box::new(MetricsEvent::Inference(sample_genai_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Tool(sample_tool_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Suspension(sample_suspension_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Handoff(sample_handoff_span()))),
            PersistedLine::Event(Box::new(MetricsEvent::Delegation(sample_delegation_span()))),
            PersistedLine::RunEnd {
                line_type: RunEndMarker::RunEnd,
                session_duration_ms: 42000,
            },
        ];

        for line in &lines {
            let json = serde_json::to_string(line).unwrap();
            let restored: PersistedLine = serde_json::from_str(&json).unwrap();
            // Verify round-trip by re-serializing
            let json2 = serde_json::to_string(&restored).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn persistent_sink_config_defaults() {
        let config = PersistenceConfig::default();
        assert_eq!(
            config.storage_dir,
            std::env::temp_dir().join("awaken-persistent-sink")
        );
        assert_eq!(config.max_retry_attempts, 8);
        assert_eq!(config.base_backoff, Duration::from_millis(500));
        assert_eq!(config.max_backoff, Duration::from_secs(30));
    }
}
