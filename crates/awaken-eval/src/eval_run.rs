//! `EvalRun` model + filesystem-backed store (ADR-0032 D1).
//!
//! An [`EvalRun`] is one server-side execution of a dataset against the
//! runtime: each fixture is replayed, scored, and recorded. The run is
//! immutable once written — re-running the same dataset produces a new
//! [`EvalRun`] with a fresh id, leaving the previous one for diffing
//! (ADR-0032 D7).
//!
//! Storage layout (mirrors [`awaken_ext_observability::trace_store`]):
//!
//!   `{root}/eval_runs/{yyyy-mm}/{run_id}.json`
//!
//! One JSON document per run. Write-once + immutable, so unlike the
//! trace store we don't need NDJSON appending or revision bookkeeping.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::outcome::ReplayReport;

/// One server-side eval run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRun {
    /// Globally unique run id (ULID, minted at run start).
    pub id: String,
    /// Dataset that drove the run.
    pub dataset_id: String,
    /// `meta.revision` of the dataset at the moment the run started. A
    /// diff between two runs against different revisions must surface
    /// the schema change instead of pretending the fixtures matched.
    pub dataset_revision: u64,
    /// Per-fixture replay results, in the dataset's fixture order.
    pub items: Vec<EvalRunItem>,
    /// Wall-clock start (epoch seconds).
    pub started_at_secs: u64,
    /// Wall-clock end (epoch seconds). Always populated — runs are
    /// written to storage exactly once, after every fixture has
    /// replayed (`EvalRunStore::write` is the only persistence path).
    pub ended_at_secs: u64,
}

/// One fixture's worth of an [`EvalRun`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunItem {
    /// Fixture id from `Fixture::id`. Stable across runs of the same
    /// dataset; the diff endpoint pairs items by this.
    pub fixture_id: String,
    /// Matrix cell that produced this item. `None` for plain (non-matrix)
    /// runs where `fixture_id` alone is the natural key. When set, the
    /// `(fixture_id, cell)` pair becomes the diff-pairing key so two
    /// matrix runs against the same model are comparable while different
    /// cells of the same fixture stay independent.
    ///
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` so
    /// pre-matrix `EvalRun` JSON on disk parses unchanged and small
    /// non-matrix runs stay compact on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<MatrixCell>,
    /// Replay report — same shape the `awaken-eval replay` CLI writes.
    /// Reusing the type means the existing diff/score code paths apply
    /// unchanged to server-driven runs.
    pub report: ReplayReport,
    /// `run_id` of the replay's [`TraceStore`] write. Lets the admin UI
    /// jump from an eval run item to the full trace it produced
    /// (replays go through the real observability stack with
    /// `awaken.replay=true` set on the spans).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_run_id: Option<String>,
    /// Zero-based index of this sample within a flakiness-sampling run.
    /// `None` for single-sample runs (default, current behaviour) so the
    /// wire shape stays unchanged. Set to `Some(i)` only when the request
    /// explicitly asks for `samples >= 2`. Diff pairing keys include this
    /// field so two samples of the same `(fixture_id, cell)` stay
    /// independent entries instead of silently colliding in a map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_index: Option<u32>,
}

/// One cell of a matrix evaluation. Each axis is optional so the cell
/// shape is forward-compatible: today only the `model_id` axis is
/// populated; adding `temperature` / `prompt_variant` later means new
/// optional fields, no breaking change for existing items.
///
/// `Eq + Hash` lets [`crate::report::diff_against_baseline`] use the
/// pair `(fixture_id, cell)` as a `BTreeMap` key when pairing items.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MatrixCell {
    /// Which model the cell ran against. `None` is the "no model axis"
    /// case (legacy non-matrix items) which the diff pairer treats as
    /// pair-by-fixture-id-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

/// Expand a `models` axis into a vector of [`MatrixCell`]s. Empty input
/// yields a single default cell so callers can iterate uniformly: a
/// plain (non-matrix) fixture is "the 1-cell matrix" under the hood.
pub fn expand_cells(models: &[String]) -> Vec<MatrixCell> {
    if models.is_empty() {
        return vec![MatrixCell::default()];
    }
    models
        .iter()
        .map(|m| MatrixCell {
            model_id: Some(m.clone()),
        })
        .collect()
}

/// Errors raised by [`EvalRunStore`].
#[derive(Debug, Error)]
pub enum EvalRunStoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("eval run {0} not found")]
    NotFound(String),
    #[error("invalid run id: {0}")]
    InvalidRunId(String),
}

/// Filter for [`EvalRunStore::list`] and [`EvalRunStore::list_full`].
#[derive(Debug, Clone, Default)]
pub struct EvalRunFilter {
    /// Limit to runs that exercised this dataset.
    pub dataset_id: Option<String>,
    /// Inclusive lower bound on `started_at_secs`. `None` = no lower bound.
    pub since_secs: Option<u64>,
    /// Exclusive upper bound on `started_at_secs`. `None` = no upper bound.
    pub until_secs: Option<u64>,
    /// Cap on returned entries. `None` = implementation default.
    pub limit: Option<usize>,
}

/// One row in a [`EvalRunStore::list`] result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunSummary {
    pub id: String,
    pub dataset_id: String,
    pub dataset_revision: u64,
    pub started_at_secs: u64,
    pub item_count: usize,
    pub passed_count: usize,
}

impl From<&EvalRun> for EvalRunSummary {
    fn from(run: &EvalRun) -> Self {
        let passed_count = run.items.iter().filter(|i| i.report.passed).count();
        Self {
            id: run.id.clone(),
            dataset_id: run.dataset_id.clone(),
            dataset_revision: run.dataset_revision,
            started_at_secs: run.started_at_secs,
            item_count: run.items.len(),
            passed_count,
        }
    }
}

/// Persistence + query API for [`EvalRun`]s.
pub trait EvalRunStore: Send + Sync {
    fn write(&self, run: &EvalRun) -> Result<(), EvalRunStoreError>;
    fn read(&self, run_id: &str) -> Result<EvalRun, EvalRunStoreError>;
    fn list(&self, filter: &EvalRunFilter) -> Result<Vec<EvalRunSummary>, EvalRunStoreError>;
    /// Full-run variant of `list`. Used by the trend endpoint which needs
    /// per-item aggregates (cost, latency) that `EvalRunSummary` doesn't
    /// carry. Defaults to walking `list` + `read` so custom impls only
    /// override when they can serve full runs in one pass.
    fn list_full(&self, filter: &EvalRunFilter) -> Result<Vec<EvalRun>, EvalRunStoreError> {
        let summaries = self.list(filter)?;
        let mut runs = Vec::with_capacity(summaries.len());
        for s in summaries {
            runs.push(self.read(&s.id)?);
        }
        Ok(runs)
    }
    /// Delete every persisted run whose `started_at_secs` is older than
    /// `older_than_secs`. Returns the number of runs removed. Implementations
    /// should clean up empty shard directories after deleting their last
    /// run so the layout doesn't accumulate hollow `{yyyy-mm}/` shells.
    fn prune(&self, older_than_secs: u64) -> Result<u64, EvalRunStoreError>;
}

/// Filesystem-backed [`EvalRunStore`]. Layout mirrors `FileTraceStore`:
/// `{root}/eval_runs/{yyyy-mm}/{run_id}.json`.
pub struct FileEvalRunStore {
    root: PathBuf,
}

impl FileEvalRunStore {
    /// Create the store, ensuring `root/eval_runs` exists.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, EvalRunStoreError> {
        let root = root.into();
        let runs_dir = root.join("eval_runs");
        fs::create_dir_all(&runs_dir)?;
        Ok(Self { root })
    }

    fn runs_root(&self) -> PathBuf {
        self.root.join("eval_runs")
    }

    fn shard_dir(&self, started_at_secs: u64) -> PathBuf {
        let (year, month) = year_month_utc(started_at_secs as i64);
        self.runs_root().join(format!("{year:04}-{month:02}"))
    }

    fn locate(&self, run_id: &str) -> Option<PathBuf> {
        validate_run_id(run_id).ok()?;
        let root = self.runs_root();
        let entries = fs::read_dir(&root).ok()?;
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let candidate = dir.join(format!("{run_id}.json"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
}

impl EvalRunStore for FileEvalRunStore {
    fn write(&self, run: &EvalRun) -> Result<(), EvalRunStoreError> {
        validate_run_id(&run.id)?;
        let start = if run.started_at_secs == 0 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        } else {
            run.started_at_secs
        };
        let shard = self.shard_dir(start);
        fs::create_dir_all(&shard)?;
        let path = shard.join(format!("{}.json", run.id));
        let bytes = serde_json::to_vec_pretty(run)?;
        // Write atomically via tempfile + rename so a crash mid-write
        // never leaves a partial JSON document where `read` would parse
        // it as malformed.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn read(&self, run_id: &str) -> Result<EvalRun, EvalRunStoreError> {
        let path = self
            .locate(run_id)
            .ok_or_else(|| EvalRunStoreError::NotFound(run_id.into()))?;
        let bytes = fs::read(&path)?;
        let run: EvalRun = serde_json::from_slice(&bytes)?;
        Ok(run)
    }

    fn list(&self, filter: &EvalRunFilter) -> Result<Vec<EvalRunSummary>, EvalRunStoreError> {
        let runs = self.list_full(filter)?;
        let mut summaries: Vec<EvalRunSummary> = runs.iter().map(EvalRunSummary::from).collect();
        summaries.sort_by(|a, b| b.started_at_secs.cmp(&a.started_at_secs));
        if let Some(limit) = filter.limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
    }

    fn list_full(&self, filter: &EvalRunFilter) -> Result<Vec<EvalRun>, EvalRunStoreError> {
        let root = self.runs_root();
        let mut runs: Vec<EvalRun> = Vec::new();
        if !root.exists() {
            return Ok(runs);
        }
        for shard_entry in fs::read_dir(&root)? {
            let shard = shard_entry?.path();
            if !shard.is_dir() {
                continue;
            }
            for run_entry in fs::read_dir(&shard)? {
                let path = run_entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else { continue };
                let Ok(run): Result<EvalRun, _> = serde_json::from_slice(&bytes) else {
                    continue;
                };
                if let Some(ref ds) = filter.dataset_id
                    && &run.dataset_id != ds
                {
                    continue;
                }
                if let Some(since) = filter.since_secs
                    && run.started_at_secs < since
                {
                    continue;
                }
                if let Some(until) = filter.until_secs
                    && run.started_at_secs >= until
                {
                    continue;
                }
                runs.push(run);
            }
        }
        // Newest-first matches `list` ordering. Callers needing time-series
        // (e.g. trend) can re-sort ascending.
        runs.sort_by(|a, b| b.started_at_secs.cmp(&a.started_at_secs));
        if let Some(limit) = filter.limit {
            runs.truncate(limit);
        }
        Ok(runs)
    }

    fn prune(&self, older_than_secs: u64) -> Result<u64, EvalRunStoreError> {
        let root = self.runs_root();
        let mut deleted: u64 = 0;
        if !root.exists() {
            return Ok(0);
        }
        for shard_entry in fs::read_dir(&root)? {
            let shard = shard_entry?.path();
            if !shard.is_dir() {
                continue;
            }
            let mut shard_empty_after = true;
            for run_entry in fs::read_dir(&shard)? {
                let path = run_entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    // .json.tmp from an interrupted write — leave alone.
                    shard_empty_after = false;
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else {
                    shard_empty_after = false;
                    continue;
                };
                let Ok(run): Result<EvalRun, _> = serde_json::from_slice(&bytes) else {
                    // Malformed file blocks the shard from being collected
                    // until the operator removes it manually — silent
                    // deletion would hide real corruption.
                    shard_empty_after = false;
                    continue;
                };
                if run.started_at_secs < older_than_secs {
                    fs::remove_file(&path)?;
                    deleted += 1;
                } else {
                    shard_empty_after = false;
                }
            }
            // Best-effort directory cleanup — ignore failure (concurrent
            // writer may have just landed a new file).
            if shard_empty_after {
                let _ = fs::remove_dir(&shard);
            }
        }
        Ok(deleted)
    }
}

fn validate_run_id(run_id: &str) -> Result<(), EvalRunStoreError> {
    if run_id.is_empty() || run_id.contains(['/', '\\', '\0']) || run_id == "." || run_id == ".." {
        return Err(EvalRunStoreError::InvalidRunId(run_id.into()));
    }
    Ok(())
}

fn year_month_utc(epoch_secs: i64) -> (i32, u32) {
    // Same calendar arithmetic as `FileTraceStore::year_month_utc` —
    // Hinnant's date math, public domain.
    let days = epoch_secs.div_euclid(86_400) + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m)
}

/// Generate a fresh run id: a real ULID (26-char Crockford base32,
/// lexicographically sortable by timestamp, globally unique across
/// processes).
pub fn mint_run_id() -> String {
    ulid::Ulid::new().to_string()
}

#[cfg(test)]
#[path = "eval_run_test.rs"]
mod tests;

/// Public helper to build the absolute on-disk path a run *would* live
/// at, given its `started_at_secs`. Useful for tests that want to
/// pre-populate a shard or assert layout.
pub fn run_path_for(root: &Path, run_id: &str, started_at_secs: u64) -> PathBuf {
    let (year, month) = year_month_utc(started_at_secs as i64);
    root.join("eval_runs")
        .join(format!("{year:04}-{month:02}"))
        .join(format!("{run_id}.json"))
}
