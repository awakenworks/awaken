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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffEntry {
    /// Both reports are present, both `passed`, and every observable
    /// metric matched. No change.
    Unchanged { fixture_id: String },
    /// Baseline passed but the new run failed — a *regression*.
    Regression {
        fixture_id: String,
        new_failures: Vec<String>,
    },
    /// Baseline failed but the new run passed — a *fix*.
    Fixed { fixture_id: String },
    /// Both runs failed; failure set differs.
    StillFailing {
        fixture_id: String,
        new_failures: Vec<String>,
    },
    /// Both runs passed but at least one observable metric drifted
    /// (final text, token counts, tool counts, error_type, etc.).
    /// Surfaces silent regressions that don't change the pass/fail bit
    /// — e.g. an inference being dropped from `inference_count` while
    /// the answer-substring expectation still happens to match.
    Drift {
        fixture_id: String,
        fields: Vec<String>,
    },
    /// Fixture only present in the baseline (deleted or filtered).
    MissingFromNew { fixture_id: String },
    /// Fixture only present in the new run (added).
    NewlyAdded { fixture_id: String, passed: bool },
}

impl DiffEntry {
    pub fn fixture_id(&self) -> &str {
        match self {
            DiffEntry::Unchanged { fixture_id }
            | DiffEntry::Regression { fixture_id, .. }
            | DiffEntry::Fixed { fixture_id }
            | DiffEntry::StillFailing { fixture_id, .. }
            | DiffEntry::Drift { fixture_id, .. }
            | DiffEntry::MissingFromNew { fixture_id }
            | DiffEntry::NewlyAdded { fixture_id, .. } => fixture_id,
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
        .map(|id| match (baseline_map.get(id), new_map.get(id)) {
            (Some(b), Some(n)) => match (b.passed, n.passed) {
                (true, true) => {
                    let fields = diff_passing_fields(b, n);
                    if fields.is_empty() {
                        DiffEntry::Unchanged {
                            fixture_id: id.to_string(),
                        }
                    } else {
                        DiffEntry::Drift {
                            fixture_id: id.to_string(),
                            fields,
                        }
                    }
                }
                (true, false) => DiffEntry::Regression {
                    fixture_id: id.to_string(),
                    new_failures: n.failures.iter().map(|f| f.kind().to_string()).collect(),
                },
                (false, true) => DiffEntry::Fixed {
                    fixture_id: id.to_string(),
                },
                (false, false) => DiffEntry::StillFailing {
                    fixture_id: id.to_string(),
                    new_failures: n.failures.iter().map(|f| f.kind().to_string()).collect(),
                },
            },
            (Some(_), None) => DiffEntry::MissingFromNew {
                fixture_id: id.to_string(),
            },
            (None, Some(n)) => DiffEntry::NewlyAdded {
                fixture_id: id.to_string(),
                passed: n.passed,
            },
            (None, None) => unreachable!("id collected from at least one side"),
        })
        .collect();

    DiffSummary { entries }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expectation::Failure;
    use std::io::Cursor;

    fn report(id: &str, passed: bool, failures: Vec<Failure>) -> ReplayReport {
        ReplayReport {
            fixture_id: id.into(),
            passed,
            failures,
            final_text: format!("text-{id}"),
            inference_count: 1,
            tool_count: 0,
            tool_failures: 0,
            total_input_tokens: 10,
            total_output_tokens: 5,
            total_tokens: 15,
            session_duration_ms: 100,
            elapsed_ms: 100,
            tool_calls_by_agent: Vec::new(),
            error_type: None,
            inference_error_count: 0,
            runtime_failure: None,
        }
    }

    fn token_failure() -> Failure {
        Failure::TokenBudgetExceeded {
            budget: 100,
            actual: 200,
        }
    }

    // ── write/read NDJSON ───────────────────────────────────────────

    #[test]
    fn ndjson_write_then_read_roundtrip() {
        let mut reports = vec![
            report("alpha", true, vec![]),
            report("beta", false, vec![token_failure()]),
        ];
        let mut buf = Vec::new();
        write_ndjson(&mut buf, &reports).unwrap();
        let parsed = read_ndjson(Cursor::new(&buf)).unwrap();
        // `elapsed_ms` is excluded from the serialised baseline (see
        // `ReplayReport::elapsed_ms`), so it deserialises back as 0.
        for r in &mut reports {
            r.elapsed_ms = 0;
        }
        assert_eq!(parsed, reports);
    }

    #[test]
    fn ndjson_one_line_per_report() {
        let reports = vec![report("a", true, vec![]), report("b", true, vec![])];
        let mut buf = Vec::new();
        write_ndjson(&mut buf, &reports).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn ndjson_skips_blank_lines() {
        let payload = "\n\n";
        let parsed = read_ndjson(Cursor::new(payload.as_bytes())).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn ndjson_read_returns_parse_error_for_garbage() {
        let payload = "{\"valid\": true}\nnot-json\n";
        let err = read_ndjson(Cursor::new(payload.as_bytes())).unwrap_err();
        match err {
            ReportError::Parse { line, .. } => {
                // First non-empty line is line 1; the bad line is line 2
                // OR line 1 if "valid": true alone fails (different schema).
                // We just assert line is between 1 and 2 inclusive.
                assert!((1..=2).contains(&line), "unexpected line {line}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn ndjson_path_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reports.ndjson");
        let mut reports = vec![report("x", true, vec![])];
        write_ndjson_path(&path, &reports).unwrap();
        assert!(path.exists());
        let read = read_ndjson_path(&path).unwrap();
        for r in &mut reports {
            r.elapsed_ms = 0;
        }
        assert_eq!(read, reports);
    }

    #[test]
    fn ndjson_path_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/reports.ndjson");
        write_ndjson_path(&path, &[report("x", true, vec![])]).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn ndjson_read_path_io_error_for_missing_file() {
        let err = read_ndjson_path("/nonexistent/awaken-eval/missing.ndjson").unwrap_err();
        match err {
            ReportError::Io { .. } => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    // ── diff_against_baseline ───────────────────────────────────────

    #[test]
    fn diff_unchanged_when_both_pass() {
        let s = diff_against_baseline(&[report("a", true, vec![])], &[report("a", true, vec![])]);
        assert!(s.is_clean());
        assert_eq!(s.regressions(), 0);
        assert!(matches!(
            &s.entries[0],
            DiffEntry::Unchanged { fixture_id } if fixture_id == "a"
        ));
    }

    #[test]
    fn diff_regression_when_baseline_passed_new_failed() {
        let s = diff_against_baseline(
            &[report("a", true, vec![])],
            &[report("a", false, vec![token_failure()])],
        );
        assert_eq!(s.regressions(), 1);
        assert!(!s.is_clean());
        match &s.entries[0] {
            DiffEntry::Regression {
                fixture_id,
                new_failures,
            } => {
                assert_eq!(fixture_id, "a");
                assert_eq!(new_failures, &vec!["token_budget_exceeded".to_string()]);
            }
            other => panic!("expected Regression, got {other:?}"),
        }
    }

    #[test]
    fn diff_fixed_when_baseline_failed_new_passed() {
        let s = diff_against_baseline(
            &[report("a", false, vec![token_failure()])],
            &[report("a", true, vec![])],
        );
        assert_eq!(s.regressions(), 0);
        assert!(s.is_clean());
        assert!(matches!(
            &s.entries[0],
            DiffEntry::Fixed { fixture_id } if fixture_id == "a"
        ));
    }

    #[test]
    fn diff_still_failing_does_not_block_ci() {
        let s = diff_against_baseline(
            &[report("a", false, vec![token_failure()])],
            &[report("a", false, vec![token_failure()])],
        );
        assert!(
            s.is_clean(),
            "still-failing should not block when baseline already failed"
        );
    }

    #[test]
    fn diff_missing_blocks_ci() {
        let s = diff_against_baseline(&[report("gone", true, vec![])], &[]);
        assert_eq!(s.missing(), 1);
        assert!(!s.is_clean());
    }

    #[test]
    fn diff_newly_added_does_not_block_ci() {
        let s = diff_against_baseline(&[], &[report("new", true, vec![])]);
        assert_eq!(s.added(), 1);
        assert!(s.is_clean());
        assert!(matches!(
            &s.entries[0],
            DiffEntry::NewlyAdded { fixture_id, passed: true } if fixture_id == "new"
        ));
    }

    #[test]
    fn diff_newly_added_failing_blocks_check() {
        // Review v3 #4: a newly added failing fixture should block
        // `awaken-eval check` so a broken fixture committed today
        // actually fails CI. Previously this silently passed because
        // the baseline never blessed it.
        let s = diff_against_baseline(&[], &[report("new", false, vec![token_failure()])]);
        assert_eq!(s.added(), 1);
        assert!(!s.is_clean());
    }

    #[test]
    fn diff_newly_added_passing_does_not_block() {
        // A newly added fixture that already passes is still
        // informational — baseline never blessed it, but the new run is
        // green, so the gate doesn't need to fire.
        let s = diff_against_baseline(&[], &[report("new", true, vec![])]);
        assert_eq!(s.added(), 1);
        assert!(s.is_clean());
    }

    #[test]
    fn diff_entries_sorted_by_id() {
        let s = diff_against_baseline(
            &[report("zeta", true, vec![]), report("alpha", true, vec![])],
            &[
                report("beta", true, vec![]),
                report("alpha", true, vec![]),
                report("zeta", true, vec![]),
            ],
        );
        let ids: Vec<&str> = s.entries.iter().map(DiffEntry::fixture_id).collect();
        assert_eq!(ids, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn diff_entry_is_blocking_for_regression_missing_and_drift() {
        assert!(
            !DiffEntry::Unchanged {
                fixture_id: "x".into()
            }
            .is_blocking()
        );
        assert!(
            DiffEntry::Regression {
                fixture_id: "x".into(),
                new_failures: vec![]
            }
            .is_blocking()
        );
        assert!(
            !DiffEntry::Fixed {
                fixture_id: "x".into()
            }
            .is_blocking()
        );
        assert!(
            !DiffEntry::StillFailing {
                fixture_id: "x".into(),
                new_failures: vec![]
            }
            .is_blocking()
        );
        assert!(
            DiffEntry::Drift {
                fixture_id: "x".into(),
                fields: vec!["final_text".into()],
            }
            .is_blocking()
        );
        assert!(
            DiffEntry::MissingFromNew {
                fixture_id: "x".into()
            }
            .is_blocking()
        );
        assert!(
            !DiffEntry::NewlyAdded {
                fixture_id: "x".into(),
                passed: true
            }
            .is_blocking()
        );
        assert!(
            DiffEntry::NewlyAdded {
                fixture_id: "x".into(),
                passed: false
            }
            .is_blocking(),
            "newly added failing fixture must block check"
        );
    }

    #[test]
    fn diff_passing_pair_with_matching_metrics_is_unchanged() {
        let b = report("a", true, vec![]);
        let n = report("a", true, vec![]);
        let s = diff_against_baseline(&[b], &[n]);
        assert!(matches!(&s.entries[0], DiffEntry::Unchanged { .. }));
        assert!(s.is_clean());
    }

    #[test]
    fn diff_passing_pair_with_drifted_final_text_is_drift_and_blocks() {
        let b = report("a", true, vec![]);
        let mut n = report("a", true, vec![]);
        n.final_text = "different".into();
        let s = diff_against_baseline(&[b], &[n]);
        assert_eq!(s.drift(), 1);
        assert!(!s.is_clean());
        match &s.entries[0] {
            DiffEntry::Drift { fields, .. } => {
                assert_eq!(fields, &vec!["final_text".to_string()]);
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn diff_passing_pair_with_drifted_inference_count_is_drift() {
        // The motivating case from review v2 #5: failure path drops from
        // inference_count: 1 to inference_count: 0 while the expectation
        // (`final_answer_excludes`) still happens to pass.
        let b = report("a", true, vec![]);
        let mut n = report("a", true, vec![]);
        n.inference_count = 0;
        let s = diff_against_baseline(&[b], &[n]);
        match &s.entries[0] {
            DiffEntry::Drift { fields, .. } => {
                assert_eq!(fields, &vec!["inference_count".to_string()]);
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn diff_passing_pair_with_drifted_total_tokens_only_is_drift() {
        // Token-only providers can report TokenUsage.total_tokens
        // without prompt/completion breakdown, so the report's
        // total_input/output_tokens both stay 0. total_tokens is what
        // scoring actually uses — drift on it must be observable.
        let b = report("a", true, vec![]);
        let mut n = report("a", true, vec![]);
        n.total_tokens = 999;
        let s = diff_against_baseline(&[b], &[n]);
        match &s.entries[0] {
            DiffEntry::Drift { fields, .. } => {
                assert_eq!(fields, &vec!["total_tokens".to_string()]);
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn diff_passing_pair_lists_every_drifted_field() {
        let b = report("a", true, vec![]);
        let mut n = report("a", true, vec![]);
        n.total_input_tokens = 9999;
        n.total_output_tokens = 9999;
        n.error_type = Some("rate_limit".into());
        let s = diff_against_baseline(&[b], &[n]);
        match &s.entries[0] {
            DiffEntry::Drift { fields, .. } => {
                assert_eq!(
                    fields,
                    &vec![
                        "total_input_tokens".to_string(),
                        "total_output_tokens".to_string(),
                        "error_type".to_string(),
                    ]
                );
            }
            other => panic!("expected Drift, got {other:?}"),
        }
    }

    #[test]
    fn diff_summary_serde_roundtrip() {
        let s = diff_against_baseline(
            &[
                report("a", true, vec![]),
                report("b", false, vec![token_failure()]),
            ],
            &[
                report("a", false, vec![token_failure()]),
                report("b", true, vec![]),
            ],
        );
        let json = serde_json::to_string(&s).unwrap();
        let parsed: DiffSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }
}
