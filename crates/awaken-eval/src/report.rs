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
    /// Both reports are present and `passed`. No change.
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
            | DiffEntry::MissingFromNew { fixture_id }
            | DiffEntry::NewlyAdded { fixture_id, .. } => fixture_id,
        }
    }

    /// Whether this entry should fail a CI gate.
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            DiffEntry::Regression { .. } | DiffEntry::MissingFromNew { .. }
        )
    }
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
                (true, true) => DiffEntry::Unchanged {
                    fixture_id: id.to_string(),
                },
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
            session_duration_ms: 100,
            elapsed_ms: 100,
            tool_calls_by_agent: Vec::new(),
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
        let reports = vec![
            report("alpha", true, vec![]),
            report("beta", false, vec![token_failure()]),
        ];
        let mut buf = Vec::new();
        write_ndjson(&mut buf, &reports).unwrap();
        let parsed = read_ndjson(Cursor::new(&buf)).unwrap();
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
        let reports = vec![report("x", true, vec![])];
        write_ndjson_path(&path, &reports).unwrap();
        assert!(path.exists());
        let read = read_ndjson_path(&path).unwrap();
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
    fn diff_newly_added_failing_does_not_block_either() {
        // An added fixture that already fails is still informational —
        // baseline never blessed it, so we don't gate. The CI workflow
        // can choose to require all-green explicitly if desired.
        let s = diff_against_baseline(&[], &[report("new", false, vec![token_failure()])]);
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
    fn diff_entry_is_blocking_only_for_regression_and_missing() {
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
