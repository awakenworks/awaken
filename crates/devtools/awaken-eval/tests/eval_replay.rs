//! End-to-end: a dataset replays through the REAL runtime and scores correctly,
//! and datasets round-trip through the JSON store.

use awaken_eval::store::{load_dataset, save_dataset};
use awaken_eval::{Case, Dataset, Expectation, ScriptedTurn, replay};

fn dataset() -> Dataset {
    Dataset {
        name: "smoke".to_string(),
        cases: vec![
            // Passes: the replayed output contains "42" and the run ends naturally.
            Case {
                id: "answers-42".to_string(),
                instructions: "be terse".to_string(),
                input: "what is the answer?".to_string(),
                script: vec![ScriptedTurn {
                    text: "the answer is 42".to_string(),
                }],
                expectations: vec![
                    Expectation::OutputContains {
                        substring: "42".to_string(),
                    },
                    Expectation::Succeeded,
                ],
            },
            // Fails: the output does not contain "999".
            Case {
                id: "wrong-expectation".to_string(),
                instructions: String::new(),
                input: "hello".to_string(),
                script: vec![ScriptedTurn {
                    text: "hi there".to_string(),
                }],
                expectations: vec![Expectation::OutputContains {
                    substring: "999".to_string(),
                }],
            },
        ],
    }
}

#[tokio::test]
async fn dataset_replays_through_the_real_runtime_and_scores() {
    let report = replay::run_dataset(&dataset()).await;

    assert_eq!(report.total(), 2);
    assert_eq!(report.passed(), 1);
    assert!(!report.all_passed());

    // The passing case matched both expectations; the failing one did not.
    let pass = report
        .scores
        .iter()
        .find(|s| s.case_id == "answers-42")
        .unwrap();
    assert!(pass.passed());
    let fail = report
        .scores
        .iter()
        .find(|s| s.case_id == "wrong-expectation")
        .unwrap();
    assert!(!fail.passed());
    assert!(fail.results[0].detail.contains("999"));
}

#[tokio::test]
async fn a_single_case_replays_the_committed_assistant_text() {
    let case = Case {
        id: "echoes-instructions".to_string(),
        instructions: String::new(),
        input: "go".to_string(),
        script: vec![ScriptedTurn {
            text: "done — result ready".to_string(),
        }],
        expectations: vec![Expectation::OutputEquals {
            text: "done — result ready".to_string(),
        }],
    };
    let score = replay::run_case(&case).await;
    assert!(
        score.passed(),
        "committed text should equal the scripted turn"
    );
}

#[test]
fn datasets_round_trip_through_the_json_store() {
    let dir = std::env::temp_dir().join(format!("awaken-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dataset.json");

    let original = dataset();
    save_dataset(&path, &original).unwrap();
    let loaded = load_dataset(&path).unwrap();

    assert_eq!(loaded.name, original.name);
    assert_eq!(loaded.cases.len(), original.cases.len());
    assert_eq!(loaded.cases[0].id, "answers-42");
    assert_eq!(loaded.cases[0].expectations.len(), 2);

    std::fs::remove_file(&path).ok();
}
