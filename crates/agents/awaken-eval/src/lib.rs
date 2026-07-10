//! Fixture-driven evaluation harness (#4).
//!
//! A [`Case`] records an agent's config, its input, the model responses to replay,
//! and the [`Expectation`]s to score. [`replay`] feeds the recorded responses back
//! through the REAL runtime via the `RunExecutor` port and scores the committed
//! output — the harness owns no execution logic, so a scored case exercises the
//! true engine path (delegation/permission/commit and all). Datasets persist as
//! JSON through [`store`]. `Purpose::EvalRecording` is the consent purpose a
//! record-from-real-run path attributes captures to (see `awaken-data-subject`).

pub mod replay;
pub mod store;

use serde::{Deserialize, Serialize};

/// One recorded model turn: the assistant text the model returned. (Tool-call
/// scripting is a follow-up; text turns cover the common judged case.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedTurn {
    pub text: String,
}

/// A single evaluation case: agent config + input + recorded model responses to
/// replay + the expectations to score against the committed outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    /// The agent's system instructions (may be empty).
    #[serde(default)]
    pub instructions: String,
    /// The user input that starts the run.
    pub input: String,
    /// The recorded model responses, replayed in order.
    pub script: Vec<ScriptedTurn>,
    /// What the committed outcome must satisfy.
    pub expectations: Vec<Expectation>,
}

/// A named set of cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    pub name: String,
    pub cases: Vec<Case>,
}

/// A typed check against a replayed case's outcome. A small closed set — extended
/// deliberately, never a free-form DSL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expectation {
    /// The final assistant text contains `substring`.
    OutputContains { substring: String },
    /// The final assistant text equals `text` after trimming.
    OutputEquals { text: String },
    /// The run ended naturally (not an error or cancel).
    Succeeded,
}

impl Expectation {
    /// The stable snake_case kind, recorded on the result.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Expectation::OutputContains { .. } => "output_contains",
            Expectation::OutputEquals { .. } => "output_equals",
            Expectation::Succeeded => "succeeded",
        }
    }

    /// Score this expectation against the replayed outcome.
    fn evaluate(&self, output: &str, succeeded: bool) -> ExpectationResult {
        let (passed, detail) = match self {
            Expectation::OutputContains { substring } => (
                output.contains(substring),
                format!("expected output to contain {substring:?}"),
            ),
            Expectation::OutputEquals { text } => (
                output.trim() == text.trim(),
                format!("expected output to equal {text:?}"),
            ),
            Expectation::Succeeded => (succeeded, "expected the run to end naturally".to_string()),
        };
        ExpectationResult {
            expectation_kind: self.kind().to_string(),
            passed,
            detail: if passed { "ok".to_string() } else { detail },
        }
    }
}

/// The outcome of one expectation against one case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpectationResult {
    pub expectation_kind: String,
    pub passed: bool,
    pub detail: String,
}

/// Every expectation's result for one case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseScore {
    pub case_id: String,
    pub results: Vec<ExpectationResult>,
}

impl CaseScore {
    /// The case passes when every expectation passes.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }
}

/// The scored report for a whole dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub dataset: String,
    pub scores: Vec<CaseScore>,
}

impl Report {
    /// How many cases passed every expectation.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.scores.iter().filter(|s| s.passed()).count()
    }

    /// Total cases scored.
    #[must_use]
    pub fn total(&self) -> usize {
        self.scores.len()
    }

    /// Whether every case passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.scores.iter().all(CaseScore::passed)
    }
}

/// Score a case's `expectations` against its replayed `output` and terminal state.
#[must_use]
pub fn score_case(case: &Case, output: &str, succeeded: bool) -> CaseScore {
    CaseScore {
        case_id: case.id.clone(),
        results: case
            .expectations
            .iter()
            .map(|e| e.evaluate(output, succeeded))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(exps: Vec<Expectation>) -> Case {
        Case {
            id: "c1".into(),
            instructions: String::new(),
            input: "hi".into(),
            script: vec![ScriptedTurn {
                text: "the answer is 42".into(),
            }],
            expectations: exps,
        }
    }

    #[test]
    fn contains_and_succeeded_pass_on_a_matching_output() {
        let c = case(vec![
            Expectation::OutputContains {
                substring: "42".into(),
            },
            Expectation::Succeeded,
        ]);
        let score = score_case(&c, "the answer is 42", true);
        assert!(score.passed());
    }

    #[test]
    fn a_missing_substring_fails_with_detail() {
        let c = case(vec![Expectation::OutputContains {
            substring: "99".into(),
        }]);
        let score = score_case(&c, "the answer is 42", true);
        assert!(!score.passed());
        assert!(score.results[0].detail.contains("99"));
    }

    #[test]
    fn a_failed_run_fails_the_succeeded_expectation() {
        let c = case(vec![Expectation::Succeeded]);
        let score = score_case(&c, "", false);
        assert!(!score.passed());
    }

    #[test]
    fn report_counts_and_all_passed() {
        let report = Report {
            dataset: "d".into(),
            scores: vec![
                score_case(&case(vec![Expectation::Succeeded]), "x", true),
                score_case(&case(vec![Expectation::Succeeded]), "x", false),
            ],
        };
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 1);
        assert!(!report.all_passed());
    }
}
