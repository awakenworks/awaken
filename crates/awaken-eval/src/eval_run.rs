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

/// Execution semantics used to produce an [`EvalRun`].
///
/// Persisting this on the run record keeps `provider_script` replay
/// (deterministic CI smoke tests) distinct from Live provider evaluation
/// (real model/agent behaviour). Older run JSON did not carry the field;
/// deserialisation defaults those records to `Scripted`, which matches
/// the historical behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunExecutionMode {
    #[default]
    Scripted,
    Live,
}

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
    /// How this run executed its fixtures.
    #[serde(default)]
    pub execution_mode: EvalRunExecutionMode,
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
    /// Returned by [`EvalRunStore::write`] when a run with the same id
    /// already exists on disk. Eval runs are write-once / immutable; the
    /// store will not silently clobber a prior run.
    #[error("eval run {0} already exists")]
    AlreadyExists(String),
    /// Returned by [`EvalRunStore::write`] / [`EvalRunStore::read`]
    /// when `run.items` contains duplicate `(fixture_id, cell,
    /// sample_index)` keys.
    /// `diff_against_baseline` / `diff_eval_items` collect items into a
    /// `BTreeMap` keyed on that triple — duplicates would silently
    /// overwrite each other and produce an insertion-order-dependent
    /// DiffSummary. The store rejects the write so an invalid run can't
    /// land on disk in the first place.
    #[error("eval run {0} contains duplicate item keys: {1}")]
    DuplicateItemKeys(String, String),
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
    pub execution_mode: EvalRunExecutionMode,
    pub started_at_secs: u64,
    pub item_count: usize,
    pub passed_count: usize,
}

/// Per-(fixture, cell) roll-up across flakiness samples. The boolean
/// `pass_at_k` here uses the run's emitted `samples` as `k`: at least
/// one sample passed. `pass_pow_k` means every emitted sample passed.
/// This is intentionally a direct empirical roll-up of the run, not a
/// statistical estimator for an unobserved larger sample population.
///
/// Single-sample runs (default) trivially have `pass_at_k == pass_pow_k`
/// equal to the lone sample's pass bit — the aggregate is still
/// well-formed but adds no signal beyond the underlying [`ReplayReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleAggregate {
    pub fixture_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<MatrixCell>,
    /// Number of [`EvalRunItem`]s contributing to this group.
    pub samples: u32,
    /// How many of those `samples` had `report.passed == true`.
    pub passed: u32,
    /// `passed / samples`; `0.0` when `samples == 0` (never emitted in
    /// practice since `aggregate_samples` skips empty groups).
    pub pass_rate: f64,
    /// `passed >= 1` — at least one sample passed. The pass@k semantic
    /// commonly used for "can the agent succeed at all".
    pub pass_at_k: bool,
    /// `passed == samples` — every sample passed. The pass^k semantic
    /// used for reliability-critical agents.
    pub pass_pow_k: bool,
}

impl EvalRun {
    /// Group `items` by `(fixture_id, cell)` and produce one
    /// [`SampleAggregate`] per group. Groups are sorted by
    /// `(fixture_id, cell)` for stable output. Empty `items` produces
    /// an empty `Vec` (no spurious zero-aggregates).
    pub fn aggregate_samples(&self) -> Vec<SampleAggregate> {
        let mut groups: std::collections::BTreeMap<(String, MatrixCell), (u32, u32)> =
            Default::default();
        for item in &self.items {
            let key = (
                item.fixture_id.clone(),
                item.cell.clone().unwrap_or_default(),
            );
            let entry = groups.entry(key).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(1); // samples
            if item.report.passed {
                entry.1 = entry.1.saturating_add(1); // passed
            }
        }
        groups
            .into_iter()
            .map(|((fixture_id, cell), (samples, passed))| {
                let cell_opt = if cell == MatrixCell::default() {
                    None
                } else {
                    Some(cell)
                };
                let pass_rate = if samples == 0 {
                    0.0
                } else {
                    f64::from(passed) / f64::from(samples)
                };
                SampleAggregate {
                    fixture_id,
                    cell: cell_opt,
                    samples,
                    passed,
                    pass_rate,
                    pass_at_k: passed >= 1,
                    pass_pow_k: passed == samples && samples > 0,
                }
            })
            .collect()
    }
}

impl From<&EvalRun> for EvalRunSummary {
    fn from(run: &EvalRun) -> Self {
        let passed_count = run.items.iter().filter(|i| i.report.passed).count();
        Self {
            id: run.id.clone(),
            dataset_id: run.dataset_id.clone(),
            dataset_revision: run.dataset_revision,
            execution_mode: run.execution_mode,
            started_at_secs: run.started_at_secs,
            item_count: run.items.len(),
            passed_count,
        }
    }
}

/// Persistence + query API for [`EvalRun`]s.
///
/// Implementations MUST reject writes whose `items` carry duplicate
/// `(fixture_id, cell, sample_index)` keys and must not return such runs
/// from normal query paths ([`EvalRunStoreError::DuplicateItemKeys`]).
/// The matrix diff path (`diff_against_baseline` / `diff_eval_items`)
/// keys items on that triple and silently overwrites collisions in its
/// `BTreeMap`; exposing a duplicate-key run turns every later baseline
/// diff against that run into an order-dependent lie. The check belongs
/// at the store boundary so dataset runs, online runs, and future entry
/// points all share the same invariant — see
/// [`crate::validate_unique_item_keys`].
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

fn validate_run_item_keys(run: &EvalRun) -> Result<(), EvalRunStoreError> {
    crate::report::validate_unique_item_keys(&run.items)
        .map_err(|e| EvalRunStoreError::DuplicateItemKeys(run.id.clone(), e))
}

impl EvalRunStore for FileEvalRunStore {
    fn write(&self, run: &EvalRun) -> Result<(), EvalRunStoreError> {
        validate_run_id(&run.id)?;
        // Write-once semantics take priority over payload validation:
        // if a run id already exists, the caller is attempting to
        // clobber immutable storage regardless of what the new payload
        // contains.
        if self.locate(&run.id).is_some() {
            return Err(EvalRunStoreError::AlreadyExists(run.id.clone()));
        }
        // Enforce the trait-level invariant at the store boundary so
        // every entry point (dataset matrix runs, online ad-hoc runs,
        // future bulk-import tooling) is guarded uniformly. See the
        // EvalRunStore trait doc for why duplicate (fixture_id, cell,
        // sample_index) keys would silently break baseline diffs.
        validate_run_item_keys(run)?;
        // Resolve started_at_secs locally (only for shard routing) AND
        // also clone it into the persisted run, so the on-disk record
        // never carries a 0 sentinel that diverges from its location.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let start = if run.started_at_secs == 0 {
            now_secs
        } else {
            run.started_at_secs
        };
        let mut persisted = run.clone();
        if persisted.started_at_secs == 0 {
            persisted.started_at_secs = start;
        }
        let shard = self.shard_dir(start);
        fs::create_dir_all(&shard)?;
        let path = shard.join(format!("{}.json", run.id));
        let bytes = serde_json::to_vec_pretty(&persisted)?;
        // True atomic create-or-fail: open the FINAL path with O_EXCL
        // (`create_new(true)`). If two writers race past the locate()
        // check above, only ONE open succeeds; the loser maps to
        // AlreadyExists. The earlier rename(tmp, path) silently
        // clobbered on Unix — we no longer trust that.
        //
        // A crash between `open` and `write_all` leaves a 0-byte file
        // which `read` will fail on (serde_json on empty input). That
        // surfaces as a corrupt-store error rather than silent data
        // loss, which is the right side of the trade — the alternative
        // (tmp + rename) couldn't make AlreadyExists fail-fast.
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(&bytes)?;
                f.sync_all()?;
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(EvalRunStoreError::AlreadyExists(run.id.clone()))
            }
            Err(err) => Err(EvalRunStoreError::Io(err)),
        }
    }

    fn read(&self, run_id: &str) -> Result<EvalRun, EvalRunStoreError> {
        let path = self
            .locate(run_id)
            .ok_or_else(|| EvalRunStoreError::NotFound(run_id.into()))?;
        let bytes = fs::read(&path)?;
        let run: EvalRun = serde_json::from_slice(&bytes)?;
        validate_run_item_keys(&run)?;
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
                // Corrupt / partially-written records must NOT disappear
                // silently — eval runs are the source of truth for the
                // diff/list API and a missing run looks identical to "no
                // such id" from upstream. Log loud and continue; admins
                // can grep the warning + delete the file or restore from
                // backup.
                let bytes = match fs::read(&path) {
                    Ok(b) => b,
                    Err(err) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "FileEvalRunStore: skipping unreadable eval-run file"
                        );
                        continue;
                    }
                };
                let run: EvalRun = match serde_json::from_slice(&bytes) {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "FileEvalRunStore: skipping corrupt eval-run file"
                        );
                        continue;
                    }
                };
                if let Err(err) = validate_run_item_keys(&run) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "FileEvalRunStore: skipping invalid eval-run file"
                    );
                    continue;
                }
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
