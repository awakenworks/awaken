//! Public benchmark adapters and statistical scorers.
//!
//! These protocols stay separate from the product-specific three-state Outcome
//! contract. Preference ranking, reference compaction, and Memory selection do
//! not have interchangeable labels even though they share the ACP runner.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use awaken_ext_compact::{DEFAULT_COMPACT_INSTRUCTIONS, SUMMARIZE_PROMPT};
use serde::{Deserialize, Serialize};

use crate::acp_runner::ToolFreeAcpRunner;
use crate::memory_eval::{MemoryDataset, SelectionCase};

const VERSION: u32 = 1;
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    A,
    B,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairwiseCase {
    pub id: String,
    pub subset: String,
    pub prompt: String,
    pub response_a: String,
    pub response_b: String,
    pub expected: Choice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairwiseDataset {
    pub version: u32,
    pub name: String,
    pub cases: Vec<PairwiseCase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkObservation {
    pub case_id: String,
    pub output: String,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinomialMetrics {
    pub total: usize,
    pub correct: usize,
    pub rate: f64,
    pub ci95_low: f64,
    pub ci95_high: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairwiseReport {
    pub dataset: String,
    pub total: usize,
    pub observed: usize,
    pub provider_errors: usize,
    pub schema_valid: BinomialMetrics,
    pub accuracy: BinomialMetrics,
    pub accuracy_when_a_is_better: BinomialMetrics,
    pub accuracy_when_b_is_better: BinomialMetrics,
    pub by_subset: BTreeMap<String, BinomialMetrics>,
    pub mean_latency_ms: f64,
    pub duplicate_observation_ids: Vec<String>,
    pub unexpected_observation_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairwiseCrossReport {
    pub dataset: String,
    pub total: usize,
    pub jointly_valid: usize,
    pub agreement: BinomialMetrics,
    pub cohens_kappa: f64,
    pub both_correct: usize,
    pub left_only_correct: usize,
    pub right_only_correct: usize,
    pub both_wrong: usize,
    pub disagreement_case_ids: Vec<String>,
    pub left_invalid_case_ids: Vec<String>,
    pub right_invalid_case_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairwiseReply {
    better: Choice,
    explanation: String,
}

impl PairwiseDataset {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION || self.name.trim().is_empty() || self.cases.is_empty() {
            return Err("pairwise dataset needs version 1, a name, and cases".into());
        }
        let mut ids = BTreeSet::new();
        for case in &self.cases {
            if case.id.trim().is_empty()
                || case.subset.trim().is_empty()
                || case.prompt.trim().is_empty()
                || case.response_a.trim().is_empty()
                || case.response_b.trim().is_empty()
                || !ids.insert(&case.id)
            {
                return Err(format!("invalid or duplicate pairwise case {:?}", case.id));
            }
        }
        Ok(())
    }
}

/// Import the JSON returned by the Hugging Face datasets-server `rows` API for
/// `allenai/reward-bench-2`. Every rejected completion becomes one comparison;
/// chosen position alternates to expose position bias.
pub fn import_rewardbench2_rows(
    value: &serde_json::Value,
    limit: usize,
) -> Result<PairwiseDataset, String> {
    let pages = value
        .as_array()
        .map_or_else(|| vec![value], |pages| pages.iter().collect());
    let rows = pages
        .into_iter()
        .map(|page| {
            page.get("rows")
                .and_then(serde_json::Value::as_array)
                .ok_or("RewardBench 2 export page must contain a rows array")
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten();
    let mut cases = Vec::new();
    for row in rows {
        let row = row.get("row").unwrap_or(row);
        let id = string(row, "id")?;
        let prompt = string(row, "prompt")?;
        let subset = string(row, "subset")?;
        let chosen = strings(row, "chosen")?;
        let rejected = strings(row, "rejected")?;
        for (candidate, rejected) in rejected.into_iter().enumerate() {
            for chosen in &chosen {
                let expected = if cases.len() % 2 == 0 {
                    Choice::A
                } else {
                    Choice::B
                };
                let (response_a, response_b) = match expected {
                    Choice::A => (chosen.clone(), rejected.clone()),
                    Choice::B => (rejected.clone(), chosen.clone()),
                };
                cases.push(PairwiseCase {
                    id: format!("rb2-{id}-{candidate}-{}", cases.len()),
                    subset: subset.clone(),
                    prompt: prompt.clone(),
                    response_a,
                    response_b,
                    expected,
                });
            }
        }
    }
    if limit != 0 && cases.len() > limit {
        let mut groups: BTreeMap<_, std::collections::VecDeque<PairwiseCase>> = BTreeMap::new();
        for case in cases {
            groups
                .entry(case.subset.clone())
                .or_default()
                .push_back(case);
        }
        cases = round_robin_limit(groups, limit);
    }
    for (index, case) in cases.iter_mut().enumerate() {
        let desired = if index % 2 == 0 { Choice::A } else { Choice::B };
        if case.expected != desired {
            std::mem::swap(&mut case.response_a, &mut case.response_b);
            case.expected = desired;
        }
    }
    let dataset = PairwiseDataset {
        version: VERSION,
        name: "rewardbench2-pairwise".into(),
        cases,
    };
    dataset.validate()?;
    Ok(dataset)
}

pub async fn run_pairwise_acp(
    dataset: &PairwiseDataset,
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> Vec<BenchmarkObservation> {
    let runner = ToolFreeAcpRunner::new(argv, env);
    let mut observations = Vec::with_capacity(dataset.cases.len());
    for (sequence, case) in dataset.cases.iter().enumerate() {
        let input = format!(
            "USER REQUEST:\n{}\n\nRESPONSE A:\n{}\n\nRESPONSE B:\n{}\n\nReturn only JSON.",
            case.prompt, case.response_a, case.response_b
        );
        observations.push(
            run_one(
                &runner,
                "pairwise-judge-eval",
                sequence,
                "Compare the two candidate responses only against the user's request. Prefer the response that is more correct, relevant, self-contained, safe, and instruction-following. Do not favor a response because of its position, length, style, or identity. Return exactly {\"better\":\"a\"|\"b\",\"explanation\":\"brief reason\"} with no additional keys or text.",
                input,
                &case.id,
            )
            .await,
        );
    }
    observations
}

#[must_use]
pub fn score_pairwise(
    dataset: &PairwiseDataset,
    observations: &[BenchmarkObservation],
) -> PairwiseReport {
    let (by_id, duplicates, unexpected) = index_observations(
        dataset.cases.iter().map(|case| case.id.as_str()),
        observations,
    );
    let mut valid = 0;
    let mut correct = 0;
    let mut by_subset: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_position: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut latency = 0_u128;
    let mut observed = 0;
    let mut provider_errors = 0;
    for case in &dataset.cases {
        by_subset.entry(case.subset.clone()).or_default().0 += 1;
        by_position
            .entry(match case.expected {
                Choice::A => "a",
                Choice::B => "b",
            })
            .or_default()
            .0 += 1;
        let entries = by_id.get(case.id.as_str()).cloned().unwrap_or_default();
        provider_errors += usize::from(entries.len() == 1 && entries[0].error.is_some());
        if entries.len() != 1 || entries[0].error.is_some() {
            continue;
        }
        observed += 1;
        latency += u128::from(entries[0].latency_ms);
        let Ok(reply) = serde_json::from_str::<PairwiseReply>(&entries[0].output) else {
            continue;
        };
        if reply.explanation.trim().is_empty() {
            continue;
        }
        valid += 1;
        let hit = reply.better == case.expected;
        correct += usize::from(hit);
        by_subset.entry(case.subset.clone()).or_default().1 += usize::from(hit);
        let position = by_position
            .entry(match case.expected {
                Choice::A => "a",
                Choice::B => "b",
            })
            .or_default();
        position.1 += usize::from(hit);
    }
    PairwiseReport {
        dataset: dataset.name.clone(),
        total: dataset.cases.len(),
        observed,
        provider_errors,
        schema_valid: binomial(valid, dataset.cases.len()),
        accuracy: binomial(correct, dataset.cases.len()),
        accuracy_when_a_is_better: metric_tuple(by_position.get("a").copied().unwrap_or_default()),
        accuracy_when_b_is_better: metric_tuple(by_position.get("b").copied().unwrap_or_default()),
        by_subset: by_subset
            .into_iter()
            .map(|(key, value)| (key, metric_tuple(value)))
            .collect(),
        mean_latency_ms: if observed == 0 {
            0.0
        } else {
            latency as f64 / observed as f64
        },
        duplicate_observation_ids: duplicates,
        unexpected_observation_ids: unexpected,
    }
}

#[must_use]
pub fn compare_pairwise(
    dataset: &PairwiseDataset,
    left: &[BenchmarkObservation],
    right: &[BenchmarkObservation],
) -> PairwiseCrossReport {
    let mut left_by_id = BTreeMap::new();
    let mut right_by_id = BTreeMap::new();
    for observation in left {
        left_by_id
            .entry(observation.case_id.as_str())
            .or_insert_with(Vec::new)
            .push(observation);
    }
    for observation in right {
        right_by_id
            .entry(observation.case_id.as_str())
            .or_insert_with(Vec::new)
            .push(observation);
    }
    let mut jointly_valid = 0;
    let mut agreements = 0;
    let mut both_correct = 0;
    let mut left_only_correct = 0;
    let mut right_only_correct = 0;
    let mut both_wrong = 0;
    let mut left_a = 0;
    let mut right_a = 0;
    let mut disagreement_case_ids = Vec::new();
    let mut left_invalid_case_ids = Vec::new();
    let mut right_invalid_case_ids = Vec::new();
    for case in &dataset.cases {
        let left_prediction = prediction(left_by_id.get(case.id.as_str()));
        let right_prediction = prediction(right_by_id.get(case.id.as_str()));
        if left_prediction.is_none() {
            left_invalid_case_ids.push(case.id.clone());
        }
        if right_prediction.is_none() {
            right_invalid_case_ids.push(case.id.clone());
        }
        let (Some(left_prediction), Some(right_prediction)) = (left_prediction, right_prediction)
        else {
            continue;
        };
        jointly_valid += 1;
        left_a += usize::from(left_prediction == Choice::A);
        right_a += usize::from(right_prediction == Choice::A);
        let agrees = left_prediction == right_prediction;
        agreements += usize::from(agrees);
        if !agrees {
            disagreement_case_ids.push(case.id.clone());
        }
        match (
            left_prediction == case.expected,
            right_prediction == case.expected,
        ) {
            (true, true) => both_correct += 1,
            (true, false) => left_only_correct += 1,
            (false, true) => right_only_correct += 1,
            (false, false) => both_wrong += 1,
        }
    }
    let cohens_kappa = if jointly_valid == 0 {
        0.0
    } else {
        let n = jointly_valid as f64;
        let observed = agreements as f64 / n;
        let left_a = left_a as f64 / n;
        let right_a = right_a as f64 / n;
        let expected = left_a * right_a + (1.0 - left_a) * (1.0 - right_a);
        if (1.0 - expected).abs() < f64::EPSILON {
            1.0
        } else {
            (observed - expected) / (1.0 - expected)
        }
    };
    PairwiseCrossReport {
        dataset: dataset.name.clone(),
        total: dataset.cases.len(),
        jointly_valid,
        agreement: binomial(agreements, jointly_valid),
        cohens_kappa,
        both_correct,
        left_only_correct,
        right_only_correct,
        both_wrong,
        disagreement_case_ids,
        left_invalid_case_ids,
        right_invalid_case_ids,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCompactCase {
    pub id: String,
    pub query: String,
    pub transcript: String,
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCompactDataset {
    pub version: u32,
    pub name: String,
    pub cases: Vec<ReferenceCompactCase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceCompactReport {
    pub dataset: String,
    pub total: usize,
    pub observed: usize,
    pub provider_errors: usize,
    pub mean_token_precision: f64,
    pub mean_token_recall: f64,
    pub mean_token_f1: f64,
    pub mean_compression_ratio: f64,
    pub mean_latency_ms: f64,
}

impl ReferenceCompactDataset {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION || self.name.trim().is_empty() || self.cases.is_empty() {
            return Err("reference compact dataset needs version 1, a name, and cases".into());
        }
        let mut ids = BTreeSet::new();
        if self.cases.iter().any(|case| {
            case.id.trim().is_empty()
                || case.query.trim().is_empty()
                || case.transcript.trim().is_empty()
                || case.reference.trim().is_empty()
                || !ids.insert(&case.id)
        }) {
            return Err("invalid or duplicate reference compact case".into());
        }
        Ok(())
    }
}

/// Import QMSum meeting JSON documents. General and specific queries are both
/// retained; the caller controls cost with `limit`.
pub fn import_qmsum_documents(
    documents: &[serde_json::Value],
    limit: usize,
) -> Result<ReferenceCompactDataset, String> {
    let mut cases = Vec::new();
    for (document_index, document) in documents.iter().enumerate() {
        let transcript = document
            .get("meeting_transcripts")
            .and_then(serde_json::Value::as_array)
            .ok_or("QMSum document lacks meeting_transcripts")?
            .iter()
            .map(|turn| {
                format!(
                    "{}: {}",
                    string(turn, "speaker").unwrap_or_default(),
                    string(turn, "content").unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let queries = ["general_query_list", "specific_query_list"]
            .into_iter()
            .flat_map(|key| {
                document
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
            });
        for (query_index, query) in queries.enumerate() {
            cases.push(ReferenceCompactCase {
                id: format!("qmsum-{document_index}-{query_index}"),
                query: string(query, "query")?,
                transcript: transcript.clone(),
                reference: string(query, "answer")?,
            });
        }
    }
    if limit != 0 && cases.len() > limit {
        let mut groups: BTreeMap<_, std::collections::VecDeque<ReferenceCompactCase>> =
            BTreeMap::new();
        for case in cases {
            let document = case
                .id
                .rsplit_once('-')
                .map_or_else(|| case.id.clone(), |(document, _)| document.to_string());
            groups.entry(document).or_default().push_back(case);
        }
        cases = round_robin_limit(groups, limit);
    }
    let dataset = ReferenceCompactDataset {
        version: VERSION,
        name: "qmsum-reference-compact".into(),
        cases,
    };
    dataset.validate()?;
    Ok(dataset)
}

pub async fn run_reference_compact_acp(
    dataset: &ReferenceCompactDataset,
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> Vec<BenchmarkObservation> {
    let runner = ToolFreeAcpRunner::new(argv, env);
    let mut observations = Vec::with_capacity(dataset.cases.len());
    for (sequence, case) in dataset.cases.iter().enumerate() {
        let input = format!(
            "Focus the compact state on this pending information need: {}\n\n{}\n\n{}",
            case.query, case.transcript, SUMMARIZE_PROMPT
        );
        observations.push(
            run_one(
                &runner,
                "reference-compact-eval",
                sequence,
                DEFAULT_COMPACT_INSTRUCTIONS,
                input,
                &case.id,
            )
            .await,
        );
    }
    observations
}

#[must_use]
pub fn score_reference_compact(
    dataset: &ReferenceCompactDataset,
    observations: &[BenchmarkObservation],
) -> ReferenceCompactReport {
    let mut precision = 0.0;
    let mut recall = 0.0;
    let mut f1 = 0.0;
    let mut ratio = 0.0;
    let mut latency = 0_u128;
    let mut observed = 0;
    let mut provider_errors = 0;
    for case in &dataset.cases {
        let case_observations = observations
            .iter()
            .filter(|item| item.case_id == case.id)
            .collect::<Vec<_>>();
        provider_errors +=
            usize::from(case_observations.len() == 1 && case_observations[0].error.is_some());
        let matches = case_observations
            .into_iter()
            .filter(|item| item.error.is_none())
            .collect::<Vec<_>>();
        if matches.len() != 1 || matches[0].output.trim().is_empty() {
            continue;
        }
        observed += 1;
        latency += u128::from(matches[0].latency_ms);
        let (p, r, score) = token_f1(&matches[0].output, &case.reference);
        precision += p;
        recall += r;
        f1 += score;
        ratio +=
            tokens(&matches[0].output).len() as f64 / tokens(&case.transcript).len().max(1) as f64;
    }
    let divisor = observed.max(1) as f64;
    ReferenceCompactReport {
        dataset: dataset.name.clone(),
        total: dataset.cases.len(),
        observed,
        provider_errors,
        mean_token_precision: precision / divisor,
        mean_token_recall: recall / divisor,
        mean_token_f1: f1 / divisor,
        mean_compression_ratio: ratio / divisor,
        mean_latency_ms: latency as f64 / divisor,
    }
}

/// Convert LoCoMo QA annotations into production Memory-selector cases. Gold
/// evidence turns are mixed with lexical hard negatives, so this evaluates the
/// selector/reranker without pretending to evaluate vector retrieval.
pub fn import_locomo_selection(
    value: &serde_json::Value,
    limit: usize,
    distractors: usize,
) -> Result<MemoryDataset, String> {
    let conversations = value.as_array().ok_or("LoCoMo root must be an array")?;
    let mut groups: BTreeMap<String, std::collections::VecDeque<SelectionCase>> = BTreeMap::new();
    for conversation in conversations {
        let sample_id = string(conversation, "sample_id")?;
        let sessions = conversation
            .get("conversation")
            .and_then(serde_json::Value::as_object)
            .ok_or("LoCoMo conversation missing")?;
        let mut turns = Vec::new();
        for value in sessions.values() {
            let Some(session) = value.as_array() else {
                continue;
            };
            for turn in session {
                let id = string(turn, "dia_id")?;
                let speaker = string(turn, "speaker")?;
                let text = string(turn, "text")?;
                turns.push((id, format!("{speaker}: {text}")));
            }
        }
        let by_id = turns
            .iter()
            .map(|(id, text)| (id.as_str(), text.as_str()))
            .collect::<BTreeMap<_, _>>();
        for (qa_index, qa) in conversation
            .get("qa")
            .and_then(serde_json::Value::as_array)
            .ok_or("LoCoMo qa missing")?
            .iter()
            .enumerate()
        {
            let question = string(qa, "question")?;
            let category = qa
                .get("category")
                .map(serde_json::Value::to_string)
                .unwrap_or_else(|| "unknown".into());
            let evidence = qa
                .get("evidence")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if evidence.is_empty() || evidence.iter().any(|id| !by_id.contains_key(id)) {
                continue;
            }
            let expected_ids = evidence.iter().copied().collect::<BTreeSet<_>>();
            let query_terms = tokens(&question).into_iter().collect::<BTreeSet<_>>();
            let mut negatives = turns
                .iter()
                .filter(|(id, _)| !expected_ids.contains(id.as_str()))
                .map(|(id, text)| {
                    let overlap = tokens(text)
                        .into_iter()
                        .filter(|term| query_terms.contains(term))
                        .count();
                    (std::cmp::Reverse(overlap), id, text)
                })
                .collect::<Vec<_>>();
            negatives.sort();
            let mut candidates = evidence
                .iter()
                .map(|id| ((*id).to_string(), by_id[*id].to_string(), true))
                .collect::<Vec<_>>();
            candidates.extend(
                negatives
                    .into_iter()
                    .take(distractors)
                    .map(|(_, id, text)| (id.clone(), text.clone(), false)),
            );
            candidates.sort_by(|left, right| left.0.cmp(&right.0));
            let expected_indices = candidates
                .iter()
                .enumerate()
                .filter_map(|(index, (_, _, expected))| expected.then_some(index))
                .collect::<Vec<_>>();
            groups
                .entry(format!("{sample_id}/category-{category}"))
                .or_default()
                .push_back(SelectionCase {
                    id: format!("locomo-{sample_id}-category-{category}-{qa_index}"),
                    query: question,
                    memories: candidates
                        .into_iter()
                        .map(|(id, text, _)| format!("{id}: {text}"))
                        .collect(),
                    max: expected_indices.len(),
                    expected_indices,
                });
        }
    }
    let cases = round_robin_limit(groups, limit);
    let dataset = MemoryDataset {
        version: VERSION,
        name: "locomo-memory-selection".into(),
        extraction_cases: Vec::new(),
        selection_cases: cases,
    };
    dataset.validate()?;
    Ok(dataset)
}

fn round_robin_limit<T>(
    mut groups: BTreeMap<String, std::collections::VecDeque<T>>,
    limit: usize,
) -> Vec<T> {
    let available = groups.values().map(std::collections::VecDeque::len).sum();
    let target = if limit == 0 {
        available
    } else {
        limit.min(available)
    };
    let mut selected = Vec::with_capacity(target);
    while selected.len() < target {
        let mut progressed = false;
        for group in groups.values_mut() {
            if let Some(case) = group.pop_front() {
                selected.push(case);
                progressed = true;
                if selected.len() == target {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
    }
    selected
}

async fn run_one(
    runner: &ToolFreeAcpRunner,
    namespace: &str,
    sequence: usize,
    instructions: &str,
    input: String,
    case_id: &str,
) -> BenchmarkObservation {
    let started = Instant::now();
    let result = tokio::time::timeout(
        TIMEOUT,
        runner.run(namespace, sequence, instructions, input, 2),
    )
    .await;
    let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let (output, error) = match result {
        Ok(Ok(output)) => (output, None),
        Ok(Err(error)) => (String::new(), Some(error.to_string())),
        Err(_) => (String::new(), Some(format!("ACP {namespace} timed out"))),
    };
    BenchmarkObservation {
        case_id: case_id.into(),
        output,
        latency_ms,
        error,
    }
}

fn index_observations<'a>(
    expected: impl Iterator<Item = &'a str>,
    observations: &'a [BenchmarkObservation],
) -> (
    BTreeMap<&'a str, Vec<&'a BenchmarkObservation>>,
    Vec<String>,
    Vec<String>,
) {
    let expected = expected.collect::<BTreeSet<_>>();
    let mut by_id: BTreeMap<&str, Vec<&BenchmarkObservation>> = BTreeMap::new();
    let mut unexpected = BTreeSet::new();
    for observation in observations {
        if expected.contains(observation.case_id.as_str()) {
            by_id
                .entry(&observation.case_id)
                .or_default()
                .push(observation);
        } else {
            unexpected.insert(observation.case_id.clone());
        }
    }
    let duplicates = by_id
        .iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(id, _)| (*id).to_string())
        .collect();
    (by_id, duplicates, unexpected.into_iter().collect())
}

fn binomial(correct: usize, total: usize) -> BinomialMetrics {
    if total == 0 {
        return BinomialMetrics {
            total,
            correct,
            rate: 0.0,
            ci95_low: 0.0,
            ci95_high: 0.0,
        };
    }
    let n = total as f64;
    let p = correct as f64 / n;
    let z = 1.959_963_984_540_054;
    let denominator = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denominator;
    let margin = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denominator;
    let low = if correct == 0 {
        0.0
    } else {
        (center - margin).max(0.0)
    };
    let high = if correct == total {
        1.0
    } else {
        (center + margin).min(1.0)
    };
    BinomialMetrics {
        total,
        correct,
        rate: p,
        ci95_low: low,
        ci95_high: high,
    }
}

fn metric_tuple((total, correct): (usize, usize)) -> BinomialMetrics {
    binomial(correct, total)
}

fn prediction(observations: Option<&Vec<&BenchmarkObservation>>) -> Option<Choice> {
    let observations = observations?;
    if observations.len() != 1 || observations[0].error.is_some() {
        return None;
    }
    serde_json::from_str::<PairwiseReply>(&observations[0].output)
        .ok()
        .filter(|reply| !reply.explanation.trim().is_empty())
        .map(|reply| reply.better)
}

fn string(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing string field {key:?}"))
}

fn strings(value: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("missing array field {key:?}"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("non-string in {key:?}"))
        })
        .collect()
}

fn tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn token_f1(actual: &str, expected: &str) -> (f64, f64, f64) {
    let actual = tokens(actual);
    let expected = tokens(expected);
    let mut remaining = expected.iter().fold(BTreeMap::new(), |mut counts, token| {
        *counts.entry(token).or_insert(0_usize) += 1;
        counts
    });
    let overlap = actual
        .iter()
        .filter(|token| {
            remaining.get_mut(token).is_some_and(|count| {
                if *count == 0 {
                    false
                } else {
                    *count -= 1;
                    true
                }
            })
        })
        .count();
    let precision = overlap as f64 / actual.len().max(1) as f64;
    let recall = overlap as f64 / expected.len().max(1) as f64;
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    (precision, recall, f1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewardbench_adapter_balances_response_position() {
        let input = serde_json::json!({"rows":[{"row":{"id":"1","prompt":"p","chosen":["good"],"rejected":["bad-1","bad-2"],"subset":"reasoning"}}]});
        let dataset = import_rewardbench2_rows(&input, 0).unwrap();
        assert_eq!(dataset.cases.len(), 2);
        assert_eq!(dataset.cases[0].expected, Choice::A);
        assert_eq!(dataset.cases[1].expected, Choice::B);
    }

    #[test]
    fn rewardbench_limit_is_stratified_instead_of_taking_an_ordered_prefix() {
        let input = serde_json::json!({"rows":[
            {"row":{"id":"1","prompt":"p1","chosen":["good"],"rejected":["bad-1","bad-2"],"subset":"a"}},
            {"row":{"id":"2","prompt":"p2","chosen":["good"],"rejected":["bad-1","bad-2"],"subset":"b"}}
        ]});
        let dataset = import_rewardbench2_rows(&input, 2).unwrap();
        assert_eq!(
            dataset
                .cases
                .iter()
                .map(|case| case.subset.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["a", "b"])
        );
        assert_eq!(dataset.cases[0].expected, Choice::A);
        assert_eq!(dataset.cases[1].expected, Choice::B);
    }

    #[test]
    fn pairwise_score_reports_wilson_interval_and_position_slices() {
        let input = serde_json::json!({"rows":[{"row":{"id":"1","prompt":"p","chosen":["good"],"rejected":["bad-1","bad-2"],"subset":"reasoning"}}]});
        let dataset = import_rewardbench2_rows(&input, 0).unwrap();
        let observations = dataset
            .cases
            .iter()
            .map(|case| BenchmarkObservation {
                case_id: case.id.clone(),
                output: format!(
                    r#"{{"better":"{}","explanation":"grounded"}}"#,
                    match case.expected {
                        Choice::A => "a",
                        Choice::B => "b",
                    }
                ),
                latency_ms: 10,
                error: None,
            })
            .collect::<Vec<_>>();
        let report = score_pairwise(&dataset, &observations);
        assert_eq!(report.accuracy.correct, 2);
        assert!(report.accuracy.ci95_low < 1.0);
        assert_eq!(report.accuracy_when_a_is_better.total, 1);
        assert_eq!(report.accuracy_when_b_is_better.total, 1);
    }

    #[test]
    fn wilson_interval_is_bounded_and_contains_every_finite_sample_rate() {
        for total in 1..=100 {
            for correct in 0..=total {
                let metrics = binomial(correct, total);
                assert!((0.0..=1.0).contains(&metrics.ci95_low));
                assert!((0.0..=1.0).contains(&metrics.ci95_high));
                assert!(metrics.ci95_low <= metrics.rate);
                assert!(metrics.rate <= metrics.ci95_high);
            }
        }
    }

    #[test]
    fn invalid_schema_counts_as_wrong_in_every_accuracy_denominator() {
        let input = serde_json::json!({"rows":[{"row":{"id":"1","prompt":"p","chosen":["good"],"rejected":["bad-1","bad-2"],"subset":"reasoning"}}]});
        let dataset = import_rewardbench2_rows(&input, 0).unwrap();
        let observations = vec![BenchmarkObservation {
            case_id: dataset.cases[0].id.clone(),
            output: r#"{"better":"a","explanation":"reason"}"#.into(),
            latency_ms: 1,
            error: None,
        }];
        let report = score_pairwise(&dataset, &observations);
        assert_eq!(report.by_subset["reasoning"].total, 2);
        assert_eq!(report.schema_valid.total, 2);
        assert_eq!(report.observed, 1);
    }

    #[test]
    fn cross_report_separates_disagreement_from_joint_failure() {
        let input = serde_json::json!({"rows":[{"row":{"id":"1","prompt":"p","chosen":["good"],"rejected":["bad-1","bad-2"],"subset":"reasoning"}}]});
        let dataset = import_rewardbench2_rows(&input, 0).unwrap();
        let left = dataset
            .cases
            .iter()
            .map(|case| BenchmarkObservation {
                case_id: case.id.clone(),
                output: r#"{"better":"a","explanation":"reason"}"#.into(),
                latency_ms: 1,
                error: None,
            })
            .collect::<Vec<_>>();
        let right = dataset
            .cases
            .iter()
            .map(|case| BenchmarkObservation {
                case_id: case.id.clone(),
                output: r#"{"better":"b","explanation":"reason"}"#.into(),
                latency_ms: 1,
                error: None,
            })
            .collect::<Vec<_>>();
        let report = compare_pairwise(&dataset, &left, &right);
        assert_eq!(report.jointly_valid, 2);
        assert_eq!(report.agreement.correct, 0);
        assert_eq!(report.left_only_correct, 1);
        assert_eq!(report.right_only_correct, 1);
        assert_eq!(report.disagreement_case_ids.len(), 2);
    }

    #[test]
    fn qmsum_adapter_and_reference_score_are_deterministic() {
        let document = serde_json::json!({"meeting_transcripts":[{"speaker":"A","content":"alpha beta gamma"}],"general_query_list":[{"query":"what?","answer":"alpha beta"}],"specific_query_list":[]});
        let dataset = import_qmsum_documents(&[document], 1).unwrap();
        let report = score_reference_compact(
            &dataset,
            &[BenchmarkObservation {
                case_id: dataset.cases[0].id.clone(),
                output: "alpha beta".into(),
                latency_ms: 2,
                error: None,
            }],
        );
        assert_eq!(report.mean_token_f1, 1.0);
        assert!(report.mean_compression_ratio < 1.0);
    }

    #[test]
    fn qmsum_limit_samples_across_meetings_not_one_ordered_prefix() {
        let document = |speaker: &str| {
            serde_json::json!({
                "meeting_transcripts":[{"speaker":speaker,"content":"alpha beta gamma"}],
                "general_query_list":[{"query":"q1","answer":"alpha"},{"query":"q2","answer":"beta"}],
                "specific_query_list":[]
            })
        };
        let dataset = import_qmsum_documents(&[document("A"), document("B")], 2).unwrap();
        assert!(dataset.cases[0].transcript.starts_with("A:"));
        assert!(dataset.cases[1].transcript.starts_with("B:"));
    }

    #[test]
    fn locomo_adapter_keeps_gold_evidence_and_adds_distractors() {
        let input = serde_json::json!([{"sample_id":"c1","conversation":{"session_1":[{"dia_id":"D1:1","speaker":"A","text":"likes blue"},{"dia_id":"D1:2","speaker":"B","text":"likes red"},{"dia_id":"D1:3","speaker":"A","text":"works remote"}]},"qa":[{"question":"What color does A like?","answer":"blue","evidence":["D1:1"],"category":1}]}]);
        let dataset = import_locomo_selection(&input, 1, 2).unwrap();
        assert_eq!(dataset.selection_cases.len(), 1);
        assert_eq!(dataset.selection_cases[0].memories.len(), 3);
        assert_eq!(dataset.selection_cases[0].expected_indices.len(), 1);
    }

    #[test]
    fn locomo_limit_round_robins_conversations_and_categories() {
        let conversation = |sample: &str, category: usize| {
            serde_json::json!({
                "sample_id":sample,
                "conversation":{"session_1":[
                    {"dia_id":"D1:1","speaker":"A","text":"gold"},
                    {"dia_id":"D1:2","speaker":"B","text":"distractor"}
                ]},
                "qa":[
                    {"question":"q1","answer":"gold","evidence":["D1:1"],"category":category},
                    {"question":"q2","answer":"gold","evidence":["D1:1"],"category":category}
                ]
            })
        };
        let dataset = import_locomo_selection(
            &serde_json::json!([conversation("c1", 1), conversation("c2", 2)]),
            2,
            1,
        )
        .unwrap();
        assert!(dataset.selection_cases[0].id.contains("c1"));
        assert!(dataset.selection_cases[1].id.contains("c2"));
    }
}
