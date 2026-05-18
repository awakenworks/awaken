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
        revision_count: 0,
        judge_score: None,
        cost_usd: None,
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
        DiffEntry::Unchanged { fixture_id, .. } if fixture_id == "a"
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
            ..
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
        DiffEntry::Fixed { fixture_id, .. } if fixture_id == "a"
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
        DiffEntry::NewlyAdded { fixture_id, passed: true, cell: None, .. } if fixture_id == "new"
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
            fixture_id: "x".into(),
            sample_index: None,
            cell: None
        }
        .is_blocking()
    );
    assert!(
        DiffEntry::Regression {
            fixture_id: "x".into(),
            sample_index: None,
            new_failures: vec![],
            cell: None
        }
        .is_blocking()
    );
    assert!(
        !DiffEntry::Fixed {
            fixture_id: "x".into(),
            sample_index: None,
            cell: None
        }
        .is_blocking()
    );
    assert!(
        !DiffEntry::StillFailing {
            fixture_id: "x".into(),
            sample_index: None,
            new_failures: vec![],
            cell: None
        }
        .is_blocking()
    );
    assert!(
        DiffEntry::Drift {
            fixture_id: "x".into(),
            sample_index: None,
            fields: vec!["final_text".into()],
            cell: None
        }
        .is_blocking()
    );
    assert!(
        DiffEntry::MissingFromNew {
            fixture_id: "x".into(),
            sample_index: None,
            cell: None
        }
        .is_blocking()
    );
    assert!(
        !DiffEntry::NewlyAdded {
            fixture_id: "x".into(),
            sample_index: None,
            passed: true,
            cell: None
        }
        .is_blocking()
    );
    assert!(
        DiffEntry::NewlyAdded {
            fixture_id: "x".into(),
            sample_index: None,
            passed: false,
            cell: None
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
fn diff_passing_pair_with_drifted_cost_usd_is_drift() {
    // A silent price bump (same tokens, new dollar amount) must surface
    // as Drift so cost regressions don't sneak past the gate the way
    // they would if only failures were tracked.
    let b = report("a", true, vec![]);
    let mut n = report("a", true, vec![]);
    n.cost_usd = Some(0.025);
    let s = diff_against_baseline(&[b], &[n]);
    match &s.entries[0] {
        DiffEntry::Drift { fields, .. } => {
            assert_eq!(fields, &vec!["cost_usd".to_string()]);
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

// ── Matrix-aware diff (diff_eval_items) ─────────────────────────

use crate::eval_run::{EvalRunItem, MatrixCell, expand_cells};

fn item(fixture_id: &str, passed: bool, model: Option<&str>, text: &str) -> EvalRunItem {
    let mut r = report(fixture_id, passed, vec![]);
    r.final_text = text.into();
    EvalRunItem {
        fixture_id: fixture_id.into(),
        cell: model.map(|m| MatrixCell {
            model_id: Some(m.into()),
        }),
        report: r,
        trace_run_id: None,
        sample_index: None,
    }
}

fn sampled_item(
    fixture_id: &str,
    passed: bool,
    model: Option<&str>,
    sample: u32,
    text: &str,
) -> EvalRunItem {
    let mut i = item(fixture_id, passed, model, text);
    i.sample_index = Some(sample);
    i
}

#[test]
fn expand_cells_empty_returns_single_default_cell() {
    // Non-matrix callers iterate uniformly via expand_cells.
    let cells = expand_cells(&[]);
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0], MatrixCell::default());
}

#[test]
fn expand_cells_one_per_model() {
    let cells = expand_cells(&["claude-opus-4-7".into(), "gpt-4o".into()]);
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0].model_id.as_deref(), Some("claude-opus-4-7"));
    assert_eq!(cells[1].model_id.as_deref(), Some("gpt-4o"));
}

#[test]
fn diff_eval_items_matches_diff_against_baseline_when_no_cells() {
    // Backward-compat: items with no cell should produce the same
    // entries as the report-only diff.
    let baseline = vec![item("a", true, None, "ok")];
    let new = vec![item("a", false, None, "broken")];
    let with_items = diff_eval_items(&baseline, &new);
    let with_reports = diff_against_baseline(
        &baseline
            .iter()
            .map(|i| i.report.clone())
            .collect::<Vec<_>>(),
        &new.iter().map(|i| i.report.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(with_items, with_reports);
}

#[test]
fn diff_eval_items_pairs_by_fixture_id_and_cell() {
    // Same fixture id, different model cells — must produce
    // independent entries (one Drift on m1, one Regression on m2).
    let baseline = vec![
        item("alpha", true, Some("m1"), "ok"),
        item("alpha", true, Some("m2"), "ok"),
    ];
    let new = vec![
        item("alpha", true, Some("m1"), "drifted"),
        item("alpha", false, Some("m2"), "ok"),
    ];
    let s = diff_eval_items(&baseline, &new);
    assert_eq!(s.entries.len(), 2);
    // alpha+m1 → Drift, alpha+m2 → Regression
    let m1 = s
        .entries
        .iter()
        .find(|e| e.cell().and_then(|c| c.model_id.as_deref()) == Some("m1"))
        .unwrap();
    let m2 = s
        .entries
        .iter()
        .find(|e| e.cell().and_then(|c| c.model_id.as_deref()) == Some("m2"))
        .unwrap();
    assert!(matches!(m1, DiffEntry::Drift { .. }));
    assert!(matches!(m2, DiffEntry::Regression { .. }));
}

#[test]
fn diff_eval_items_keys_by_sample_index_so_samples_dont_collide() {
    // 3 samples of the same (fixture, cell) must produce 3 independent
    // diff entries, not silently collapse into one. The bug this guards
    // against: BTreeMap::collect would otherwise keep the last item per
    // (fixture_id, cell) key and lose the others.
    let baseline = vec![
        sampled_item("alpha", true, Some("m1"), 0, "ok"),
        sampled_item("alpha", true, Some("m1"), 1, "ok"),
        sampled_item("alpha", true, Some("m1"), 2, "ok"),
    ];
    let new = vec![
        sampled_item("alpha", true, Some("m1"), 0, "ok"),
        sampled_item("alpha", false, Some("m1"), 1, "broken"),
        sampled_item("alpha", true, Some("m1"), 2, "ok"),
    ];
    let s = diff_eval_items(&baseline, &new);
    assert_eq!(s.entries.len(), 3, "one entry per sample");
    let sample1 = s.entries.iter().find(|e| e.sample_index() == Some(1));
    assert!(matches!(sample1, Some(DiffEntry::Regression { .. })));
    // The other two samples must show Unchanged, not collapse.
    let unchanged_count = s
        .entries
        .iter()
        .filter(|e| matches!(e, DiffEntry::Unchanged { .. }))
        .count();
    assert_eq!(unchanged_count, 2);
}

#[test]
fn diff_eval_items_different_samples_count_as_added_missing() {
    // Baseline ran samples=1 (sample_index = None); new ran samples=3
    // (sample_index Some(0,1,2)). The diff treats None vs Some as
    // different keys — surfaces "you changed run config" instead of
    // silently averaging.
    let baseline = vec![item("alpha", true, Some("m1"), "ok")];
    let new = vec![
        sampled_item("alpha", true, Some("m1"), 0, "ok"),
        sampled_item("alpha", true, Some("m1"), 1, "ok"),
    ];
    let s = diff_eval_items(&baseline, &new);
    assert_eq!(s.entries.len(), 3);
    let missing = s
        .entries
        .iter()
        .filter(|e| matches!(e, DiffEntry::MissingFromNew { .. }))
        .count();
    let added = s
        .entries
        .iter()
        .filter(|e| matches!(e, DiffEntry::NewlyAdded { .. }))
        .count();
    assert_eq!(missing, 1, "the unsampled baseline becomes MissingFromNew");
    assert_eq!(added, 2, "both new samples are NewlyAdded");
}

#[test]
fn diff_eval_items_different_models_count_as_added_missing() {
    // Baseline cell exists only for m1; new cell exists only for
    // m2. They share fixture_id but the model axis is part of the
    // key — m1 is Missing, m2 is NewlyAdded.
    let baseline = vec![item("alpha", true, Some("m1"), "ok")];
    let new = vec![item("alpha", true, Some("m2"), "ok")];
    let s = diff_eval_items(&baseline, &new);
    assert_eq!(s.entries.len(), 2);
    let m1 = s
        .entries
        .iter()
        .find(|e| e.cell().and_then(|c| c.model_id.as_deref()) == Some("m1"))
        .unwrap();
    let m2 = s
        .entries
        .iter()
        .find(|e| e.cell().and_then(|c| c.model_id.as_deref()) == Some("m2"))
        .unwrap();
    assert!(matches!(m1, DiffEntry::MissingFromNew { .. }));
    assert!(matches!(m2, DiffEntry::NewlyAdded { .. }));
}

#[test]
fn diff_entry_serde_round_trips_cell_field() {
    let entry = DiffEntry::Regression {
        fixture_id: "alpha".into(),
        new_failures: vec!["foo".into()],
        cell: Some(MatrixCell {
            model_id: Some("claude-opus-4-7".into()),
        }),
        sample_index: None,
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(
        json.contains("\"cell\""),
        "cell must serialise when set: {json}"
    );
    let parsed: DiffEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn diff_entry_serde_omits_cell_when_none() {
    // skip_serializing_if keeps legacy JSON shape exactly the same
    // as before the matrix cell was added.
    let entry = DiffEntry::Unchanged {
        fixture_id: "x".into(),
        cell: None,
        sample_index: None,
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(!json.contains("cell"));
}
