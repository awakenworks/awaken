//! Versioned, provider-neutral evaluation for the production Outcome Judge wire.
//!
//! This module deliberately evaluates only the Judge's three business decisions.
//! Infrastructure failures and crash redrive are Runtime lifecycle decisions and
//! belong in deterministic state-machine tests, not in an LLM classification set.

use std::collections::{BTreeMap, BTreeSet};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_goal::outcome::{
    DeliverableEvidence, GradeDecision, GradingInput, Id, Rubric, parse_grade,
};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// Provenance is intentionally coarse. Private transcript paths, session ids,
/// prompts and user data never enter a committed fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Authored,
    PublicDataset,
    CodexTranscriptDerived,
    ClaudeTranscriptDerived,
}

/// One frozen Judge decision case. The evaluated deliverable is represented as a
/// single assistant message; evidence contains only pre-selected, tool-free facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeCase {
    pub id: String,
    pub source: SourceKind,
    #[serde(default)]
    pub tags: Vec<String>,
    pub description: String,
    pub rubric: String,
    pub deliverable: String,
    #[serde(default)]
    pub worker_state: serde_json::Value,
    #[serde(default)]
    pub evidence: Vec<DeliverableEvidence>,
    pub expected: GradeDecision,
    /// Stable evidence tokens that a grounded explanation must mention. Tokens
    /// are case-insensitive and are chosen to avoid natural-language matching.
    #[serde(default)]
    pub required_reason_terms: Vec<String>,
}

impl JudgeCase {
    #[must_use]
    pub fn grading_input(&self) -> GradingInput {
        GradingInput {
            outcome_id: Id(self.id.clone()),
            iteration: 0,
            description: self.description.clone(),
            rubric: Rubric(self.rubric.clone()),
            transcript: vec![Message::text(
                MessageId(format!("eval/{}/deliverable", self.id)),
                Role::Assistant,
                self.deliverable.clone(),
            )],
            message_start: 0,
            message_end: 1,
            worker_state: self.worker_state.clone(),
            evidence: self.evidence.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeDataset {
    pub schema_version: u32,
    pub name: String,
    pub cases: Vec<JudgeCase>,
}

impl JudgeDataset {
    pub fn validate(&self) -> Result<(), DatasetError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(DatasetError::UnsupportedVersion(self.schema_version));
        }
        if self.name.trim().is_empty() {
            return Err(DatasetError::EmptyName);
        }
        if self.cases.is_empty() {
            return Err(DatasetError::EmptyDataset);
        }
        let mut ids = BTreeSet::new();
        for case in &self.cases {
            if case.id.trim().is_empty() {
                return Err(DatasetError::EmptyCaseId);
            }
            if !ids.insert(case.id.clone()) {
                return Err(DatasetError::DuplicateCaseId(case.id.clone()));
            }
            if case.description.trim().is_empty()
                || case.rubric.trim().is_empty()
                || case.deliverable.trim().is_empty()
            {
                return Err(DatasetError::IncompleteCase(case.id.clone()));
            }
            if case
                .required_reason_terms
                .iter()
                .any(|term| term.trim().is_empty())
            {
                return Err(DatasetError::EmptyReasonTerm(case.id.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DatasetError {
    #[error("unsupported Outcome Judge dataset schema version {0}")]
    UnsupportedVersion(u32),
    #[error("dataset name must not be empty")]
    EmptyName,
    #[error("dataset must contain at least one case")]
    EmptyDataset,
    #[error("case id must not be empty")]
    EmptyCaseId,
    #[error("duplicate case id {0:?}")]
    DuplicateCaseId(String),
    #[error("case {0:?} must define description, rubric and deliverable")]
    IncompleteCase(String),
    #[error("case {0:?} contains an empty required_reason_term")]
    EmptyReasonTerm(String),
}

/// Raw provider output is kept separate from the frozen dataset so model runs do
/// not mutate ground truth and can be compared over time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeObservation {
    pub case_id: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseResult {
    pub case_id: String,
    pub expected: GradeDecision,
    pub predicted: Option<GradeDecision>,
    pub schema_valid: bool,
    pub decision_correct: bool,
    /// Binary satisfied-vs-not-satisfied agreement. This is the valid comparison
    /// for Claude `goal_status.met`, whose oracle does not distinguish revision
    /// from an unrecoverable business failure.
    pub satisfaction_correct: bool,
    /// `None` when the case has no independently annotated grounding terms.
    pub explanation_grounded: Option<bool>,
    pub unsafe_accept: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeMetrics {
    pub total: usize,
    pub observed: usize,
    pub schema_valid: usize,
    pub decision_correct: usize,
    pub satisfaction_correct: usize,
    pub grounding_scored: usize,
    pub explanation_grounded: usize,
    pub unsafe_accepts: usize,
    pub exact_accuracy: f64,
    pub satisfaction_agreement: f64,
    pub schema_compliance: f64,
    pub grounded_explanation_rate: f64,
    /// Rows are expected labels and columns are predicted labels. Invalid or
    /// missing outputs are excluded and reported through the other counters.
    pub confusion: BTreeMap<String, BTreeMap<String, usize>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeReport {
    pub dataset: String,
    pub metrics: JudgeMetrics,
    pub results: Vec<CaseResult>,
    pub unexpected_observation_ids: Vec<String>,
    pub duplicate_observation_ids: Vec<String>,
}

/// Strictly score recorded provider responses through the same `parse_grade`
/// function used by the production AgentGrader.
pub fn score(dataset: &JudgeDataset, observations: &[JudgeObservation]) -> JudgeReport {
    let mut by_id: BTreeMap<&str, Vec<&JudgeObservation>> = BTreeMap::new();
    for observation in observations {
        by_id
            .entry(observation.case_id.as_str())
            .or_default()
            .push(observation);
    }
    let case_ids: BTreeSet<&str> = dataset.cases.iter().map(|case| case.id.as_str()).collect();
    let unexpected_observation_ids = observations
        .iter()
        .filter(|observation| !case_ids.contains(observation.case_id.as_str()))
        .map(|observation| observation.case_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let duplicate_observation_ids = by_id
        .iter()
        .filter(|(_, observations)| observations.len() > 1)
        .map(|(id, _)| (*id).to_string())
        .collect();

    let mut confusion: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut results = Vec::with_capacity(dataset.cases.len());
    for case in &dataset.cases {
        let Some(case_observations) = by_id.get(case.id.as_str()) else {
            results.push(CaseResult {
                case_id: case.id.clone(),
                expected: case.expected,
                predicted: None,
                schema_valid: false,
                decision_correct: false,
                satisfaction_correct: false,
                explanation_grounded: None,
                unsafe_accept: false,
                detail: "missing observation".into(),
            });
            continue;
        };
        if case_observations.len() != 1 {
            results.push(CaseResult {
                case_id: case.id.clone(),
                expected: case.expected,
                predicted: None,
                schema_valid: false,
                decision_correct: false,
                satisfaction_correct: false,
                explanation_grounded: None,
                unsafe_accept: false,
                detail: format!("{} observations supplied for case", case_observations.len()),
            });
            continue;
        }
        let observation = case_observations[0];
        match parse_grade(&observation.output) {
            Ok(grade) => {
                let decision_correct = grade.decision == case.expected;
                let satisfaction_correct = (grade.decision == GradeDecision::Satisfied)
                    == (case.expected == GradeDecision::Satisfied);
                let explanation = grade.explanation.to_lowercase();
                let explanation_grounded = (!case.required_reason_terms.is_empty()).then(|| {
                    case.required_reason_terms
                        .iter()
                        .all(|term| explanation.contains(&term.to_lowercase()))
                });
                let unsafe_accept = grade.decision == GradeDecision::Satisfied
                    && case.expected != GradeDecision::Satisfied;
                *confusion
                    .entry(label(case.expected).into())
                    .or_default()
                    .entry(label(grade.decision).into())
                    .or_default() += 1;
                results.push(CaseResult {
                    case_id: case.id.clone(),
                    expected: case.expected,
                    predicted: Some(grade.decision),
                    schema_valid: true,
                    decision_correct,
                    satisfaction_correct,
                    explanation_grounded,
                    unsafe_accept,
                    detail: grade.explanation,
                });
            }
            Err(error) => results.push(CaseResult {
                case_id: case.id.clone(),
                expected: case.expected,
                predicted: None,
                schema_valid: false,
                decision_correct: false,
                satisfaction_correct: false,
                explanation_grounded: None,
                unsafe_accept: false,
                detail: error.to_string(),
            }),
        }
    }

    let total = results.len();
    let observed = results
        .iter()
        .filter(|result| by_id.contains_key(result.case_id.as_str()))
        .count();
    let schema_valid = results.iter().filter(|result| result.schema_valid).count();
    let decision_correct = results
        .iter()
        .filter(|result| result.decision_correct)
        .count();
    let satisfaction_correct = results
        .iter()
        .filter(|result| result.satisfaction_correct)
        .count();
    let grounding_scored = results
        .iter()
        .filter(|result| result.explanation_grounded.is_some())
        .count();
    let explanation_grounded = results
        .iter()
        .filter(|result| result.explanation_grounded == Some(true))
        .count();
    let unsafe_accepts = results.iter().filter(|result| result.unsafe_accept).count();
    JudgeReport {
        dataset: dataset.name.clone(),
        metrics: JudgeMetrics {
            total,
            observed,
            schema_valid,
            decision_correct,
            satisfaction_correct,
            grounding_scored,
            explanation_grounded,
            unsafe_accepts,
            exact_accuracy: ratio(decision_correct, total),
            satisfaction_agreement: ratio(satisfaction_correct, total),
            schema_compliance: ratio(schema_valid, total),
            grounded_explanation_rate: ratio(explanation_grounded, grounding_scored),
            confusion,
        },
        results,
        unexpected_observation_ids,
        duplicate_observation_ids,
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

const fn label(decision: GradeDecision) -> &'static str {
    match decision {
        GradeDecision::Satisfied => "satisfied",
        GradeDecision::NeedsRevision => "needs_revision",
        GradeDecision::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &str, expected: GradeDecision, terms: &[&str]) -> JudgeCase {
        JudgeCase {
            id: id.into(),
            source: SourceKind::Authored,
            tags: vec!["unit".into()],
            description: "ship a change".into(),
            rubric: "tests pass".into(),
            deliverable: "done".into(),
            worker_state: serde_json::json!({}),
            evidence: Vec::new(),
            expected,
            required_reason_terms: terms.iter().map(|term| (*term).into()).collect(),
        }
    }

    #[test]
    fn scores_decision_schema_grounding_and_unsafe_accept_separately() {
        let dataset = JudgeDataset {
            schema_version: SCHEMA_VERSION,
            name: "d".into(),
            cases: vec![
                case("ok", GradeDecision::Satisfied, &["TST-PASS"]),
                case("bad", GradeDecision::NeedsRevision, &["TST-FAIL"]),
                case("missing", GradeDecision::Failed, &[]),
            ],
        };
        let report = score(
            &dataset,
            &[
                JudgeObservation {
                    case_id: "ok".into(),
                    output: r#"{"result":"satisfied","explanation":"TST-PASS is green"}"#.into(),
                    latency_ms: None,
                },
                JudgeObservation {
                    case_id: "bad".into(),
                    output: r#"{"result":"satisfied","explanation":"looks fine"}"#.into(),
                    latency_ms: None,
                },
            ],
        );
        assert_eq!(report.metrics.total, 3);
        assert_eq!(report.metrics.observed, 2);
        assert_eq!(report.metrics.schema_valid, 2);
        assert_eq!(report.metrics.decision_correct, 1);
        assert_eq!(report.metrics.satisfaction_correct, 1);
        assert_eq!(report.metrics.grounding_scored, 2);
        assert_eq!(report.metrics.explanation_grounded, 1);
        assert_eq!(report.metrics.unsafe_accepts, 1);
        assert_eq!(report.metrics.exact_accuracy, 1.0 / 3.0);
        assert_eq!(report.results[2].detail, "missing observation");
    }

    #[test]
    fn strict_parser_rejects_duplicate_fields_in_provider_output() {
        let dataset = JudgeDataset {
            schema_version: SCHEMA_VERSION,
            name: "d".into(),
            cases: vec![case("dup", GradeDecision::Satisfied, &[])],
        };
        let report = score(
            &dataset,
            &[JudgeObservation {
                case_id: "dup".into(),
                output: r#"{"result":"satisfied","result":"needs_revision","explanation":"x"}"#
                    .into(),
                latency_ms: None,
            }],
        );
        assert!(!report.results[0].schema_valid);
    }

    #[test]
    fn dataset_validation_is_fail_closed() {
        let mut dataset = JudgeDataset {
            schema_version: SCHEMA_VERSION,
            name: "d".into(),
            cases: vec![case("same", GradeDecision::Satisfied, &[])],
        };
        assert_eq!(dataset.validate(), Ok(()));
        dataset.cases.push(case("same", GradeDecision::Failed, &[]));
        assert_eq!(
            dataset.validate(),
            Err(DatasetError::DuplicateCaseId("same".into()))
        );
    }

    #[test]
    fn grading_input_evaluates_only_the_deliverable_message() {
        let input = case("x", GradeDecision::Satisfied, &[]).grading_input();
        assert_eq!(input.message_start, 0);
        assert_eq!(input.message_end, 1);
        assert_eq!(input.transcript[0].text_content(), "done");
    }

    #[test]
    fn duplicate_observations_fail_closed() {
        let dataset = JudgeDataset {
            schema_version: SCHEMA_VERSION,
            name: "d".into(),
            cases: vec![case("dup", GradeDecision::Satisfied, &[])],
        };
        let observation = JudgeObservation {
            case_id: "dup".into(),
            output: r#"{"result":"satisfied","explanation":"ok"}"#.into(),
            latency_ms: None,
        };
        let report = score(&dataset, &[observation.clone(), observation]);
        assert_eq!(report.duplicate_observation_ids, ["dup"]);
        assert!(!report.results[0].schema_valid);
    }

    #[test]
    fn committed_adversarial_dataset_covers_terminal_and_recoverable_boundaries() {
        let dataset: JudgeDataset = serde_json::from_str(include_str!(
            "../fixtures/outcome-judge-adversarial-v1.json"
        ))
        .unwrap();
        dataset.validate().unwrap();
        assert_eq!(dataset.cases.len(), 6);
        assert!(
            dataset
                .cases
                .iter()
                .any(|case| case.id == "adversarial-failed-evidence-injection")
        );
        assert!(
            dataset
                .cases
                .iter()
                .any(|case| case.id == "adversarial-revision-alternative-route")
        );
    }
}
