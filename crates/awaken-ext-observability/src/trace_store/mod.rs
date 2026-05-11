//! Persistence layer for run-scoped trace events.  See ADR-0030 D3-D4.

pub mod file;

use std::collections::HashSet;
use std::time::SystemTime;

use crate::metrics::MetricsEvent;

/// Why a run is referenced (and therefore exempt from prune).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceKind {
    /// Pinned by an eval dataset item (ADR-0032 D5).
    Dataset,
    /// Pinned by an experiment guardrail evidence record (ADR-0031).
    ExperimentEvidence,
    /// Pinned manually via the admin console.
    OperatorPin,
}

/// Filter for the `list` query.  All fields are AND-combined.
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    pub agent_id: Option<String>,
    pub prompt_id: Option<String>,
    pub experiment_id: Option<String>,
    pub variant_name: Option<String>,
    pub since: Option<SystemTime>,
    pub limit: Option<usize>,
}

/// One row in `list` results.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub run_id: String,
    pub agent_id: String,
    pub started_at: SystemTime,
    pub ended_at: Option<SystemTime>,
    pub prompt_ids: Vec<String>,
    pub experiment_id: Option<String>,
    pub variant_name: Option<String>,
    pub final_status: Option<String>,
    pub judge_score: Option<f32>,
}

/// Persistence + query API.  Implementations append events as they arrive,
/// re-read whole runs on demand, and prune unreferenced runs on a TTL.
pub trait TraceStore: Send + Sync {
    /// Append one event to the run's shard.  Idempotent only by file
    /// position — callers must not retry without dedupe at a higher layer.
    fn append(&self, run_id: &str, event: &MetricsEvent) -> Result<(), TraceStoreError>;

    /// Read every event recorded for `run_id` in append order.  Tolerates a
    /// partial trailing line (e.g. crash mid-write).
    fn read(&self, run_id: &str) -> Result<Vec<MetricsEvent>, TraceStoreError>;

    /// Return summaries matching `filter`, sorted by `started_at` desc.
    fn list(&self, filter: &TraceFilter) -> Result<Vec<RunSummary>, TraceStoreError>;

    /// Mark `run_id` as referenced so `prune` skips it.
    fn mark_referenced(&self, run_id: &str, by: ReferenceKind) -> Result<(), TraceStoreError>;

    /// Delete every shard whose `started_at` is older than `older_than` and
    /// whose run id is not in `except_referenced`.  Returns the number of
    /// shards deleted.
    fn prune(
        &self,
        older_than: SystemTime,
        except_referenced: &HashSet<String>,
    ) -> Result<u64, TraceStoreError>;

    /// Persist a `RunSummary` index for `run_id` so `list` can return the
    /// run without rescanning events. Called by `PersistentSink::on_run_end`
    /// after the event stream is finalised, so the index always points at
    /// committed shards.
    fn write_index_for_run(
        &self,
        run_id: &str,
        summary: &RunSummary,
    ) -> Result<(), TraceStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TraceStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("run {run_id} not found")]
    NotFound { run_id: String },
    #[error("invalid run id: {0}")]
    InvalidRunId(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_summary_eq_struct_assemble() {
        let s = RunSummary {
            run_id: "01HX".into(),
            agent_id: "weather".into(),
            started_at: SystemTime::UNIX_EPOCH,
            ended_at: None,
            prompt_ids: vec!["a1b2c3d4e5f6".into()],
            experiment_id: None,
            variant_name: None,
            final_status: None,
            judge_score: None,
        };
        assert_eq!(s.run_id, "01HX");
        assert_eq!(s.prompt_ids[0].len(), 12);
    }

    #[test]
    fn trace_filter_default_is_empty() {
        let f = TraceFilter::default();
        assert!(f.agent_id.is_none());
        assert!(f.prompt_id.is_none());
        assert!(f.limit.is_none());
    }
}
