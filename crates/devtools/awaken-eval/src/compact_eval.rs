//! Gold-corpus scoring and live ACP evaluation for the compactor Agent.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use awaken_ext_compact::{DEFAULT_COMPACT_INSTRUCTIONS, SUMMARIZE_PROMPT};
use serde::{Deserialize, Serialize};

use crate::acp_runner::ToolFreeAcpRunner;

const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactDataset {
    pub version: u32,
    pub name: String,
    pub cases: Vec<CompactCase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactCase {
    pub id: String,
    pub transcript: String,
    pub required_terms: Vec<String>,
    pub forbidden_terms: Vec<String>,
}

impl CompactDataset {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.name.trim().is_empty() || self.cases.is_empty() {
            return Err("compact dataset needs version 1, a name, and cases".into());
        }
        let mut ids = BTreeSet::new();
        for case in &self.cases {
            if case.id.trim().is_empty()
                || case.transcript.trim().is_empty()
                || case.required_terms.is_empty()
                || !ids.insert(&case.id)
            {
                return Err(format!("invalid or duplicate compact case {:?}", case.id));
            }
            if case
                .required_terms
                .iter()
                .chain(&case.forbidden_terms)
                .any(|term| term.trim().is_empty())
            {
                return Err(format!("compact case {:?} has a blank term", case.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactObservation {
    pub case_id: String,
    pub output: String,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactCaseScore {
    pub case_id: String,
    pub required_found: usize,
    pub required_total: usize,
    pub forbidden_dropped: usize,
    pub forbidden_total: usize,
    pub nonempty: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactReport {
    pub total_cases: usize,
    pub observed_cases: usize,
    pub errored_cases: usize,
    pub exact_cases: usize,
    pub required_found: usize,
    pub required_total: usize,
    pub forbidden_dropped: usize,
    pub forbidden_total: usize,
    pub duplicate_observation_ids: Vec<String>,
    pub unexpected_observation_ids: Vec<String>,
    pub cases: Vec<CompactCaseScore>,
}

#[must_use]
pub fn score(dataset: &CompactDataset, observations: &[CompactObservation]) -> CompactReport {
    let expected = dataset
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut by_id = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    let mut unexpected = BTreeSet::new();
    for observation in observations {
        if !expected.contains(observation.case_id.as_str()) {
            unexpected.insert(observation.case_id.clone());
        } else if by_id
            .insert(observation.case_id.as_str(), observation)
            .is_some()
        {
            duplicates.insert(observation.case_id.clone());
        }
    }
    let mut report = CompactReport {
        total_cases: dataset.cases.len(),
        observed_cases: 0,
        errored_cases: 0,
        exact_cases: 0,
        required_found: 0,
        required_total: 0,
        forbidden_dropped: 0,
        forbidden_total: 0,
        duplicate_observation_ids: duplicates.iter().cloned().collect(),
        unexpected_observation_ids: unexpected.into_iter().collect(),
        cases: Vec::with_capacity(dataset.cases.len()),
    };
    for case in &dataset.cases {
        if let Some(observation) = by_id.get(case.id.as_str())
            && !duplicates.contains(&case.id)
        {
            report.observed_cases += 1;
            report.errored_cases += usize::from(observation.error.is_some());
        }
        let output = by_id
            .get(case.id.as_str())
            .filter(|_| !duplicates.contains(&case.id))
            .filter(|observation| observation.error.is_none())
            .map(|observation| observation.output.to_ascii_lowercase())
            .unwrap_or_default();
        let required_found = case
            .required_terms
            .iter()
            .filter(|term| output.contains(&term.to_ascii_lowercase()))
            .count();
        let forbidden_dropped = case
            .forbidden_terms
            .iter()
            .filter(|term| !output.contains(&term.to_ascii_lowercase()))
            .count();
        let nonempty = !output.trim().is_empty();
        let passed = nonempty
            && required_found == case.required_terms.len()
            && forbidden_dropped == case.forbidden_terms.len();
        report.required_found += required_found;
        report.required_total += case.required_terms.len();
        report.forbidden_dropped += forbidden_dropped;
        report.forbidden_total += case.forbidden_terms.len();
        report.exact_cases += usize::from(passed);
        report.cases.push(CompactCaseScore {
            case_id: case.id.clone(),
            required_found,
            required_total: case.required_terms.len(),
            forbidden_dropped,
            forbidden_total: case.forbidden_terms.len(),
            nonempty,
            passed,
        });
    }
    report
}

/// Run each case independently so one malformed output cannot hide another.
pub async fn run_acp(
    dataset: &CompactDataset,
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> Vec<CompactObservation> {
    let runner = ToolFreeAcpRunner::new(argv, env);
    let mut observations = Vec::with_capacity(dataset.cases.len());
    for (sequence, case) in dataset.cases.iter().enumerate() {
        let input = format!("{}\n\n{}", case.transcript, SUMMARIZE_PROMPT);
        let started = Instant::now();
        let result = tokio::time::timeout(
            TIMEOUT,
            runner.run(
                "compact-eval",
                sequence,
                DEFAULT_COMPACT_INSTRUCTIONS,
                input,
                2,
            ),
        )
        .await;
        let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let (output, error) = match result {
            Ok(Ok(output)) => (output, None),
            Ok(Err(error)) => (String::new(), Some(error.to_string())),
            Err(_) => (
                String::new(),
                Some("ACP compact evaluation timed out".into()),
            ),
        };
        observations.push(CompactObservation {
            case_id: case.id.clone(),
            output,
            latency_ms,
            error,
        });
    }
    observations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset() -> CompactDataset {
        CompactDataset {
            version: 1,
            name: "compact".into(),
            cases: vec![CompactCase {
                id: "one".into(),
                transcript: "final ALPHA, obsolete BETA".into(),
                required_terms: vec!["ALPHA".into()],
                forbidden_terms: vec!["BETA".into()],
            }],
        }
    }

    #[test]
    fn scores_preservation_and_stale_information_independently() {
        let report = score(
            &dataset(),
            &[CompactObservation {
                case_id: "one".into(),
                output: "keep ALPHA".into(),
                latency_ms: 1,
                error: None,
            }],
        );
        assert_eq!(report.exact_cases, 1);
        assert_eq!(report.observed_cases, 1);
        assert_eq!(report.errored_cases, 0);
        assert_eq!(report.required_found, 1);
        assert_eq!(report.forbidden_dropped, 1);
    }

    #[test]
    fn provider_error_is_observed_but_never_scored_as_content() {
        let report = score(
            &dataset(),
            &[CompactObservation {
                case_id: "one".into(),
                output: "ALPHA".into(),
                latency_ms: 1,
                error: Some("quota".into()),
            }],
        );
        assert_eq!(report.observed_cases, 1);
        assert_eq!(report.errored_cases, 1);
        assert_eq!(report.required_found, 0);
        assert_eq!(report.exact_cases, 0);
    }

    #[test]
    fn duplicate_outputs_fail_closed() {
        let observation = CompactObservation {
            case_id: "one".into(),
            output: "ALPHA".into(),
            latency_ms: 1,
            error: None,
        };
        let report = score(&dataset(), &[observation.clone(), observation]);
        assert_eq!(report.exact_cases, 0);
        assert_eq!(report.duplicate_observation_ids, vec!["one"]);
    }

    #[test]
    fn committed_gold_dataset_is_valid_and_covers_boundaries() {
        let dataset: CompactDataset =
            serde_json::from_str(include_str!("../fixtures/compact-gold-v1.json")).unwrap();
        dataset.validate().unwrap();
        assert_eq!(dataset.cases.len(), 8);
        assert!(
            dataset
                .cases
                .iter()
                .any(|case| case.id == "decision-reversal")
        );
        assert!(
            dataset
                .cases
                .iter()
                .any(|case| case.id == "instruction-injection")
        );
    }

    #[test]
    fn committed_adversarial_dataset_covers_adjacent_and_distant_state() {
        let dataset: CompactDataset =
            serde_json::from_str(include_str!("../fixtures/compact-adversarial-v1.json")).unwrap();
        dataset.validate().unwrap();
        assert_eq!(dataset.cases.len(), 4);
        assert!(
            dataset
                .cases
                .iter()
                .any(|case| case.id == "completed-source-durable-consequence")
        );
        assert!(
            dataset
                .cases
                .iter()
                .any(|case| case.id == "distant-override-with-injection")
        );
    }
}
