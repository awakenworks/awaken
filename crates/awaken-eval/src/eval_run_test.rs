use super::*;
use crate::expectation::Failure;
use crate::outcome::ReplayReport;

fn sample_report(id: &str, passed: bool) -> ReplayReport {
    ReplayReport {
        fixture_id: id.into(),
        passed,
        failures: if passed {
            Vec::new()
        } else {
            vec![Failure::AnswerMissingPhrase {
                phrase: "answer".into(),
            }]
        },
        final_text: "ok".into(),
        inference_count: 1,
        tool_count: 0,
        tool_failures: 0,
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_tokens: 15,
        session_duration_ms: 1,
        elapsed_ms: 0,
        tool_calls_by_agent: Vec::new(),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
        cost_usd: None,
    }
}

fn sample_run(id: &str, dataset: &str, started: u64) -> EvalRun {
    EvalRun {
        id: id.into(),
        dataset_id: dataset.into(),
        dataset_revision: 1,
        items: vec![
            EvalRunItem {
                fixture_id: "alpha".into(),
                cell: None,
                report: sample_report("alpha", true),
                trace_run_id: Some("trace-alpha".into()),
                sample_index: None,
            },
            EvalRunItem {
                fixture_id: "beta".into(),
                cell: None,
                report: sample_report("beta", false),
                trace_run_id: None,
                sample_index: None,
            },
        ],
        started_at_secs: started,
        ended_at_secs: started + 5,
    }
}

#[test]
fn summary_counts_passed_items() {
    // EvalRunSummary derivation pre-aggregates pass counts so the list
    // endpoint doesn't have to walk every item server-side. Drift here
    // would silently misreport the green/red split on the admin UI.
    let run = sample_run("RUN1", "DS1", 1_700_000_000);
    let summary = EvalRunSummary::from(&run);
    assert_eq!(summary.item_count, 2);
    assert_eq!(summary.passed_count, 1);
    assert_eq!(summary.dataset_id, "DS1");
    assert_eq!(summary.dataset_revision, 1);
}

#[test]
fn file_store_round_trips_run_and_locates_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let run = sample_run("RUN42", "DS1", 1_700_000_000);
    store.write(&run).unwrap();
    let read = store.read("RUN42").unwrap();
    assert_eq!(read, run);
}

#[test]
fn file_store_list_filters_by_dataset_and_sorts_newest_first() {
    // Two datasets, two runs each. The list filter must return only
    // the matching dataset's runs, newest-first. Drift on either axis
    // breaks the admin UI's "recent runs for dataset X" pane.
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    store
        .write(&sample_run("A1", "DS1", 1_700_000_000))
        .unwrap();
    store
        .write(&sample_run("A2", "DS1", 1_700_001_000))
        .unwrap();
    store
        .write(&sample_run("B1", "DS2", 1_700_000_500))
        .unwrap();

    let filter = EvalRunFilter {
        dataset_id: Some("DS1".into()),
        since_secs: None,
        until_secs: None,
        limit: None,
    };
    let summaries = store.list(&filter).unwrap();
    let ids: Vec<&str> = summaries.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["A2", "A1"]);
}

#[test]
fn file_store_list_limit_truncates_after_sort() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    for i in 0..5 {
        let id = format!("R{i}");
        store
            .write(&sample_run(&id, "DS1", 1_700_000_000 + i))
            .unwrap();
    }
    let filter = EvalRunFilter {
        dataset_id: None,
        since_secs: None,
        until_secs: None,
        limit: Some(2),
    };
    let summaries = store.list(&filter).unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, "R4");
    assert_eq!(summaries[1].id, "R3");
}

#[test]
fn file_store_read_returns_not_found_for_missing_id() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let err = store.read("missing").unwrap_err();
    assert!(matches!(err, EvalRunStoreError::NotFound(id) if id == "missing"));
}

#[test]
fn file_store_rejects_invalid_run_id_on_write() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let mut run = sample_run("../escape", "DS1", 1_700_000_000);
    run.id = "../escape".into();
    let err = store.write(&run).unwrap_err();
    assert!(matches!(err, EvalRunStoreError::InvalidRunId(id) if id == "../escape"));
}

#[test]
fn file_store_path_layout_uses_year_month_shard() {
    // 2023-11-15T00:00:00Z = 1_700_006_400 seconds since epoch.
    let tmp = tempfile::tempdir().unwrap();
    let path = run_path_for(tmp.path(), "RUN-LAYOUT", 1_700_006_400);
    assert!(
        path.ends_with("eval_runs/2023-11/RUN-LAYOUT.json"),
        "unexpected layout: {path:?}"
    );
}

#[test]
fn file_store_prune_drops_runs_older_than_cutoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    // Three runs spaced one day apart. Cutoff sits in the middle so
    // exactly the oldest two are reaped.
    let day = 86_400;
    store
        .write(&sample_run("OLD1", "DS1", 1_700_000_000))
        .unwrap();
    store
        .write(&sample_run("OLD2", "DS1", 1_700_000_000 + day))
        .unwrap();
    store
        .write(&sample_run("KEEP", "DS1", 1_700_000_000 + 5 * day))
        .unwrap();

    let cutoff = 1_700_000_000 + 2 * day;
    let removed = store.prune(cutoff).unwrap();
    assert_eq!(removed, 2);

    let surviving = store
        .list(&EvalRunFilter::default())
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect::<Vec<_>>();
    assert_eq!(surviving, vec!["KEEP"]);
}

#[test]
fn file_store_prune_no_op_when_nothing_old_enough() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    store
        .write(&sample_run("RECENT", "DS1", 1_700_000_000))
        .unwrap();
    // Cutoff older than every run — nothing reaped.
    let removed = store.prune(1).unwrap();
    assert_eq!(removed, 0);
    assert_eq!(store.list(&EvalRunFilter::default()).unwrap().len(), 1);
}

#[test]
fn file_store_prune_leaves_corrupt_files_in_place() {
    // A malformed .json file shouldn't be silently deleted — that would
    // hide real corruption. prune must skip it and report it as kept.
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    store
        .write(&sample_run("GOOD", "DS1", 1_700_000_000))
        .unwrap();
    // Plant a garbage file in the shard.
    let shard = tmp.path().join("eval_runs/2023-11");
    std::fs::write(shard.join("CORRUPT.json"), b"not json").unwrap();

    // Cutoff in the future would otherwise reap everything.
    let removed = store.prune(u64::MAX).unwrap();
    assert_eq!(removed, 1, "only the valid run is reaped");
    // The corrupt file remains for operator inspection.
    assert!(shard.join("CORRUPT.json").exists());
}

#[test]
fn mint_run_id_produces_unique_26_char_ulids() {
    // ULID is 26 chars Crockford base32. Uniqueness is the load-bearing
    // property: two consecutive mints must differ even when issued in
    // the same millisecond (the random component disambiguates).
    let a = mint_run_id();
    let b = mint_run_id();
    assert_ne!(a, b);
    assert_eq!(a.len(), 26);
    assert_eq!(b.len(), 26);
}

#[test]
fn eval_run_item_serde_omits_sample_index_when_none() {
    // Back-compat: single-sample runs must produce the same JSON shape
    // they did before the flakiness feature landed — no `sample_index`
    // field. Catches accidental skip_serializing_if removals.
    let item = EvalRunItem {
        fixture_id: "alpha".into(),
        cell: None,
        report: sample_report("alpha", true),
        trace_run_id: None,
        sample_index: None,
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("sample_index"), "json: {json}");
}

#[test]
fn eval_run_item_serde_round_trips_sample_index() {
    let item = EvalRunItem {
        fixture_id: "alpha".into(),
        cell: None,
        report: sample_report("alpha", true),
        trace_run_id: None,
        sample_index: Some(2),
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains(r#""sample_index":2"#));
    let parsed: EvalRunItem = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, item);
}

#[test]
fn eval_run_item_deserialises_legacy_json_without_sample_index() {
    // A pre-flakiness EvalRun JSON on disk must continue to parse.
    let legacy = r#"{
        "fixture_id": "alpha",
        "report": {
            "fixture_id": "alpha",
            "passed": true,
            "failures": [],
            "final_text": "ok",
            "inference_count": 1,
            "tool_count": 0,
            "tool_failures": 0,
            "total_input_tokens": 10,
            "total_output_tokens": 5,
            "total_tokens": 15,
            "session_duration_ms": 1
        }
    }"#;
    let parsed: EvalRunItem = serde_json::from_str(legacy).unwrap();
    assert!(parsed.sample_index.is_none());
    assert!(parsed.cell.is_none());
}

#[test]
fn file_store_list_full_filters_by_since_until() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    for (id, started) in [
        ("EARLY", 1_700_000_000),
        ("MID", 1_700_000_500),
        ("LATE", 1_700_001_000),
    ] {
        store.write(&sample_run(id, "DS", started)).unwrap();
    }
    // since=200, until=800 keeps only MID (EARLY < since; LATE >= until).
    let filter = EvalRunFilter {
        dataset_id: None,
        since_secs: Some(1_700_000_200),
        until_secs: Some(1_700_000_800),
        limit: None,
    };
    let runs = store.list_full(&filter).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "MID");
}

#[test]
fn aggregate_samples_single_sample_degenerates_to_pass_fail() {
    let run = sample_run("R", "DS", 1);
    // sample_run has 2 items: alpha=pass, beta=fail (no cell, no sample_index).
    let aggs = run.aggregate_samples();
    assert_eq!(aggs.len(), 2);
    let alpha = aggs.iter().find(|a| a.fixture_id == "alpha").unwrap();
    assert_eq!(alpha.samples, 1);
    assert_eq!(alpha.passed, 1);
    assert!(alpha.pass_at_k);
    assert!(alpha.pass_pow_k);
    let beta = aggs.iter().find(|a| a.fixture_id == "beta").unwrap();
    assert_eq!(beta.samples, 1);
    assert_eq!(beta.passed, 0);
    assert!(!beta.pass_at_k);
    assert!(!beta.pass_pow_k);
}

#[test]
fn aggregate_samples_groups_by_fixture_and_cell() {
    // 3 items, same fixture, same cell, mixed pass/fail → one group with
    // pass@k=true (at least one passed) and pass^k=false (not all passed).
    let mut run = sample_run("R", "DS", 1);
    run.items.clear();
    for (i, passed) in [(0u32, true), (1u32, false), (2u32, true)] {
        let mut item = EvalRunItem {
            fixture_id: "alpha".into(),
            cell: Some(MatrixCell {
                model_id: Some("m1".into()),
            }),
            report: sample_report("alpha", passed),
            trace_run_id: None,
            sample_index: Some(i),
        };
        item.report.passed = passed;
        run.items.push(item);
    }
    let aggs = run.aggregate_samples();
    assert_eq!(aggs.len(), 1, "all three samples fold into one group");
    let g = &aggs[0];
    assert_eq!(g.samples, 3);
    assert_eq!(g.passed, 2);
    assert!((g.pass_rate - 2.0 / 3.0).abs() < 1e-9);
    assert!(g.pass_at_k, "≥1 passed → pass@k true");
    assert!(!g.pass_pow_k, "not all passed → pass^k false");
    assert_eq!(
        g.cell.as_ref().and_then(|c| c.model_id.as_deref()),
        Some("m1")
    );
}

#[test]
fn aggregate_samples_all_pass_marks_pow_k() {
    let mut run = sample_run("R", "DS", 1);
    run.items.clear();
    for i in 0..3u32 {
        let mut item = EvalRunItem {
            fixture_id: "a".into(),
            cell: None,
            report: sample_report("a", true),
            trace_run_id: None,
            sample_index: Some(i),
        };
        item.report.passed = true;
        run.items.push(item);
    }
    let aggs = run.aggregate_samples();
    assert_eq!(aggs.len(), 1);
    assert!(aggs[0].pass_pow_k);
    assert!(aggs[0].pass_at_k);
}
