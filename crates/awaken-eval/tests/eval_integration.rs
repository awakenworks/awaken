//! End-to-end integration tests for awaken-eval.
//!
//! These exercise the public API across module boundaries:
//!
//! 1. Load fixtures from disk (`fixture::load_directory`).
//! 2. Replay them through [`MockReplayer`].
//! 3. Score each outcome with [`score`].
//! 4. Write reports as NDJSON.
//! 5. Diff a fresh report against a committed baseline.
//!
//! The bundled `crates/awaken-eval/fixtures` directory is exercised
//! directly to confirm authoring conventions remain valid as the framework
//! evolves.

use std::path::PathBuf;

use awaken_eval::{
    DiffEntry, Fixture, MockReplayer, MockResponse, ReplayReport, diff_against_baseline,
    fixture::load_directory, read_ndjson_path, replay_all, score, write_ndjson_path,
};

fn bundled_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

async fn replay_dir(dir: &PathBuf) -> Vec<ReplayReport> {
    let fixtures = load_directory(dir).expect("fixtures load");
    let outcomes = replay_all(&MockReplayer::new(), &fixtures).await;
    fixtures
        .iter()
        .zip(outcomes.iter())
        .map(|(fx, outcome)| {
            let failures = score(outcome, &fx.expect);
            ReplayReport::from_outcome(outcome, failures)
        })
        .collect()
}

// ── Bundled fixtures sanity ─────────────────────────────────────────

#[tokio::test]
async fn bundled_fixtures_replay_and_pass() {
    let dir = bundled_fixtures_dir();
    let reports = replay_dir(&dir).await;
    assert!(
        !reports.is_empty(),
        "fixtures/ must ship at least one fixture; got {dir:?}"
    );
    for r in &reports {
        assert!(
            r.passed,
            "fixture {} unexpectedly failed: {:?}",
            r.fixture_id, r.failures
        );
    }
}

#[tokio::test]
async fn bundled_fixtures_have_unique_ids() {
    let fixtures = load_directory(bundled_fixtures_dir()).unwrap();
    let mut ids = std::collections::HashSet::new();
    for fx in &fixtures {
        assert!(ids.insert(fx.id.clone()), "duplicate id {}", fx.id);
    }
}

#[tokio::test]
async fn bundled_fixtures_each_have_non_empty_expectation() {
    let fixtures = load_directory(bundled_fixtures_dir()).unwrap();
    for fx in &fixtures {
        assert!(
            !fx.expect.is_empty(),
            "fixture {} has no expectation criteria — at least one is required",
            fx.id
        );
    }
}

// ── Replay → Score → Report → Read round-trip ───────────────────────

#[tokio::test]
async fn full_replay_pipeline_round_trips_through_disk() {
    let dir = temp_dir();
    let report_path = dir.path().join("report.ndjson");

    let mut reports = replay_dir(&bundled_fixtures_dir()).await;
    write_ndjson_path(&report_path, &reports).unwrap();

    let read_back = read_ndjson_path(&report_path).unwrap();
    // `elapsed_ms` is the wall-clock cost of the run — deliberately not
    // serialised (see `ReplayReport::elapsed_ms`), so it deserialises
    // back as 0.
    for r in &mut reports {
        r.elapsed_ms = 0;
    }
    assert_eq!(read_back, reports);
}

// ── Baseline diff: clean → regression → fixed ───────────────────────

#[tokio::test]
async fn diff_baseline_against_itself_is_clean() {
    let reports = replay_dir(&bundled_fixtures_dir()).await;
    let summary = diff_against_baseline(&reports, &reports);
    assert!(summary.is_clean());
    assert_eq!(summary.regressions(), 0);
    for entry in &summary.entries {
        assert!(matches!(entry, DiffEntry::Unchanged { .. }));
    }
}

#[tokio::test]
async fn diff_detects_regression_after_fixture_mutation() {
    let original = replay_dir(&bundled_fixtures_dir()).await;

    // Mutate the bundled "01_simple_qa" fixture in a temp dir so the answer
    // no longer satisfies its expectation.
    let dir = temp_dir();
    let fx_path = dir.path().join("01_simple_qa.json");
    let bundle = bundled_fixtures_dir();
    let original_text = std::fs::read_to_string(bundle.join("01_simple_qa.json")).unwrap();
    let mut fx: Fixture = serde_json::from_str(&original_text).unwrap();
    fx.mock_response = MockResponse::Text {
        text: "I refuse to answer.".into(),
    };
    std::fs::write(&fx_path, serde_json::to_string_pretty(&fx).unwrap()).unwrap();
    // Copy the rest of the fixtures so the run only differs in 01.
    for entry in std::fs::read_dir(&bundle).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy() == "01_simple_qa.json" {
            continue;
        }
        std::fs::copy(entry.path(), dir.path().join(&name)).unwrap();
    }

    let regressed = replay_dir(&dir.path().to_path_buf()).await;
    let summary = diff_against_baseline(&original, &regressed);

    assert!(!summary.is_clean(), "expected regression detection");
    assert_eq!(summary.regressions(), 1);
    let entry = summary
        .entries
        .iter()
        .find(|e| matches!(e, DiffEntry::Regression { fixture_id, .. } if fixture_id == "01_simple_qa"))
        .expect("regression on 01_simple_qa");
    if let DiffEntry::Regression { new_failures, .. } = entry {
        assert!(
            new_failures.iter().any(|k| k == "answer_missing_phrase"),
            "expected answer_missing_phrase, got {new_failures:?}"
        );
    }
}

#[tokio::test]
async fn diff_detects_missing_fixture_when_new_run_is_partial() {
    let original = replay_dir(&bundled_fixtures_dir()).await;

    // Remove one fixture file in a copy directory, then replay.
    let dir = temp_dir();
    for entry in std::fs::read_dir(bundled_fixtures_dir()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy() == "05_error_path.json" {
            continue;
        }
        std::fs::copy(entry.path(), dir.path().join(&name)).unwrap();
    }
    let partial = replay_dir(&dir.path().to_path_buf()).await;
    let summary = diff_against_baseline(&original, &partial);
    assert!(!summary.is_clean());
    assert_eq!(summary.missing(), 1);
}

#[tokio::test]
async fn diff_detects_newly_added_without_blocking() {
    let original = replay_dir(&bundled_fixtures_dir()).await;

    // Add an extra fixture in a copy directory.
    let dir = temp_dir();
    for entry in std::fs::read_dir(bundled_fixtures_dir()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    let extra_path = dir.path().join("99_added.json");
    std::fs::write(
        &extra_path,
        r#"{
            "id": "99_added",
            "user_input": "anything",
            "mock_response": {"kind": "text", "text": "ok"},
            "expect": {"final_answer_contains": ["ok"]}
        }"#,
    )
    .unwrap();
    let extended = replay_dir(&dir.path().to_path_buf()).await;
    let summary = diff_against_baseline(&original, &extended);
    assert_eq!(summary.added(), 1);
    assert!(summary.is_clean(), "newly added fixtures must not block CI");
}
