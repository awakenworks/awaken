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
        execution_mode: EvalRunExecutionMode::Scripted,
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
    assert_eq!(summary.execution_mode, EvalRunExecutionMode::Scripted);
}

#[test]
fn eval_run_deserialises_legacy_json_without_execution_mode() {
    // Pre-`execution_mode` JSON whose items carry no `cell` was always a
    // scripted run (matrix/live both stamp `cell` on every item). The
    // shadow deserialiser must restore those records as `Scripted` so
    // the field's introduction stays a non-breaking on-disk change.
    let run = sample_run("LEGACY", "DS1", 1_700_000_000);
    let mut value = serde_json::to_value(&run).unwrap();
    value.as_object_mut().unwrap().remove("execution_mode");

    let parsed: EvalRun = serde_json::from_value(value).unwrap();

    assert_eq!(parsed.execution_mode, EvalRunExecutionMode::Scripted);
}

#[test]
fn eval_run_deserialises_legacy_live_matrix_run_as_live() {
    // Regression: live-matrix runs created before `execution_mode` landed
    // have `item.cell` set but no `execution_mode` field. Defaulting them
    // to `Scripted` would make `load_and_validate_baseline` reject every
    // pre-existing live baseline with "cannot diff across execution
    // modes". Infer from items so the field's introduction stays a
    // non-breaking on-disk change for live baselines too.
    let mut run = sample_run("LEGACY-LIVE", "DS1", 1_700_000_000);
    run.execution_mode = EvalRunExecutionMode::Live;
    for item in &mut run.items {
        item.cell = Some(MatrixCell {
            model_id: Some("gpt-x".into()),
        });
    }
    let mut value = serde_json::to_value(&run).unwrap();
    value.as_object_mut().unwrap().remove("execution_mode");

    let parsed: EvalRun = serde_json::from_value(value).unwrap();

    assert_eq!(parsed.execution_mode, EvalRunExecutionMode::Live);
}

#[test]
fn eval_run_rejects_legacy_mixed_cell_presence() {
    let mut run = sample_run("LEGACY-MIXED", "DS1", 1_700_000_000);
    run.items[0].cell = Some(MatrixCell {
        model_id: Some("gpt-x".into()),
    });
    let mut value = serde_json::to_value(&run).unwrap();
    value.as_object_mut().unwrap().remove("execution_mode");

    let err = serde_json::from_value::<EvalRun>(value).unwrap_err();

    assert!(
        err.to_string().contains("mixes items"),
        "unexpected error: {err}"
    );
}

#[test]
fn eval_run_rejects_explicit_live_items_without_cells() {
    let mut run = sample_run("LIVE-MISSING-CELL", "DS1", 1_700_000_000);
    run.execution_mode = EvalRunExecutionMode::Live;
    run.items[0].cell = Some(MatrixCell {
        model_id: Some("gpt-x".into()),
    });
    // item[1] remains cell=None, which is not a valid explicit live run.
    let value = serde_json::to_value(&run).unwrap();

    let err = serde_json::from_value::<EvalRun>(value).unwrap_err();

    assert!(
        err.to_string().contains("execution_mode=live"),
        "unexpected error: {err}"
    );
}

#[test]
fn eval_run_rejects_explicit_scripted_items_with_cells() {
    let mut run = sample_run("SCRIPTED-WITH-CELL", "DS1", 1_700_000_000);
    run.items[0].cell = Some(MatrixCell {
        model_id: Some("gpt-x".into()),
    });
    let value = serde_json::to_value(&run).unwrap();

    let err = serde_json::from_value::<EvalRun>(value).unwrap_err();

    assert!(
        err.to_string().contains("execution_mode=scripted"),
        "unexpected error: {err}"
    );
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
fn file_store_write_is_write_once() {
    // Eval runs are immutable; a second write under the same id must
    // surface AlreadyExists, not silently overwrite the prior bytes.
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let first = sample_run("RUN42", "DS1", 1_700_000_000);
    store.write(&first).unwrap();
    let mut second = first.clone();
    second.dataset_id = "DS2".into();
    let err = store.write(&second).unwrap_err();
    assert!(matches!(
        err,
        EvalRunStoreError::AlreadyExists(ref id) if id == "RUN42"
    ));
    // Persisted bytes still belong to the first writer.
    let read = store.read("RUN42").unwrap();
    assert_eq!(read.dataset_id, "DS1");
}

#[test]
fn file_store_successful_write_removes_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    store
        .write(&sample_run("RUN-ATOMIC", "DS1", 1_700_000_000))
        .unwrap();

    let runs_root = tmp.path().join("eval_runs");
    let mut temp_files = Vec::new();
    for entry in std::fs::read_dir(&runs_root).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() || dir.file_name().and_then(|n| n.to_str()) == Some(".ids") {
            continue;
        }
        for file in std::fs::read_dir(&dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                temp_files.push(path);
            }
        }
    }
    assert!(temp_files.is_empty(), "temp files: {temp_files:?}");
    assert!(runs_root.join(".ids").join("RUN-ATOMIC").exists());
}

#[test]
fn file_store_write_once_is_global_across_started_at_shards() {
    // Write-once is keyed by run_id, not by the derived shard path. Two
    // writers racing with the same id but different started_at months
    // must not both win by landing in different `{yyyy-mm}/` dirs.
    let tmp = tempfile::tempdir().unwrap();
    let store_a = FileEvalRunStore::new(tmp.path()).unwrap();
    let store_b = FileEvalRunStore::new(tmp.path()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let run_a = sample_run("RUN-GLOBAL", "DS1", 1_700_000_000);
    let run_b = sample_run("RUN-GLOBAL", "DS2", 1_720_000_000);

    let barrier_a = barrier.clone();
    let a = std::thread::spawn(move || {
        barrier_a.wait();
        store_a.write(&run_a)
    });
    let barrier_b = barrier;
    let b = std::thread::spawn(move || {
        barrier_b.wait();
        store_b.write(&run_b)
    });

    let results = vec![a.join().unwrap(), b.join().unwrap()];
    let successes = results.iter().filter(|result| result.is_ok()).count();
    let already_exists = results
        .iter()
        .filter(|result| {
            matches!(
                result,
                Err(EvalRunStoreError::AlreadyExists(id)) if id == "RUN-GLOBAL"
            )
        })
        .count();
    assert_eq!(successes, 1, "results: {results:?}");
    assert_eq!(already_exists, 1, "results: {results:?}");

    let runs_root = tmp.path().join("eval_runs");
    let mut data_files = Vec::new();
    for entry in std::fs::read_dir(&runs_root).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() || dir.file_name().and_then(|n| n.to_str()) == Some(".ids") {
            continue;
        }
        let candidate = dir.join("RUN-GLOBAL.json");
        if candidate.exists() {
            data_files.push(candidate);
        }
    }
    assert_eq!(data_files.len(), 1, "data files: {data_files:?}");
    assert!(runs_root.join(".ids").join("RUN-GLOBAL").exists());
}

#[test]
fn file_store_rejects_duplicate_item_keys() {
    // Regression: runs whose items collide on (fixture_id, cell,
    // sample_index) must never land on disk. `diff_eval_items` keys
    // items on that triple and silently overwrites collisions in its
    // BTreeMap, so a duplicate-key run would later make every baseline
    // diff against it depend on Vec insertion order — exactly the
    // "diff is wrong but plausible" failure mode the matrix gate is
    // supposed to catch. The check belongs at the store boundary so
    // dataset matrix runs, ad-hoc online runs, and any future bulk
    // import all share the invariant.
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let mut run = sample_run("RUN-DUP", "DS-DUP", 1_700_000_000);
    // Force a collision: two items with identical
    // (fixture_id, cell, sample_index) — all three are equal here.
    run.items[1].fixture_id = run.items[0].fixture_id.clone();
    let err = store.write(&run).unwrap_err();
    match err {
        EvalRunStoreError::DuplicateItemKeys(id, msg) => {
            assert_eq!(id, "RUN-DUP");
            assert!(
                msg.contains("duplicate eval-run item key"),
                "unexpected duplicate-key message: {msg}",
            );
        }
        other => panic!("expected DuplicateItemKeys, got {other:?}"),
    }
    // Nothing landed on disk — read must surface NotFound.
    let read_err = store.read("RUN-DUP").unwrap_err();
    assert!(matches!(read_err, EvalRunStoreError::NotFound(_)));
}

#[test]
fn file_store_rejects_invalid_execution_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let mut run = sample_run("RUN-BAD-SHAPE", "DS", 1_700_000_000);
    run.execution_mode = EvalRunExecutionMode::Live;
    run.items[0].cell = Some(MatrixCell {
        model_id: Some("m1".into()),
    });
    // item[1] still has no cell, so this is not a valid Live run.

    let err = store.write(&run).unwrap_err();

    assert!(matches!(
        err,
        EvalRunStoreError::InvalidExecutionShape(ref id, _) if id == "RUN-BAD-SHAPE"
    ));
    let read_err = store.read("RUN-BAD-SHAPE").unwrap_err();
    assert!(matches!(read_err, EvalRunStoreError::NotFound(_)));
}

#[test]
fn file_store_write_once_takes_priority_over_duplicate_item_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let first = sample_run("RUN42", "DS1", 1_700_000_000);
    store.write(&first).unwrap();

    let mut duplicate_payload = first.clone();
    duplicate_payload.items[1].fixture_id = duplicate_payload.items[0].fixture_id.clone();
    let err = store.write(&duplicate_payload).unwrap_err();
    assert!(matches!(
        err,
        EvalRunStoreError::AlreadyExists(ref id) if id == "RUN42"
    ));
}

#[test]
fn file_store_read_rejects_historical_duplicate_item_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let mut run = sample_run("RUN-HIST-DUP", "DS-DUP", 1_700_000_000);
    run.items[1].fixture_id = run.items[0].fixture_id.clone();

    let path = run_path_for(tmp.path(), &run.id, run.started_at_secs);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&run).unwrap()).unwrap();

    let err = store.read(&run.id).unwrap_err();
    match err {
        EvalRunStoreError::DuplicateItemKeys(id, msg) => {
            assert_eq!(id, "RUN-HIST-DUP");
            assert!(
                msg.contains("duplicate eval-run item key"),
                "unexpected duplicate-key message: {msg}",
            );
        }
        other => panic!("expected DuplicateItemKeys, got {other:?}"),
    }
}

#[test]
fn file_store_read_rejects_file_name_and_run_id_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let mut dirty = sample_run("RUN-B", "DS-DIRTY", 1_700_000_000);
    dirty.id = "RUN-B".into();

    let path = run_path_for(tmp.path(), "RUN-A", dirty.started_at_secs);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&dirty).unwrap()).unwrap();

    let err = store.read("RUN-A").unwrap_err();
    match err {
        EvalRunStoreError::InvalidRunId(msg) => {
            assert!(msg.contains("RUN-A"), "unexpected message: {msg}");
            assert!(msg.contains("RUN-B"), "unexpected message: {msg}");
        }
        other => panic!("expected InvalidRunId, got {other:?}"),
    }
}

#[test]
fn file_store_list_full_skips_historical_duplicate_item_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    store
        .write(&sample_run("GOOD", "DS-DUP", 1_700_000_500))
        .unwrap();

    let mut dirty = sample_run("DIRTY", "DS-DUP", 1_700_000_000);
    dirty.items[1].fixture_id = dirty.items[0].fixture_id.clone();
    let dirty_path = run_path_for(tmp.path(), &dirty.id, dirty.started_at_secs);
    std::fs::create_dir_all(dirty_path.parent().unwrap()).unwrap();
    std::fs::write(&dirty_path, serde_json::to_vec_pretty(&dirty).unwrap()).unwrap();

    let filter = EvalRunFilter {
        dataset_id: Some("DS-DUP".into()),
        since_secs: None,
        until_secs: None,
        limit: None,
    };
    let runs = store.list_full(&filter).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "GOOD");

    let summaries = store.list(&filter).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "GOOD");
}

#[test]
fn file_store_list_full_skips_file_name_and_run_id_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    store
        .write(&sample_run("GOOD", "DS-DIRTY", 1_700_000_500))
        .unwrap();

    let dirty = sample_run("RUN-B", "DS-DIRTY", 1_700_000_000);
    let dirty_path = run_path_for(tmp.path(), "RUN-A", dirty.started_at_secs);
    std::fs::create_dir_all(dirty_path.parent().unwrap()).unwrap();
    std::fs::write(&dirty_path, serde_json::to_vec_pretty(&dirty).unwrap()).unwrap();

    let runs = store.list_full(&EvalRunFilter::default()).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, "GOOD");
}

#[test]
fn file_store_prune_leaves_file_name_and_run_id_mismatch_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let dirty = sample_run("RUN-B", "DS-DIRTY", 1_700_000_000);
    let dirty_path = run_path_for(tmp.path(), "RUN-A", dirty.started_at_secs);
    std::fs::create_dir_all(dirty_path.parent().unwrap()).unwrap();
    std::fs::write(&dirty_path, serde_json::to_vec_pretty(&dirty).unwrap()).unwrap();
    let ids_dir = tmp.path().join("eval_runs/.ids");
    std::fs::create_dir_all(&ids_dir).unwrap();
    std::fs::write(ids_dir.join("RUN-A"), b"file marker").unwrap();
    std::fs::write(ids_dir.join("RUN-B"), b"json marker").unwrap();

    let removed = store.prune(u64::MAX).unwrap();

    assert_eq!(removed, 0);
    assert!(
        dirty_path.exists(),
        "dirty file must be retained for operator inspection"
    );
    assert!(ids_dir.join("RUN-A").exists());
    assert!(
        ids_dir.join("RUN-B").exists(),
        "prune must not trust dirty JSON id when cleaning markers"
    );
}

#[test]
fn file_store_started_at_zero_is_resolved_in_persisted_record() {
    // Earlier shape resolved started_at_secs only for shard routing, so
    // the persisted JSON still carried a `0` that diverged from its
    // location. Now the resolved value lands in the saved record too.
    let tmp = tempfile::tempdir().unwrap();
    let store = FileEvalRunStore::new(tmp.path()).unwrap();
    let mut run = sample_run("RUN-RESOLVE", "DS1", 0);
    run.ended_at_secs = 0;
    store.write(&run).unwrap();
    let read = store.read("RUN-RESOLVE").unwrap();
    assert!(
        read.started_at_secs > 0,
        "expected resolved started_at, got {}",
        read.started_at_secs
    );
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
