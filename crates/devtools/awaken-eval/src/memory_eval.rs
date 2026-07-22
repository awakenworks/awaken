//! Decision-quality evaluation for memory extraction and relevance selection.
//!
//! Extraction uses a tool-free JSON projection of the proposed `write_memory`
//! calls. Production tool mechanics remain covered by `awaken-ext-memory` tests.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use awaken_ext_memory::{
    DEFAULT_MEMORY_INSTRUCTIONS, DEFAULT_SELECTOR_INSTRUCTIONS, parse_indices, select_input,
};
use serde::{Deserialize, Serialize};

use crate::acp_runner::ToolFreeAcpRunner;

const TIMEOUT: Duration = Duration::from_secs(300);
const EXTRACTION_PROJECTION: &str = "For this evaluation only, do not call tools. Return ONLY a JSON array of the write_memory calls you would make. Each item must contain exactly the string keys name and content. Return [] when nothing is worth saving.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryDataset {
    pub version: u32,
    pub name: String,
    pub extraction_cases: Vec<ExtractionCase>,
    pub selection_cases: Vec<SelectionCase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionCase {
    pub id: String,
    pub transcript: String,
    /// Every inner group is one expected durable fact; all terms in that group
    /// must occur somewhere in the proposed memory content.
    pub expected_term_groups: Vec<Vec<String>>,
    pub forbidden_terms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionCase {
    pub id: String,
    pub query: String,
    pub memories: Vec<String>,
    pub max: usize,
    pub expected_indices: Vec<usize>,
}

impl MemoryDataset {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.name.trim().is_empty()
            || (self.extraction_cases.is_empty() && self.selection_cases.is_empty())
        {
            return Err("memory dataset needs version 1, a name, and cases".into());
        }
        let mut ids = BTreeSet::new();
        for case in &self.extraction_cases {
            if case.id.trim().is_empty()
                || case.transcript.trim().is_empty()
                || !ids.insert(&case.id)
                || case.expected_term_groups.iter().any(|group| {
                    group.is_empty() || group.iter().any(|term| term.trim().is_empty())
                })
                || case
                    .forbidden_terms
                    .iter()
                    .any(|term| term.trim().is_empty())
            {
                return Err(format!(
                    "invalid or duplicate extraction case {:?}",
                    case.id
                ));
            }
        }
        for case in &self.selection_cases {
            let expected = case
                .expected_indices
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            if case.id.trim().is_empty()
                || case.query.trim().is_empty()
                || case.memories.is_empty()
                || case.max == 0
                || !ids.insert(&case.id)
                || expected.len() != case.expected_indices.len()
                || expected.len() > case.max
                || expected.iter().any(|index| *index >= case.memories.len())
            {
                return Err(format!("invalid or duplicate selection case {:?}", case.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryObservation {
    pub case_id: String,
    pub output: String,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedMemory {
    name: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionScore {
    pub case_id: String,
    pub schema_valid: bool,
    pub expected_found: usize,
    pub expected_total: usize,
    pub forbidden_dropped: usize,
    pub forbidden_total: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionScore {
    pub case_id: String,
    pub schema_valid: bool,
    pub expected_indices: Vec<usize>,
    pub actual_indices: Vec<usize>,
    pub true_positive: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryReport {
    pub extraction_exact: usize,
    pub extraction_total: usize,
    pub extraction_observed: usize,
    pub extraction_errors: usize,
    pub extraction_schema_valid: usize,
    pub expected_memories_found: usize,
    pub expected_memories_total: usize,
    pub forbidden_terms_dropped: usize,
    pub forbidden_terms_total: usize,
    pub selection_exact: usize,
    pub selection_total: usize,
    pub selection_observed: usize,
    pub selection_errors: usize,
    pub selection_schema_valid: usize,
    pub selection_true_positive: usize,
    pub selection_expected: usize,
    pub selection_returned: usize,
    pub extraction_cases: Vec<ExtractionScore>,
    pub selection_cases: Vec<SelectionScore>,
}

#[must_use]
pub fn score(dataset: &MemoryDataset, observations: &[MemoryObservation]) -> MemoryReport {
    let mut report = MemoryReport {
        extraction_exact: 0,
        extraction_total: dataset.extraction_cases.len(),
        extraction_observed: 0,
        extraction_errors: 0,
        extraction_schema_valid: 0,
        expected_memories_found: 0,
        expected_memories_total: 0,
        forbidden_terms_dropped: 0,
        forbidden_terms_total: 0,
        selection_exact: 0,
        selection_total: dataset.selection_cases.len(),
        selection_observed: 0,
        selection_errors: 0,
        selection_schema_valid: 0,
        selection_true_positive: 0,
        selection_expected: 0,
        selection_returned: 0,
        extraction_cases: Vec::with_capacity(dataset.extraction_cases.len()),
        selection_cases: Vec::with_capacity(dataset.selection_cases.len()),
    };
    for case in &dataset.extraction_cases {
        let outputs = observations
            .iter()
            .filter(|observation| observation.case_id == case.id)
            .collect::<Vec<_>>();
        report.extraction_observed += usize::from(outputs.len() == 1);
        report.extraction_errors += usize::from(outputs.len() == 1 && outputs[0].error.is_some());
        let parsed = if outputs.len() == 1 && outputs[0].error.is_none() {
            serde_json::from_str::<Vec<ProposedMemory>>(&outputs[0].output)
                .ok()
                .filter(|memories| {
                    memories.iter().all(|memory| {
                        !memory.name.trim().is_empty() && !memory.content.trim().is_empty()
                    })
                })
        } else {
            None
        };
        let schema_valid = parsed.is_some();
        let blob = parsed
            .unwrap_or_default()
            .into_iter()
            .map(|memory| memory.content)
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        let expected_found = case
            .expected_term_groups
            .iter()
            .filter(|group| {
                group
                    .iter()
                    .all(|term| blob.contains(&term.to_ascii_lowercase()))
            })
            .count();
        let forbidden_dropped = case
            .forbidden_terms
            .iter()
            .filter(|term| !blob.contains(&term.to_ascii_lowercase()))
            .count();
        let passed = schema_valid
            && expected_found == case.expected_term_groups.len()
            && forbidden_dropped == case.forbidden_terms.len();
        report.extraction_schema_valid += usize::from(schema_valid);
        report.extraction_exact += usize::from(passed);
        report.expected_memories_found += expected_found;
        report.expected_memories_total += case.expected_term_groups.len();
        report.forbidden_terms_dropped += forbidden_dropped;
        report.forbidden_terms_total += case.forbidden_terms.len();
        report.extraction_cases.push(ExtractionScore {
            case_id: case.id.clone(),
            schema_valid,
            expected_found,
            expected_total: case.expected_term_groups.len(),
            forbidden_dropped,
            forbidden_total: case.forbidden_terms.len(),
            passed,
        });
    }
    for case in &dataset.selection_cases {
        let outputs = observations
            .iter()
            .filter(|observation| observation.case_id == case.id)
            .collect::<Vec<_>>();
        report.selection_observed += usize::from(outputs.len() == 1);
        report.selection_errors += usize::from(outputs.len() == 1 && outputs[0].error.is_some());
        let (schema_valid, actual) = if outputs.len() == 1 && outputs[0].error.is_none() {
            parse_selection(&outputs[0].output, case.memories.len(), case.max)
        } else {
            (false, Vec::new())
        };
        let expected = case
            .expected_indices
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let true_positive = actual
            .iter()
            .filter(|index| expected.contains(index))
            .count();
        let actual_set = actual.iter().copied().collect::<BTreeSet<_>>();
        let passed = schema_valid && actual_set == expected;
        report.selection_schema_valid += usize::from(schema_valid);
        report.selection_exact += usize::from(passed);
        report.selection_true_positive += true_positive;
        report.selection_expected += expected.len();
        report.selection_returned += actual.len();
        report.selection_cases.push(SelectionScore {
            case_id: case.id.clone(),
            schema_valid,
            expected_indices: case.expected_indices.clone(),
            actual_indices: actual,
            true_positive,
            passed,
        });
    }
    report
}

fn parse_selection(reply: &str, count: usize, max: usize) -> (bool, Vec<usize>) {
    if reply.trim() == "NONE" {
        return (true, Vec::new());
    }
    let parsed = parse_indices(reply, count, max);
    (!parsed.is_empty(), parsed)
}

pub async fn run_acp(
    dataset: &MemoryDataset,
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> Vec<MemoryObservation> {
    let runner = ToolFreeAcpRunner::new(argv, env);
    let mut observations =
        Vec::with_capacity(dataset.extraction_cases.len() + dataset.selection_cases.len());
    for (sequence, case) in dataset.extraction_cases.iter().enumerate() {
        let input = format!("{}\n\n{}", case.transcript, EXTRACTION_PROJECTION);
        observations.push(
            run_one(
                &runner,
                "memory-extract-eval",
                sequence,
                DEFAULT_MEMORY_INSTRUCTIONS,
                input,
                &case.id,
            )
            .await,
        );
    }
    for (sequence, case) in dataset.selection_cases.iter().enumerate() {
        let manifest = case
            .memories
            .iter()
            .enumerate()
            .map(|(index, memory)| (index, memory.clone()))
            .collect::<Vec<_>>();
        observations.push(
            run_one(
                &runner,
                "memory-select-eval",
                sequence,
                DEFAULT_SELECTOR_INSTRUCTIONS,
                select_input(&case.query, &manifest, case.max),
                &case.id,
            )
            .await,
        );
    }
    observations
}

async fn run_one(
    runner: &ToolFreeAcpRunner,
    namespace: &str,
    sequence: usize,
    instructions: &str,
    input: String,
    case_id: &str,
) -> MemoryObservation {
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
    MemoryObservation {
        case_id: case_id.into(),
        output,
        latency_ms,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_extraction_and_selection_score_independently() {
        let dataset = MemoryDataset {
            version: 1,
            name: "memory".into(),
            extraction_cases: vec![ExtractionCase {
                id: "extract".into(),
                transcript: "User prefers ALPHA; SECRET is transient".into(),
                expected_term_groups: vec![vec!["ALPHA".into()]],
                forbidden_terms: vec!["SECRET".into()],
            }],
            selection_cases: vec![SelectionCase {
                id: "select".into(),
                query: "alpha".into(),
                memories: vec!["ALPHA".into(), "BETA".into()],
                max: 1,
                expected_indices: vec![0],
            }],
        };
        dataset.validate().unwrap();
        let report = score(
            &dataset,
            &[
                MemoryObservation {
                    case_id: "extract".into(),
                    output: r#"[{"name":"preference","content":"ALPHA"}]"#.into(),
                    latency_ms: 1,
                    error: None,
                },
                MemoryObservation {
                    case_id: "select".into(),
                    output: "[0]".into(),
                    latency_ms: 1,
                    error: None,
                },
            ],
        );
        assert_eq!(report.extraction_exact, 1);
        assert_eq!(report.selection_exact, 1);
        assert_eq!(report.extraction_observed, 1);
        assert_eq!(report.selection_observed, 1);
        assert_eq!(report.extraction_errors, 0);
        assert_eq!(report.selection_errors, 0);
    }

    #[test]
    fn provider_errors_are_counted_separately_and_fail_closed() {
        let dataset = MemoryDataset {
            version: 1,
            name: "memory".into(),
            extraction_cases: Vec::new(),
            selection_cases: vec![SelectionCase {
                id: "select".into(),
                query: "alpha".into(),
                memories: vec!["ALPHA".into()],
                max: 1,
                expected_indices: vec![0],
            }],
        };
        let report = score(
            &dataset,
            &[MemoryObservation {
                case_id: "select".into(),
                output: "[0]".into(),
                latency_ms: 1,
                error: Some("quota".into()),
            }],
        );
        assert_eq!(report.selection_observed, 1);
        assert_eq!(report.selection_errors, 1);
        assert_eq!(report.selection_schema_valid, 0);
        assert_eq!(report.selection_exact, 0);
    }

    #[test]
    fn prose_wrapped_protocols_fail_closed() {
        assert_eq!(parse_selection("choose [0]", 2, 1), (false, Vec::new()));
        assert!(
            serde_json::from_str::<Vec<ProposedMemory>>(
                r#"[{"name":"x","content":"y","extra":true}]"#
            )
            .is_err()
        );
    }

    #[test]
    fn selection_exactness_is_set_based_not_output_order_based() {
        let dataset = MemoryDataset {
            version: 1,
            name: "memory".into(),
            extraction_cases: Vec::new(),
            selection_cases: vec![SelectionCase {
                id: "select".into(),
                query: "both".into(),
                memories: vec!["A".into(), "B".into()],
                max: 2,
                expected_indices: vec![0, 1],
            }],
        };
        let report = score(
            &dataset,
            &[MemoryObservation {
                case_id: "select".into(),
                output: "[1], [0]".into(),
                latency_ms: 1,
                error: None,
            }],
        );
        assert_eq!(report.selection_exact, 1);
    }

    #[test]
    fn committed_gold_dataset_covers_extraction_and_selection() {
        let dataset: MemoryDataset =
            serde_json::from_str(include_str!("../fixtures/memory-gold-v1.json")).unwrap();
        dataset.validate().unwrap();
        assert_eq!(dataset.extraction_cases.len(), 5);
        assert_eq!(dataset.selection_cases.len(), 6);
        assert!(
            dataset
                .extraction_cases
                .iter()
                .any(|case| case.id == "extract-ephemeral-and-secret")
        );
        assert!(
            dataset
                .selection_cases
                .iter()
                .any(|case| case.id == "select-untrusted-memory")
        );
    }
}
