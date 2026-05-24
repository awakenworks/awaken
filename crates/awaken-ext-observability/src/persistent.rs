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
pub(crate) fn run_id_from_metrics(metrics: &AgentMetrics) -> Option<String> {
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
pub(crate) fn derive_run_summary(
    run_id: &str,
    metrics: &AgentMetrics,
) -> crate::trace_store::RunSummary {
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
#[path = "persistent_adapter_test.rs"]
mod tracestore_adapter_tests;

#[cfg(test)]
#[path = "persistent_test.rs"]
mod tests;
