//! NDJSON report writer + baseline diff.
//!
//! A *report* is a directory or file (NDJSON, one [`ReplayReport`] per
//! line) emitted by the `awaken-eval replay` command.  CI gates use
//! [`diff_against_baseline`] to compare a fresh report against a committed
//! baseline and surface any regressions.
//!
//! NDJSON is chosen over a single JSON document so reports stream as
//! fixtures execute and partial reports are still parseable when a run is
//! interrupted.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::outcome::ReplayReport;

/// Errors raised while reading or writing reports.
#[derive(Debug, Error)]
pub enum ReportError {
    #[error("report I/O failed: {path}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("report contains invalid JSON at line {line}")]
    Parse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// Serialise `reports` as NDJSON to `writer`.
pub fn write_ndjson<W: Write>(
    writer: &mut W,
    reports: &[ReplayReport],
) -> Result<(), std::io::Error> {
    for r in reports {
        let line = serde_json::to_string(r).expect("ReplayReport serializes infallibly");
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

/// Convenience wrapper that creates `path` (and any missing parents) and
/// writes the report.
pub fn write_ndjson_path(
    path: impl AsRef<Path>,
    reports: &[ReplayReport],
) -> Result<(), ReportError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| ReportError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut file = fs::File::create(path).map_err(|source| ReportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    write_ndjson(&mut file, reports).map_err(|source| ReportError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Parse NDJSON from `reader`.
///
/// Blank lines are tolerated so editors that auto-append a trailing newline
/// don't break parsing.
pub fn read_ndjson<R: BufRead>(reader: R) -> Result<Vec<ReplayReport>, ReportError> {
    let mut reports = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| ReportError::Io {
            path: std::path::PathBuf::from("<reader>"),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let report = serde_json::from_str(&line).map_err(|source| ReportError::Parse {
            line: idx + 1,
            source,
        })?;
        reports.push(report);
    }
    Ok(reports)
}

/// Read NDJSON from a file on disk.
pub fn read_ndjson_path(path: impl AsRef<Path>) -> Result<Vec<ReplayReport>, ReportError> {
    let path = path.as_ref();
    let file = fs::File::open(path).map_err(|source| ReportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    read_ndjson(BufReader::new(file))
}

/// One row of the baseline-vs-new comparison.
///
/// Every variant carries an optional `cell: Option<MatrixCell>`. For
/// non-matrix runs (CLI `awaken-eval check`, dataset runs without a
/// `models` axis) the field stays `None` and the wire shape is
/// unchanged. For matrix runs the diff pairer keys by
/// `(fixture_id, cell)` so two cells of the same fixture stay
/// independent entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffEntry {
    /// Both reports are present, both `passed`, and every observable
    /// metric matched. No change.
    Unchanged {
        fixture_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
    /// Baseline passed but the new run failed — a *regression*.
    Regression {
        fixture_id: String,
        new_failures: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
    /// Baseline failed but the new run passed — a *fix*.
    Fixed {
        fixture_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
    /// Both runs failed; failure set differs.
    StillFailing {
        fixture_id: String,
        new_failures: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
    /// Both runs passed but at least one observable metric drifted
    /// (final text, token counts, tool counts, error_type, etc.).
    /// Surfaces silent regressions that don't change the pass/fail bit
    /// — e.g. an inference being dropped from `inference_count` while
    /// the answer-substring expectation still happens to match.
    Drift {
        fixture_id: String,
        fields: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
    /// Fixture only present in the baseline (deleted or filtered).
    MissingFromNew {
        fixture_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
    /// Fixture only present in the new run (added).
    NewlyAdded {
        fixture_id: String,
        passed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<crate::eval_run::MatrixCell>,
    },
}

impl DiffEntry {
    pub fn fixture_id(&self) -> &str {
        match self {
            DiffEntry::Unchanged { fixture_id, .. }
            | DiffEntry::Regression { fixture_id, .. }
            | DiffEntry::Fixed { fixture_id, .. }
            | DiffEntry::StillFailing { fixture_id, .. }
            | DiffEntry::Drift { fixture_id, .. }
            | DiffEntry::MissingFromNew { fixture_id, .. }
            | DiffEntry::NewlyAdded { fixture_id, .. } => fixture_id,
        }
    }

    /// Matrix cell that produced this entry. `None` for non-matrix runs.
    pub fn cell(&self) -> Option<&crate::eval_run::MatrixCell> {
        match self {
            DiffEntry::Unchanged { cell, .. }
            | DiffEntry::Regression { cell, .. }
            | DiffEntry::Fixed { cell, .. }
            | DiffEntry::StillFailing { cell, .. }
            | DiffEntry::Drift { cell, .. }
            | DiffEntry::MissingFromNew { cell, .. }
            | DiffEntry::NewlyAdded { cell, .. } => cell.as_ref(),
        }
    }

    /// Whether this entry should fail a CI gate. Regressions, missing
    /// fixtures, field-level drift, and newly-added *failing* fixtures
    /// are blocking — drift is included because a silently changing
    /// baseline is exactly the kind of slow regression the eval gate
    /// exists to catch; a newly added failing fixture is included so
    /// `awaken-eval check` actually fails when a fresh fixture lands in
    /// a broken state (otherwise the gate would only catch already-
    /// committed-passing fixtures going red).
    pub fn is_blocking(&self) -> bool {
        match self {
            DiffEntry::Regression { .. }
            | DiffEntry::MissingFromNew { .. }
            | DiffEntry::Drift { .. } => true,
            DiffEntry::NewlyAdded { passed, .. } => !*passed,
            DiffEntry::Unchanged { .. }
            | DiffEntry::Fixed { .. }
            | DiffEntry::StillFailing { .. } => false,
        }
    }
}

/// Field names compared between two passing reports. Order is stable so
/// `Drift::fields` reads consistently across runs.
fn diff_passing_fields(b: &ReplayReport, n: &ReplayReport) -> Vec<String> {
    let mut diffs: Vec<&'static str> = Vec::new();
    if b.final_text != n.final_text {
        diffs.push("final_text");
    }
    if b.inference_count != n.inference_count {
        diffs.push("inference_count");
    }
    if b.tool_count != n.tool_count {
        diffs.push("tool_count");
    }
    if b.tool_failures != n.tool_failures {
        diffs.push("tool_failures");
    }
    if b.total_input_tokens != n.total_input_tokens {
        diffs.push("total_input_tokens");
    }
    if b.total_output_tokens != n.total_output_tokens {
        diffs.push("total_output_tokens");
    }
    if b.total_tokens != n.total_tokens {
        diffs.push("total_tokens");
    }
    if b.session_duration_ms != n.session_duration_ms {
        diffs.push("session_duration_ms");
    }
    if b.error_type != n.error_type {
        diffs.push("error_type");
    }
    if b.inference_error_count != n.inference_error_count {
        diffs.push("inference_error_count");
    }
    if b.runtime_failure != n.runtime_failure {
        diffs.push("runtime_failure");
    }
    if b.tool_calls_by_agent != n.tool_calls_by_agent {
        diffs.push("tool_calls_by_agent");
    }
    diffs.into_iter().map(String::from).collect()
}

/// Aggregate result of a baseline diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub entries: Vec<DiffEntry>,
}

impl DiffSummary {
    /// True when no entry would fail a CI gate.
    pub fn is_clean(&self) -> bool {
        !self.entries.iter().any(DiffEntry::is_blocking)
    }

    /// Count of regressions (baseline passed, new failed).
    pub fn regressions(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, DiffEntry::Regression { .. }))
            .count()
    }

    /// Count of fixtures missing from the new run.
    pub fn missing(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, DiffEntry::MissingFromNew { .. }))
            .count()
    }

    /// Count of fixtures newly added in the new run.
    pub fn added(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, DiffEntry::NewlyAdded { .. }))
            .count()
    }

    /// Count of fixtures with field-level drift (both runs passed but
    /// at least one observable metric changed).
    pub fn drift(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, DiffEntry::Drift { .. }))
            .count()
    }
}

/// Compare a `new` run against a committed `baseline`, producing a
/// [`DiffSummary`] suitable for CI gating.
///
/// Pairing is by `fixture_id`. The returned `entries` are sorted by id.
pub fn diff_against_baseline(baseline: &[ReplayReport], new: &[ReplayReport]) -> DiffSummary {
    let baseline_map: BTreeMap<&str, &ReplayReport> = baseline
        .iter()
        .map(|r| (r.fixture_id.as_str(), r))
        .collect();
    let new_map: BTreeMap<&str, &ReplayReport> =
        new.iter().map(|r| (r.fixture_id.as_str(), r)).collect();

    let mut all_ids: Vec<&str> = baseline_map.keys().copied().collect();
    for id in new_map.keys() {
        if !all_ids.contains(id) {
            all_ids.push(id);
        }
    }
    all_ids.sort();

    let entries = all_ids
        .into_iter()
        .map(|id| {
            pair_to_entry(
                id.to_string(),
                None,
                baseline_map.get(id).copied(),
                new_map.get(id).copied(),
            )
        })
        .collect();

    DiffSummary { entries }
}

/// Pair eval-run items by `(fixture_id, cell)` for matrix-aware
/// comparison, then produce a [`DiffSummary`]. Two cells of the same
/// fixture become independent entries — a regression in
/// `(alpha, claude-opus)` doesn't collide with `(alpha, gpt-4o)`. Used
/// by the server's `compute_diff` when at least one item carries a
/// matrix cell. CLI `awaken-eval check` keeps using the
/// `ReplayReport`-based [`diff_against_baseline`] for its NDJSON flow.
pub fn diff_eval_items(
    baseline: &[crate::eval_run::EvalRunItem],
    new: &[crate::eval_run::EvalRunItem],
) -> DiffSummary {
    // Owned `(String, MatrixCell)` keys keep the lifetime story simple
    // and let us BTreeMap-key without a static empty-cell trick. The
    // clone cost is negligible (cells are tiny — at most a model_id).
    type Key = (String, crate::eval_run::MatrixCell);
    let key_of = |item: &crate::eval_run::EvalRunItem| -> Key {
        (
            item.fixture_id.clone(),
            item.cell.clone().unwrap_or_default(),
        )
    };

    let baseline_map: BTreeMap<Key, &crate::eval_run::EvalRunItem> =
        baseline.iter().map(|i| (key_of(i), i)).collect();
    let new_map: BTreeMap<Key, &crate::eval_run::EvalRunItem> =
        new.iter().map(|i| (key_of(i), i)).collect();

    let mut all_keys: Vec<Key> = baseline_map.keys().cloned().collect();
    for k in new_map.keys() {
        if !all_keys.contains(k) {
            all_keys.push(k.clone());
        }
    }
    all_keys.sort();

    let entries = all_keys
        .into_iter()
        .map(|key| {
            let (fixture_id, cell) = &key;
            let cell_opt = if *cell == crate::eval_run::MatrixCell::default() {
                None
            } else {
                Some(cell.clone())
            };
            let b = baseline_map.get(&key).map(|i| &i.report);
            let n = new_map.get(&key).map(|i| &i.report);
            pair_to_entry(fixture_id.clone(), cell_opt, b, n)
        })
        .collect();

    DiffSummary { entries }
}

/// Inner pairing logic shared by [`diff_against_baseline`] (cell = None)
/// and [`diff_eval_items`] (cell may be set). Keeps the four-quadrant
/// pass/fail × pass/fail decision in one place.
fn pair_to_entry(
    fixture_id: String,
    cell: Option<crate::eval_run::MatrixCell>,
    baseline: Option<&ReplayReport>,
    new: Option<&ReplayReport>,
) -> DiffEntry {
    match (baseline, new) {
        (Some(b), Some(n)) => match (b.passed, n.passed) {
            (true, true) => {
                let fields = diff_passing_fields(b, n);
                if fields.is_empty() {
                    DiffEntry::Unchanged { fixture_id, cell }
                } else {
                    DiffEntry::Drift {
                        fixture_id,
                        fields,
                        cell,
                    }
                }
            }
            (true, false) => DiffEntry::Regression {
                fixture_id,
                new_failures: n.failures.iter().map(|f| f.kind().to_string()).collect(),
                cell,
            },
            (false, true) => DiffEntry::Fixed { fixture_id, cell },
            (false, false) => DiffEntry::StillFailing {
                fixture_id,
                new_failures: n.failures.iter().map(|f| f.kind().to_string()).collect(),
                cell,
            },
        },
        (Some(_), None) => DiffEntry::MissingFromNew { fixture_id, cell },
        (None, Some(n)) => DiffEntry::NewlyAdded {
            fixture_id,
            passed: n.passed,
            cell,
        },
        (None, None) => unreachable!("key collected from at least one side"),
    }
}

#[cfg(test)]
#[path = "report_test.rs"]
mod tests;
