//! MemoryAgentBench import and scoring.
//!
//! This module owns only the public benchmark wire. Product execution remains
//! owned by Session/Run, Memory extraction/recall, context compaction, or Files;
//! callers must report those execution modes separately instead of treating
//! them as interchangeable memory implementations.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::public_benchmark::{BinomialMetrics, binomial};

const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAgentBenchCategory {
    AccurateRetrieval,
    ConflictResolution,
    TestTimeLearning,
    LongRangeUnderstanding,
}

impl MemoryAgentBenchCategory {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "Accurate_Retrieval" | "accurate_retrieval" => Ok(Self::AccurateRetrieval),
            "Conflict_Resolution" | "conflict_resolution" => Ok(Self::ConflictResolution),
            "Test_Time_Learning" | "test_time_learning" => Ok(Self::TestTimeLearning),
            "Long_Range_Understanding" | "long_range_understanding" => {
                Ok(Self::LongRangeUnderstanding)
            }
            _ => Err(format!("unsupported MemoryAgentBench category {value:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAgentBenchMetric {
    SubstringAny,
    ExactMatch,
    RecallAt5,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAgentBenchQuestion {
    pub id: String,
    pub prompt: String,
    pub ground_truths: Vec<String>,
    pub metric: MemoryAgentBenchMetric,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAgentBenchCase {
    pub id: String,
    pub category: MemoryAgentBenchCategory,
    pub source: String,
    pub context: String,
    pub questions: Vec<MemoryAgentBenchQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAgentBenchDataset {
    pub version: u32,
    pub name: String,
    pub cases: Vec<MemoryAgentBenchCase>,
}

impl MemoryAgentBenchDataset {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION || self.name.trim().is_empty() || self.cases.is_empty() {
            return Err("MemoryAgentBench dataset needs version 1, a name, and cases".into());
        }
        let mut case_ids = BTreeSet::new();
        let mut question_ids = BTreeSet::new();
        for case in &self.cases {
            if case.id.trim().is_empty()
                || case.source.trim().is_empty()
                || case.context.trim().is_empty()
                || case.questions.is_empty()
                || !case_ids.insert(&case.id)
            {
                return Err(format!(
                    "invalid or duplicate MemoryAgentBench case {:?}",
                    case.id
                ));
            }
            for question in &case.questions {
                if question.id.trim().is_empty()
                    || question.prompt.trim().is_empty()
                    || question.ground_truths.is_empty()
                    || question
                        .ground_truths
                        .iter()
                        .any(|answer| answer.trim().is_empty())
                    || !question_ids.insert(&question.id)
                {
                    return Err(format!(
                        "invalid or duplicate MemoryAgentBench question {:?}",
                        question.id
                    ));
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn question_count(&self) -> usize {
        self.cases.iter().map(|case| case.questions.len()).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAgentBenchObservation {
    pub question_id: String,
    pub output: String,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAgentBenchQuestionScore {
    pub question_id: String,
    pub category: MemoryAgentBenchCategory,
    pub source: String,
    pub metric: MemoryAgentBenchMetric,
    pub observed: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryAgentBenchReport {
    pub dataset: String,
    pub total: usize,
    pub observed: usize,
    pub provider_errors: usize,
    pub accuracy: BinomialMetrics,
    pub by_category: BTreeMap<MemoryAgentBenchCategory, BinomialMetrics>,
    pub by_source: BTreeMap<String, BinomialMetrics>,
    pub mean_latency_ms: f64,
    pub duplicate_observation_ids: Vec<String>,
    pub unexpected_observation_ids: Vec<String>,
    pub questions: Vec<MemoryAgentBenchQuestionScore>,
}

/// Import one Hugging Face split export. `value` may be a direct row array or
/// one/more datasets-server pages containing `rows[].row`.
pub fn import_rows(
    value: &serde_json::Value,
    category: MemoryAgentBenchCategory,
    limit: usize,
) -> Result<MemoryAgentBenchDataset, String> {
    let rows = exported_rows(value)?;
    let mut cases = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        if limit != 0 && cases.len() >= limit {
            break;
        }
        let context = required_string(row, "context")?;
        let metadata = row.get("metadata").and_then(serde_json::Value::as_object);
        let source = metadata
            .and_then(|metadata| metadata.get("source"))
            .and_then(serde_json::Value::as_str)
            .filter(|source| !source.trim().is_empty())
            .ok_or("MemoryAgentBench row metadata.source is missing")?
            .to_string();
        let prompts = string_list(row.get("questions"), "questions")?;
        let answers = answer_list(row.get("answers"), prompts.len())?;
        if prompts.len() != answers.len() || prompts.is_empty() {
            return Err(
                "MemoryAgentBench questions and answers must be nonempty and aligned".into(),
            );
        }
        let qa_ids = metadata
            .and_then(|metadata| metadata.get("qa_pair_ids"))
            .map(|value| string_list(Some(value), "metadata.qa_pair_ids"))
            .transpose()?
            .unwrap_or_default();
        if !qa_ids.is_empty() && qa_ids.len() != prompts.len() {
            return Err("MemoryAgentBench qa_pair_ids must align with questions".into());
        }
        let case_id = format!("mab-{}-{index}", stable_slug(&source));
        let metric = metric_for(category, &source);
        let questions = prompts
            .into_iter()
            .zip(answers)
            .enumerate()
            .map(
                |(question_index, (prompt, ground_truths))| MemoryAgentBenchQuestion {
                    id: qa_ids
                        .get(question_index)
                        .cloned()
                        .filter(|id| !id.trim().is_empty())
                        .unwrap_or_else(|| format!("{case_id}-q{question_index}")),
                    prompt,
                    ground_truths,
                    metric,
                },
            )
            .collect();
        cases.push(MemoryAgentBenchCase {
            id: case_id,
            category,
            source,
            context,
            questions,
        });
    }
    let dataset = MemoryAgentBenchDataset {
        version: VERSION,
        name: format!("memory-agent-bench-{:?}", category).to_ascii_lowercase(),
        cases,
    };
    dataset.validate()?;
    Ok(dataset)
}

#[must_use]
pub fn score(
    dataset: &MemoryAgentBenchDataset,
    observations: &[MemoryAgentBenchObservation],
) -> MemoryAgentBenchReport {
    let expected = dataset
        .cases
        .iter()
        .flat_map(|case| case.questions.iter().map(|question| question.id.as_str()))
        .collect::<BTreeSet<_>>();
    let mut by_id: BTreeMap<&str, Vec<&MemoryAgentBenchObservation>> = BTreeMap::new();
    let mut unexpected = BTreeSet::new();
    for observation in observations {
        if expected.contains(observation.question_id.as_str()) {
            by_id
                .entry(observation.question_id.as_str())
                .or_default()
                .push(observation);
        } else {
            unexpected.insert(observation.question_id.clone());
        }
    }
    let duplicates = by_id
        .iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(id, _)| (*id).to_string())
        .collect::<BTreeSet<_>>();
    let mut observed = 0;
    let mut errors = 0;
    let mut correct = 0;
    let mut latency = 0_u128;
    let mut category_counts: BTreeMap<MemoryAgentBenchCategory, (usize, usize)> = BTreeMap::new();
    let mut source_counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut questions = Vec::with_capacity(dataset.question_count());
    for case in &dataset.cases {
        for question in &case.questions {
            let values = by_id.get(question.id.as_str());
            let unique =
                values.filter(|values| values.len() == 1 && !duplicates.contains(&question.id));
            let observation = unique.and_then(|values| values.first()).copied();
            let is_observed = observation.is_some();
            observed += usize::from(is_observed);
            errors += usize::from(observation.is_some_and(|value| value.error.is_some()));
            if let Some(value) = observation {
                latency += u128::from(value.latency_ms);
            }
            let passed = observation
                .filter(|value| value.error.is_none())
                .is_some_and(|value| matches_answer(question, &value.output));
            correct += usize::from(passed);
            let category = category_counts.entry(case.category).or_default();
            category.0 += 1;
            category.1 += usize::from(passed);
            let source = source_counts.entry(case.source.clone()).or_default();
            source.0 += 1;
            source.1 += usize::from(passed);
            questions.push(MemoryAgentBenchQuestionScore {
                question_id: question.id.clone(),
                category: case.category,
                source: case.source.clone(),
                metric: question.metric,
                observed: is_observed,
                passed,
            });
        }
    }
    MemoryAgentBenchReport {
        dataset: dataset.name.clone(),
        total: dataset.question_count(),
        observed,
        provider_errors: errors,
        accuracy: binomial(correct, dataset.question_count()),
        by_category: category_counts
            .into_iter()
            .map(|(key, (total, correct))| (key, binomial(correct, total)))
            .collect(),
        by_source: source_counts
            .into_iter()
            .map(|(key, (total, correct))| (key, binomial(correct, total)))
            .collect(),
        mean_latency_ms: latency as f64 / observed.max(1) as f64,
        duplicate_observation_ids: duplicates.into_iter().collect(),
        unexpected_observation_ids: unexpected.into_iter().collect(),
        questions,
    }
}

fn metric_for(category: MemoryAgentBenchCategory, source: &str) -> MemoryAgentBenchMetric {
    if source.eq_ignore_ascii_case("Recsys_redial_full") {
        MemoryAgentBenchMetric::RecallAt5
    } else {
        match category {
            MemoryAgentBenchCategory::AccurateRetrieval
            | MemoryAgentBenchCategory::ConflictResolution => MemoryAgentBenchMetric::SubstringAny,
            MemoryAgentBenchCategory::TestTimeLearning
            | MemoryAgentBenchCategory::LongRangeUnderstanding => {
                MemoryAgentBenchMetric::ExactMatch
            }
        }
    }
}

fn matches_answer(question: &MemoryAgentBenchQuestion, output: &str) -> bool {
    match question.metric {
        MemoryAgentBenchMetric::SubstringAny => {
            let output = normalized(output);
            question
                .ground_truths
                .iter()
                .any(|answer| output.contains(&normalized(answer)))
        }
        MemoryAgentBenchMetric::ExactMatch => question
            .ground_truths
            .iter()
            .any(|answer| output.trim() == answer.trim()),
        MemoryAgentBenchMetric::RecallAt5 => {
            let predictions = output
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .take(5)
                .map(normalized)
                .collect::<BTreeSet<_>>();
            question
                .ground_truths
                .iter()
                .any(|answer| predictions.contains(&normalized(answer)))
        }
    }
}

fn exported_rows(value: &serde_json::Value) -> Result<Vec<&serde_json::Value>, String> {
    let roots = match value {
        serde_json::Value::Array(roots) => roots.iter().collect::<Vec<_>>(),
        serde_json::Value::Object(_) => vec![value],
        _ => {
            return Err(
                "MemoryAgentBench export root must be a row, row array, or datasets-server page"
                    .into(),
            );
        }
    };
    let mut rows = Vec::new();
    for root in roots {
        if let Some(page) = root.get("rows").and_then(serde_json::Value::as_array) {
            rows.extend(page.iter().map(|item| item.get("row").unwrap_or(item)));
        } else {
            rows.push(root.get("row").unwrap_or(root));
        }
    }
    Ok(rows)
}

fn required_string(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("MemoryAgentBench row field {key:?} is missing"))
}

fn string_list(value: Option<&serde_json::Value>, field: &str) -> Result<Vec<String>, String> {
    let value = value.ok_or_else(|| format!("MemoryAgentBench {field} is missing"))?;
    match value {
        serde_json::Value::String(value) if !value.trim().is_empty() => Ok(vec![value.clone()]),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| format!("MemoryAgentBench {field} contains a non-string"))
            })
            .collect(),
        _ => Err(format!(
            "MemoryAgentBench {field} must be a string or array"
        )),
    }
}

fn answer_list(
    value: Option<&serde_json::Value>,
    question_count: usize,
) -> Result<Vec<Vec<String>>, String> {
    let value = value.ok_or("MemoryAgentBench answers are missing")?;
    match value {
        serde_json::Value::String(answer) if question_count == 1 && !answer.trim().is_empty() => {
            Ok(vec![vec![answer.clone()]])
        }
        serde_json::Value::Array(answers) if answers.len() == question_count => answers
            .iter()
            .map(|answer| match answer {
                serde_json::Value::String(answer) if !answer.trim().is_empty() => {
                    Ok(vec![answer.clone()])
                }
                serde_json::Value::Array(alternatives) => alternatives
                    .iter()
                    .map(|answer| {
                        answer
                            .as_str()
                            .filter(|answer| !answer.trim().is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| {
                                "MemoryAgentBench answer alternatives must be strings".into()
                            })
                    })
                    .collect(),
                _ => Err("MemoryAgentBench answers contain an invalid value".into()),
            })
            .collect(),
        _ => Err("MemoryAgentBench answers must align with questions".into()),
    }
}

fn normalized(value: &str) -> String {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn stable_slug(value: &str) -> String {
    let slug = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug.split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> serde_json::Value {
        serde_json::json!({ "rows": [{ "row": {
            "context": "The account changed from OLD-CITY to NEW-CITY.",
            "questions": ["Where is the account now?", "Give the exact label"],
            "answers": [["NEW-CITY", "new city"], "43"],
            "metadata": {
                "source": "factconsolidation_sh_6k",
                "qa_pair_ids": ["conflict-1", "conflict-2"]
            }
        }}]})
    }

    #[test]
    fn conflict_rows_preserve_incremental_context_questions_and_alternative_answers() {
        // Causes: C1 one datasets-server page; C2 aligned questions, answers,
        // and qa ids; C3 Conflict Resolution category. Effects: E1 one case
        // retains the whole context; E2 stable question ids and alternatives;
        // E3 substring metric. Rule I1=C1+C2+C3=>E1+E2+E3.
        let dataset = import_rows(&page(), MemoryAgentBenchCategory::ConflictResolution, 0)
            .expect("import Conflict Resolution page");
        assert_eq!(dataset.cases.len(), 1, "I1/E1");
        assert_eq!(dataset.question_count(), 2, "I1/E2");
        assert_eq!(dataset.cases[0].questions[0].id, "conflict-1", "I1/E2");
        assert_eq!(
            dataset.cases[0].questions[0].ground_truths.len(),
            2,
            "I1/E2"
        );
        assert_eq!(
            dataset.cases[0].questions[0].metric,
            MemoryAgentBenchMetric::SubstringAny,
            "I1/E3"
        );
    }

    #[test]
    fn category_specific_metrics_do_not_mislabel_test_time_learning_as_temporal_learning() {
        // Causes: C1 identical public rows are classified as Conflict Resolution
        // or Test-Time Learning. Effects: E1 Conflict uses normalized substring;
        // E2 TTL uses strict exact label matching. Rule I2=C1=>E1+E2. This keeps
        // test-time learning separate from timestamp/temporal evaluation.
        let conflict =
            import_rows(&page(), MemoryAgentBenchCategory::ConflictResolution, 0).unwrap();
        let ttl = import_rows(&page(), MemoryAgentBenchCategory::TestTimeLearning, 0).unwrap();
        let long_range =
            import_rows(&page(), MemoryAgentBenchCategory::LongRangeUnderstanding, 0).unwrap();
        assert_eq!(
            conflict.cases[0].questions[1].metric,
            MemoryAgentBenchMetric::SubstringAny,
            "I2/E1"
        );
        assert_eq!(
            ttl.cases[0].questions[1].metric,
            MemoryAgentBenchMetric::ExactMatch,
            "I2/E2"
        );
        assert_eq!(
            long_range.cases[0].questions[1].metric,
            MemoryAgentBenchMetric::ExactMatch,
            "I2/E2 Long-Range Understanding keeps the official strict metric"
        );
    }

    #[test]
    fn recommendation_source_scores_recall_at_five_at_the_exact_boundary() {
        // Causes: C1 the official recommendation source selects Recall@5; C2
        // the gold item occurs at rank 5/6. Effects: E1 rank 5 passes; E2 rank
        // 6 fails. Decision table: R1=C1+rank<=5=>E1;
        // R2=C1+rank>5=>E2. This pins the public scorer boundary rather than
        // substituting the generic substring metric.
        let page = serde_json::json!([{
            "context": "A recommendation dialogue.",
            "questions": ["Recommend a movie"],
            "answers": [["target movie"]],
            "metadata": {
                "source": "Recsys_redial_full",
                "qa_pair_ids": ["recommendation-1"]
            }
        }]);
        let dataset = import_rows(&page, MemoryAgentBenchCategory::AccurateRetrieval, 0).unwrap();
        assert_eq!(
            dataset.cases[0].questions[0].metric,
            MemoryAgentBenchMetric::RecallAt5,
            "R1,C1"
        );
        let observe = |output: &str| MemoryAgentBenchObservation {
            question_id: "recommendation-1".into(),
            output: output.into(),
            latency_ms: 1,
            error: None,
        };
        assert_eq!(
            score(&dataset, &[observe("one,two,three,four,target movie")])
                .accuracy
                .correct,
            1,
            "R1/E1"
        );
        assert_eq!(
            score(&dataset, &[observe("one,two,three,four,five,target movie")])
                .accuracy
                .correct,
            0,
            "R2/E2"
        );
    }

    #[test]
    fn scorer_fails_closed_on_duplicates_errors_and_unexpected_ids() {
        // Cause/effect decision table:
        // R1 one valid conflict answer -> observed+correct;
        // R2 duplicate id -> unobserved+incorrect and duplicate evidence;
        // R3 provider error -> observed+error+incorrect;
        // R4 unknown id -> unexpected evidence, never score another question.
        let dataset =
            import_rows(&page(), MemoryAgentBenchCategory::ConflictResolution, 0).unwrap();
        let report = score(
            &dataset,
            &[
                MemoryAgentBenchObservation {
                    question_id: "conflict-1".into(),
                    output: "The answer is new city.".into(),
                    latency_ms: 3,
                    error: None,
                },
                MemoryAgentBenchObservation {
                    question_id: "conflict-1".into(),
                    output: "NEW-CITY".into(),
                    latency_ms: 4,
                    error: None,
                },
                MemoryAgentBenchObservation {
                    question_id: "conflict-2".into(),
                    output: "43".into(),
                    latency_ms: 5,
                    error: Some("quota".into()),
                },
                MemoryAgentBenchObservation {
                    question_id: "unknown".into(),
                    output: "NEW-CITY".into(),
                    latency_ms: 1,
                    error: None,
                },
            ],
        );
        assert_eq!(report.observed, 1, "R3 only");
        assert_eq!(report.provider_errors, 1, "R3");
        assert_eq!(report.accuracy.correct, 0, "R2+R3 fail closed");
        assert_eq!(report.duplicate_observation_ids, ["conflict-1"], "R2");
        assert_eq!(report.unexpected_observation_ids, ["unknown"], "R4");
    }

    #[test]
    fn exact_and_substring_scoring_remain_distinct() {
        // Causes: same prediction wraps the gold label in prose. Effects:
        // Conflict substring passes; TTL exact fails. Decision rules S1/S2
        // cover the metric distinction without a stochastic model.
        let conflict =
            import_rows(&page(), MemoryAgentBenchCategory::ConflictResolution, 0).unwrap();
        let ttl = import_rows(&page(), MemoryAgentBenchCategory::TestTimeLearning, 0).unwrap();
        let observations = [MemoryAgentBenchObservation {
            question_id: "conflict-2".into(),
            output: "label: 43".into(),
            latency_ms: 1,
            error: None,
        }];
        assert!(score(&conflict, &observations).questions[1].passed, "S1");
        assert!(!score(&ttl, &observations).questions[1].passed, "S2");
    }
}
