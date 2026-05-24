//! `EvalRun` model + filesystem-backed store (ADR-0032 D1).

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
/// (real model/agent behaviour). Legacy records infer the mode only when
/// item cell presence is all-or-none; mixed legacy records are corrupt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunExecutionMode {
    #[default]
    Scripted,
    Live,
}

/// One server-side eval run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "EvalRunWire")]
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

// Deserialisation shadow for legacy JSON without `execution_mode`.
#[derive(Deserialize)]
struct EvalRunWire {
    id: String,
    dataset_id: String,
    dataset_revision: u64,
    #[serde(default)]
    execution_mode: Option<EvalRunExecutionMode>,
    items: Vec<EvalRunItem>,
    started_at_secs: u64,
    ended_at_secs: u64,
}

impl TryFrom<EvalRunWire> for EvalRun {
    type Error = String;

    fn try_from(wire: EvalRunWire) -> Result<Self, Self::Error> {
        let any_with_cell = wire.items.iter().any(|item| item.cell.is_some());
        let any_without_cell = wire.items.iter().any(|item| item.cell.is_none());
        let execution_mode = match wire.execution_mode {
            Some(EvalRunExecutionMode::Live) if any_without_cell => {
                return Err(format!(
                    "eval run {} declares execution_mode=live but contains item(s) without matrix cell",
                    wire.id
                ));
            }
            Some(EvalRunExecutionMode::Scripted) if any_with_cell => {
                return Err(format!(
                    "eval run {} declares execution_mode=scripted but contains item(s) with matrix cell",
                    wire.id
                ));
            }
            Some(mode) => mode,
            None => match (any_with_cell, any_without_cell) {
                (true, false) => EvalRunExecutionMode::Live,
                (false, _) => EvalRunExecutionMode::Scripted,
                (true, true) => {
                    return Err(format!(
                        "legacy eval run {} mixes items with and without matrix cells; execution_mode cannot be inferred",
                        wire.id
                    ));
                }
            },
        };
        validate_execution_shape(&wire.id, execution_mode, &wire.items)?;
        Ok(Self {
            id: wire.id,
            dataset_id: wire.dataset_id,
            dataset_revision: wire.dataset_revision,
            execution_mode,
            items: wire.items,
            started_at_secs: wire.started_at_secs,
            ended_at_secs: wire.ended_at_secs,
        })
    }
}

fn validate_execution_shape(
    run_id: &str,
    execution_mode: EvalRunExecutionMode,
    items: &[EvalRunItem],
) -> Result<(), String> {
    match execution_mode {
        EvalRunExecutionMode::Live if items.iter().any(|item| item.cell.is_none()) => Err(format!(
            "eval run {run_id} declares execution_mode=live but contains item(s) without matrix cell"
        )),
        EvalRunExecutionMode::Scripted if items.iter().any(|item| item.cell.is_some()) => {
            Err(format!(
                "eval run {run_id} declares execution_mode=scripted but contains item(s) with matrix cell"
            ))
        }
        _ => Ok(()),
    }
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
    /// Returned by [`EvalRunStore::write`] when the run's
    /// `execution_mode` disagrees with item cell presence.
    #[error("eval run {0} has invalid execution shape: {1}")]
    InvalidExecutionShape(String, String),
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
    /// Items whose `report.passed == false`. Today every persisted
    /// item carries a report, so `failed_count = item_count -
    /// passed_count`; the field is kept explicit so consumers don't
    /// have to assume that invariant. If a future schema lets items
    /// be stored without a report (partial run), pending items will
    /// be `item_count - passed_count - failed_count`.
    pub failed_count: usize,
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
        let failed_count = run.items.iter().filter(|i| !i.report.passed).count();
        Self {
            id: run.id.clone(),
            dataset_id: run.dataset_id.clone(),
            dataset_revision: run.dataset_revision,
            execution_mode: run.execution_mode,
            started_at_secs: run.started_at_secs,
            item_count: run.items.len(),
            passed_count,
            failed_count,
        }
    }
}

/// Persistence + query API for [`EvalRun`]s.
///
/// Implementations MUST enforce global write-once `run.id`s, reject duplicate
/// `(fixture_id, cell, sample_index)` keys, and keep `execution_mode`
/// consistent with item cell presence.
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
    /// Delete persisted runs older than `older_than_secs`.
    fn prune(&self, older_than_secs: u64) -> Result<u64, EvalRunStoreError>;
}

/// Filesystem-backed [`EvalRunStore`]. Data files mirror
/// `FileTraceStore`: `{root}/eval_runs/{yyyy-mm}/{run_id}.json`.
/// A stable `{root}/eval_runs/.ids/{run_id}` marker provides the
/// cross-shard write-once guard when concurrent writers present the
/// same id with different `started_at_secs` values.
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

    fn id_registry_dir(&self) -> PathBuf {
        self.runs_root().join(".ids")
    }

    fn id_marker_path(&self, run_id: &str) -> PathBuf {
        self.id_registry_dir().join(run_id)
    }

    fn reserve_run_id(
        &self,
        run_id: &str,
        final_path: &Path,
    ) -> Result<PathBuf, EvalRunStoreError> {
        let dir = self.id_registry_dir();
        fs::create_dir_all(&dir)?;
        let marker = self.id_marker_path(run_id);
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(mut f) => {
                // The marker is the cross-shard write-once guard; the
                // content is only for operator debugging.
                let _ = writeln!(f, "{}", final_path.display());
                let _ = f.sync_all();
                Ok(marker)
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(EvalRunStoreError::AlreadyExists(run_id.to_string()))
            }
            Err(err) => Err(EvalRunStoreError::Io(err)),
        }
    }

    fn release_run_id_marker(&self, run_id: &str) {
        let _ = fs::remove_file(self.id_marker_path(run_id));
    }

    fn locate(&self, run_id: &str) -> Option<PathBuf> {
        validate_run_id(run_id).ok()?;
        let root = self.runs_root();
        let entries = fs::read_dir(&root).ok()?;
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || is_internal_dir(&dir) {
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

fn validate_run_shape(run: &EvalRun) -> Result<(), EvalRunStoreError> {
    validate_execution_shape(&run.id, run.execution_mode, &run.items)
        .map_err(|e| EvalRunStoreError::InvalidExecutionShape(run.id.clone(), e))
}

fn validate_loaded_run_identity(
    path: &Path,
    expected_id: Option<&str>,
    run: &EvalRun,
) -> Result<(), EvalRunStoreError> {
    validate_run_id(&run.id)?;
    if let Some(expected) = expected_id
        && run.id != expected
    {
        return Err(EvalRunStoreError::InvalidRunId(format!(
            "file for {expected} contained run id {}",
            run.id
        )));
    }
    let file_stem = path.file_stem().and_then(|stem| stem.to_str());
    if file_stem != Some(run.id.as_str()) {
        return Err(EvalRunStoreError::InvalidRunId(format!(
            "file name {} does not match run id {}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("<invalid>"),
            run.id
        )));
    }
    Ok(())
}

fn is_internal_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

impl EvalRunStore for FileEvalRunStore {
    fn write(&self, run: &EvalRun) -> Result<(), EvalRunStoreError> {
        validate_run_id(&run.id)?;
        // Write-once takes priority over payload validation.
        if self.locate(&run.id).is_some() {
            return Err(EvalRunStoreError::AlreadyExists(run.id.clone()));
        }
        if self.id_marker_path(&run.id).exists() {
            return Err(EvalRunStoreError::AlreadyExists(run.id.clone()));
        }
        // Keep diff invariants at the store boundary.
        validate_run_shape(run)?;
        validate_run_item_keys(run)?;
        // Resolve started_at_secs for both shard routing and persisted JSON.
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
        let marker = self.reserve_run_id(&run.id, &path)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = shard.join(format!(
            ".{}.{}.{}.json.tmp",
            run.id,
            std::process::id(),
            nanos
        ));
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut f) => {
                if let Err(err) = f.write_all(&bytes).and_then(|_| f.sync_all()) {
                    let _ = fs::remove_file(&tmp);
                    let _ = fs::remove_file(&marker);
                    return Err(EvalRunStoreError::Io(err));
                }
            }
            Err(err) => {
                let _ = fs::remove_file(&marker);
                return Err(EvalRunStoreError::Io(err));
            }
        }
        match fs::hard_link(&tmp, &path) {
            Ok(()) => {
                let _ = fs::File::open(&shard).and_then(|dir| dir.sync_all());
                let _ = fs::remove_file(&tmp);
                let _ = fs::File::open(&shard).and_then(|dir| dir.sync_all());
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                let _ = fs::remove_file(&marker);
                Err(EvalRunStoreError::AlreadyExists(run.id.clone()))
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                let _ = fs::remove_file(&marker);
                Err(EvalRunStoreError::Io(err))
            }
        }
    }

    fn read(&self, run_id: &str) -> Result<EvalRun, EvalRunStoreError> {
        validate_run_id(run_id)?;
        let path = self
            .locate(run_id)
            .ok_or_else(|| EvalRunStoreError::NotFound(run_id.into()))?;
        let bytes = fs::read(&path)?;
        let run: EvalRun = serde_json::from_slice(&bytes)?;
        validate_loaded_run_identity(&path, Some(run_id), &run)?;
        validate_run_shape(&run)?;
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
            if !shard.is_dir() || is_internal_dir(&shard) {
                continue;
            }
            for run_entry in fs::read_dir(&shard)? {
                let path = run_entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                // Corrupt records must not disappear silently. Log loud and
                // continue so list APIs remain usable while admins inspect.
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
                if let Err(err) = validate_loaded_run_identity(&path, None, &run) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "FileEvalRunStore: skipping eval-run file with invalid identity"
                    );
                    continue;
                }
                if let Err(err) = validate_run_shape(&run) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "FileEvalRunStore: skipping eval-run file with invalid execution shape"
                    );
                    continue;
                }
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
            if !shard.is_dir() || is_internal_dir(&shard) {
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
                if validate_loaded_run_identity(&path, None, &run)
                    .and_then(|_| validate_run_shape(&run))
                    .and_then(|_| validate_run_item_keys(&run))
                    .is_err()
                {
                    // Invalid historical files should be inspected, not
                    // pruned based on untrusted ids inside their JSON.
                    shard_empty_after = false;
                    continue;
                }
                if run.started_at_secs < older_than_secs {
                    fs::remove_file(&path)?;
                    self.release_run_id_marker(&run.id);
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
