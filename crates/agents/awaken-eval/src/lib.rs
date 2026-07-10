//! Fixture-driven evaluation harness (#4).
//!
//! A [`Case`] records an agent's config, its input, the model responses to replay,
//! and the [`Expectation`]s to score. [`replay`] feeds the recorded responses back
//! through the REAL runtime via the `RunExecutor` port and scores the committed
//! output — the harness owns no execution logic, so a scored case exercises the
//! true engine path (delegation/permission/commit and all). Datasets persist as
//! JSON through [`store`]. `Purpose::EvalRecording` is the consent purpose a
//! record-from-real-run path attributes captures to (see `awaken-data-subject`).

pub mod record;
pub mod replay;
pub mod store;

use serde::{Deserialize, Serialize};

/// One recorded model turn: either assistant text, or one/more tool calls the
/// model made. A turn with `tool_calls` drives the engine's tool loop (the eval
/// registers a fixed echo executor for each referenced tool id); an empty
/// `tool_calls` is a plain text turn that ends the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptedTurn {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ScriptedToolCall>,
}

/// One scripted tool call: the tool id the model invoked and its arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedToolCall {
    pub tool_id: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
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
    /// The final assistant text does NOT contain `substring`.
    OutputNotContains { substring: String },
    /// The final assistant text equals `text` after trimming.
    OutputEquals { text: String },
    /// The run executed a tool with this id at least once.
    ToolCalled { tool_id: String },
    /// An LLM judge scores the output against `rubric` (0–100); passes when the
    /// score is at least `min_score`. Scored by [`replay::Evaluator`] with an
    /// injected judge model — the pure [`score_case`] cannot run it, so without a
    /// judge it fails with a "no judge configured" detail.
    JudgeScore { rubric: String, min_score: u8 },
    /// The run ended naturally (not an error or cancel).
    Succeeded,
}

impl Expectation {
    /// The stable snake_case kind, recorded on the result.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Expectation::OutputContains { .. } => "output_contains",
            Expectation::OutputNotContains { .. } => "output_not_contains",
            Expectation::OutputEquals { .. } => "output_equals",
            Expectation::ToolCalled { .. } => "tool_called",
            Expectation::JudgeScore { .. } => "judge_score",
            Expectation::Succeeded => "succeeded",
        }
    }

    /// The `(rubric, min_score)` when this is a [`JudgeScore`](Self::JudgeScore),
    /// so the async evaluator can run the judge for it. `None` otherwise.
    #[must_use]
    pub fn judge_spec(&self) -> Option<(&str, u8)> {
        match self {
            Expectation::JudgeScore { rubric, min_score } => Some((rubric, *min_score)),
            _ => None,
        }
    }

    /// Score this expectation against the replayed outcome (`output` text, whether
    /// the run `succeeded`, and the ids of tools it actually called).
    fn evaluate(
        &self,
        output: &str,
        succeeded: bool,
        tools_called: &[String],
    ) -> ExpectationResult {
        let (passed, detail) = match self {
            Expectation::OutputContains { substring } => (
                output.contains(substring),
                format!("expected output to contain {substring:?}"),
            ),
            Expectation::OutputNotContains { substring } => (
                !output.contains(substring),
                format!("expected output NOT to contain {substring:?}"),
            ),
            Expectation::OutputEquals { text } => (
                output.trim() == text.trim(),
                format!("expected output to equal {text:?}"),
            ),
            Expectation::ToolCalled { tool_id } => (
                tools_called.iter().any(|t| t == tool_id),
                format!("expected tool {tool_id:?} to be called (called: {tools_called:?})"),
            ),
            // The pure path cannot run an LLM judge; `Evaluator` overrides this
            // result when a judge is configured, else it stands as a failure.
            Expectation::JudgeScore { min_score, .. } => (
                false,
                format!("judge not run (no judge configured; needed >= {min_score})"),
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

/// Score a case's `expectations` against its replayed `output`, terminal state,
/// and the ids of the tools it called.
#[must_use]
pub fn score_case(
    case: &Case,
    output: &str,
    succeeded: bool,
    tools_called: &[String],
) -> CaseScore {
    CaseScore {
        case_id: case.id.clone(),
        results: case
            .expectations
            .iter()
            .map(|e| e.evaluate(output, succeeded, tools_called))
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
                ..Default::default()
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
        let score = score_case(&c, "the answer is 42", true, &[]);
        assert!(score.passed());
    }

    #[test]
    fn a_missing_substring_fails_with_detail() {
        let c = case(vec![Expectation::OutputContains {
            substring: "99".into(),
        }]);
        let score = score_case(&c, "the answer is 42", true, &[]);
        assert!(!score.passed());
        assert!(score.results[0].detail.contains("99"));
    }

    #[test]
    fn a_failed_run_fails_the_succeeded_expectation() {
        let c = case(vec![Expectation::Succeeded]);
        let score = score_case(&c, "", false, &[]);
        assert!(!score.passed());
    }

    #[test]
    fn tool_called_and_not_contains_score_against_the_run_facts() {
        let c = case(vec![
            Expectation::ToolCalled {
                tool_id: "search".into(),
            },
            Expectation::OutputNotContains {
                substring: "error".into(),
            },
        ]);
        // `search` was called and the output has no "error" → both pass.
        let pass = score_case(&c, "the answer is 42", true, &["search".to_string()]);
        assert!(pass.passed());
        // `search` NOT called → the ToolCalled expectation fails with detail.
        let fail = score_case(&c, "the answer is 42", true, &[]);
        assert!(!fail.passed());
        assert!(fail.results[0].detail.contains("search"));
    }

    #[test]
    fn report_counts_and_all_passed() {
        let report = Report {
            dataset: "d".into(),
            scores: vec![
                score_case(&case(vec![Expectation::Succeeded]), "x", true, &[]),
                score_case(&case(vec![Expectation::Succeeded]), "x", false, &[]),
            ],
        };
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 1);
        assert!(!report.all_passed());
    }
}
