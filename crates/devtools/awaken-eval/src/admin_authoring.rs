//! Full-server evaluation for the Admin Assistant's Agent-authoring behavior.
//!
//! The server remains the execution authority: this module submits a user turn
//! and observes the persisted Agent configuration.  The dataset, observations,
//! deterministic scorer, and release floors live in `awaken-eval`, so there is
//! no parallel Python harness or second source of golden cases.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A versioned set of synthetic Agent-authoring cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAuthoringDataset {
    pub schema_version: u32,
    pub name: String,
    pub cases: Vec<AdminAuthoringCase>,
}

impl AdminAuthoringDataset {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported Admin authoring dataset schema version {}",
                self.schema_version
            ));
        }
        if self.name.trim().is_empty() || self.cases.is_empty() {
            return Err("dataset name and cases must be non-empty".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for case in &self.cases {
            if case.id.trim().is_empty()
                || case.prompt.trim().is_empty()
                || case.criteria.is_empty()
            {
                return Err(format!(
                    "case {:?} needs a non-empty id, prompt, and criteria",
                    case.id
                ));
            }
            if !ids.insert(&case.id) {
                return Err(format!("duplicate case id {:?}", case.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAuthoringCase {
    pub id: String,
    pub prompt: String,
    pub criteria: Vec<AdminCriterion>,
}

/// Closed, deterministic checks over the persisted Agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminCriterion {
    Persisted,
    HasTool { tool_id: String },
    ToolOverrideAlias { target: String, alias: String },
    ToolOverrideHasDescription { target: String },
    PermissionGated,
    PermissionUsesRealLowercaseToolIds,
    PermissionDeniesToolTerm { tool_id: String, term: String },
    HasPlugin { plugin_id: String },
    StateMachineHasTransitions,
    StateMachineHasReminder,
    CompactHasInstructions,
    CompactInstructionsAreProse,
    CompactTriggerRatioSet,
    InstructionsMinLength { min: usize },
    ToolsEmpty,
}

impl AdminCriterion {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Persisted => "persisted".into(),
            Self::HasTool { tool_id } => format!("tool:{tool_id}"),
            Self::ToolOverrideAlias { target, alias } => {
                format!("override:{target}->{alias}")
            }
            Self::ToolOverrideHasDescription { target } => {
                format!("override:{target}:has_description")
            }
            Self::PermissionGated => "permission:gated".into(),
            Self::PermissionUsesRealLowercaseToolIds => "permission:real_lowercase_tool_ids".into(),
            Self::PermissionDeniesToolTerm { tool_id, term } => {
                format!("permission:denies:{tool_id}:{term}")
            }
            Self::HasPlugin { plugin_id } => format!("plugin:{plugin_id}"),
            Self::StateMachineHasTransitions => "state_machine:has_transitions".into(),
            Self::StateMachineHasReminder => "state_machine:has_reminder".into(),
            Self::CompactHasInstructions => "compact:has_instructions".into(),
            Self::CompactInstructionsAreProse => "compact:instructions_are_prose".into(),
            Self::CompactTriggerRatioSet => "compact:trigger_ratio_set".into(),
            Self::InstructionsMinLength { min } => format!("instructions:min_length:{min}"),
            Self::ToolsEmpty => "tools:empty".into(),
        }
    }

    #[must_use]
    pub fn evaluate(&self, config: Option<&Value>) -> bool {
        let Some(config) = config else {
            return false;
        };
        match self {
            Self::Persisted => true,
            Self::HasTool { tool_id } => string_array(config, "tools")
                .iter()
                .any(|candidate| *candidate == tool_id),
            Self::ToolOverrideAlias { target, alias } => {
                tool_override(config, target)
                    .and_then(|value| value.get("alias"))
                    .and_then(Value::as_str)
                    == Some(alias)
            }
            Self::ToolOverrideHasDescription { target } => tool_override(config, target)
                .and_then(|value| value.get("description"))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty()),
            Self::PermissionGated => permission(config).is_some_and(|permission| {
                matches!(
                    permission.get("default_behavior").and_then(Value::as_str),
                    Some("ask" | "deny")
                ) || rules(permission).any(|rule| {
                    matches!(
                        rule.get("behavior").and_then(Value::as_str),
                        Some("ask" | "deny")
                    )
                })
            }),
            Self::PermissionUsesRealLowercaseToolIds => permission(config).is_some_and(|value| {
                let heads = rules(value)
                    .filter_map(|rule| rule.get("pattern").and_then(Value::as_str))
                    .map(pattern_head)
                    .filter(|head| *head != "*" && !head.starts_with("mcp__"))
                    .collect::<Vec<_>>();
                !heads.is_empty()
                    && heads.iter().all(|head| {
                        ["bash", "read", "write", "edit", "glob", "grep"].contains(head)
                    })
            }),
            Self::PermissionDeniesToolTerm { tool_id, term } => {
                permission(config).is_some_and(|value| {
                    rules(value).any(|rule| {
                        rule.get("behavior").and_then(Value::as_str) == Some("deny")
                            && rule
                                .get("pattern")
                                .and_then(Value::as_str)
                                .is_some_and(|pattern| {
                                    pattern_head(pattern) == tool_id && pattern.contains(term)
                                })
                    })
                })
            }
            Self::HasPlugin { plugin_id } => string_array(config, "plugins")
                .iter()
                .any(|candidate| *candidate == plugin_id),
            Self::StateMachineHasTransitions => {
                plugin_config(config, "state_machine").is_some_and(has_transitions)
            }
            Self::StateMachineHasReminder => {
                plugin_config(config, "state_machine").is_some_and(has_reminder)
            }
            Self::CompactHasInstructions => compact_instructions(config).is_some(),
            Self::CompactInstructionsAreProse => {
                compact_instructions(config).is_some_and(|value| {
                    value.len() > 20 && value.contains(' ') && value.parse::<u64>().is_err()
                })
            }
            Self::CompactTriggerRatioSet => plugin_config(config, "compact")
                .and_then(|value| value.get("trigger_ratio"))
                .and_then(Value::as_f64)
                .is_some_and(|ratio| ratio > 0.0 && ratio <= 1.0),
            Self::InstructionsMinLength { min } => config
                .get("system")
                .or_else(|| config.get("instructions"))
                .and_then(Value::as_str)
                .is_some_and(|value| value.len() > *min),
            Self::ToolsEmpty => config
                .get("tools")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAuthoringObservation {
    pub case_id: String,
    pub repetition: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AdminAuthoringFloors {
    pub criteria: f64,
    pub fully_correct: f64,
    pub persisted: f64,
}

impl Default for AdminAuthoringFloors {
    fn default() -> Self {
        Self {
            criteria: 0.90,
            fully_correct: 0.80,
            persisted: 1.0,
        }
    }
}

impl AdminAuthoringFloors {
    pub fn validate(self) -> Result<Self, String> {
        if [self.criteria, self.fully_correct, self.persisted]
            .iter()
            .all(|value| (0.0..=1.0).contains(value))
        {
            Ok(self)
        } else {
            Err("quality floors must be between zero and one".into())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminCriterionScore {
    pub label: String,
    pub passed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminCaseScore {
    pub case_id: String,
    pub criteria: Vec<AdminCriterionScore>,
    pub fully_correct: usize,
    pub observations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminAuthoringReport {
    pub dataset: String,
    pub cases: Vec<AdminCaseScore>,
    pub criteria_passed: usize,
    pub criteria_total: usize,
    pub fully_correct: usize,
    pub observations: usize,
    pub persisted: usize,
    pub unexpected_observations: Vec<String>,
    pub observation_contract_failures: Vec<String>,
    pub floor_failures: Vec<String>,
}

impl AdminAuthoringReport {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.floor_failures.is_empty()
            && self.unexpected_observations.is_empty()
            && self.observation_contract_failures.is_empty()
    }
}

#[must_use]
pub fn score(
    dataset: &AdminAuthoringDataset,
    observations: &[AdminAuthoringObservation],
    floors: AdminAuthoringFloors,
) -> AdminAuthoringReport {
    let by_id = dataset
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect::<BTreeMap<_, _>>();
    let mut grouped = BTreeMap::<&str, Vec<&AdminAuthoringObservation>>::new();
    let mut unexpected = Vec::new();
    let mut observation_contract_failures = Vec::new();
    let mut observation_keys = std::collections::BTreeSet::new();
    for observation in observations {
        if by_id.contains_key(observation.case_id.as_str()) {
            if !observation_keys.insert((observation.case_id.as_str(), observation.repetition)) {
                observation_contract_failures.push(format!(
                    "duplicate observation {}#{}",
                    observation.case_id, observation.repetition
                ));
                continue;
            }
            grouped
                .entry(observation.case_id.as_str())
                .or_default()
                .push(observation);
        } else {
            unexpected.push(observation.case_id.clone());
        }
    }

    let mut cases = Vec::with_capacity(dataset.cases.len());
    let mut criteria_passed = 0;
    let mut criteria_total = 0;
    let mut fully_correct = 0;
    let mut persisted = 0;
    let mut expected_repetitions = None;
    for case in &dataset.cases {
        let observed = grouped.remove(case.id.as_str()).unwrap_or_default();
        if observed.is_empty() {
            observation_contract_failures.push(format!("missing observations for {}", case.id));
        } else {
            let repetitions = observed
                .iter()
                .map(|observation| observation.repetition)
                .collect::<std::collections::BTreeSet<_>>();
            let contiguous = repetitions.iter().copied().eq(0..repetitions.len());
            if !contiguous {
                observation_contract_failures
                    .push(format!("non-contiguous repetitions for {}", case.id));
            }
            match expected_repetitions {
                None => expected_repetitions = Some(repetitions.len()),
                Some(expected) if expected != repetitions.len() => {
                    observation_contract_failures.push(format!(
                        "repetition count for {} is {}, expected {}",
                        case.id,
                        repetitions.len(),
                        expected
                    ));
                }
                Some(_) => {}
            }
        }
        let mut criterion_scores = Vec::with_capacity(case.criteria.len());
        for criterion in &case.criteria {
            let passed = observed
                .iter()
                .filter(|observation| criterion.evaluate(observation.config.as_ref()))
                .count();
            criteria_passed += passed;
            criteria_total += observed.len();
            criterion_scores.push(AdminCriterionScore {
                label: criterion.label(),
                passed,
                total: observed.len(),
            });
        }
        let full = observed
            .iter()
            .filter(|observation| {
                case.criteria
                    .iter()
                    .all(|criterion| criterion.evaluate(observation.config.as_ref()))
            })
            .count();
        fully_correct += full;
        persisted += observed
            .iter()
            .filter(|observation| observation.config.is_some())
            .count();
        cases.push(AdminCaseScore {
            case_id: case.id.clone(),
            criteria: criterion_scores,
            fully_correct: full,
            observations: observed.len(),
        });
    }
    let total = observations.len().saturating_sub(unexpected.len());
    let rates = [
        (
            "criteria",
            ratio(criteria_passed, criteria_total),
            floors.criteria,
        ),
        (
            "fully-correct",
            ratio(fully_correct, total),
            floors.fully_correct,
        ),
        ("persisted", ratio(persisted, total), floors.persisted),
    ];
    let floor_failures = rates
        .into_iter()
        .filter(|(_, actual, floor)| actual < floor)
        .map(|(name, actual, floor)| {
            format!("{name} rate {actual:.3} is below required {floor:.3}")
        })
        .collect();
    unexpected.sort();
    unexpected.dedup();
    AdminAuthoringReport {
        dataset: dataset.name.clone(),
        cases,
        criteria_passed,
        criteria_total,
        fully_correct,
        observations: total,
        persisted,
        unexpected_observations: unexpected,
        observation_contract_failures,
        floor_failures,
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Run every selected synthetic case through the public server boundary.
pub async fn run_live(
    dataset: &AdminAuthoringDataset,
    base_url: &str,
    repetitions: usize,
    selected: Option<&std::collections::BTreeSet<String>>,
) -> Vec<AdminAuthoringObservation> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .expect("static HTTP client configuration is valid");
    let base_url = base_url.trim_end_matches('/');
    let mut observations = Vec::new();
    for case in dataset
        .cases
        .iter()
        .filter(|case| selected.is_none_or(|ids| ids.contains(&case.id)))
    {
        for repetition in 0..repetitions {
            let result = draft(&client, base_url, case).await;
            let (config, error) = match result {
                Ok(config) => (config, None),
                Err(error) => (None, Some(error)),
            };
            observations.push(AdminAuthoringObservation {
                case_id: case.id.clone(),
                repetition,
                config,
                error,
            });
        }
    }
    observations
}

async fn draft(
    client: &reqwest::Client,
    base_url: &str,
    case: &AdminAuthoringCase,
) -> Result<Option<Value>, String> {
    let _ = request_json(
        client,
        reqwest::Method::DELETE,
        &format!("{base_url}/v1/config/agents/{}", case.id),
        None,
    )
    .await;
    let session = request_json(
        client,
        reqwest::Method::POST,
        &format!("{base_url}/v1/sessions"),
        Some(json!({"agent": "__admin_assistant", "title": "eval"})),
    )
    .await?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "session create response has no id".to_string())?;
    request_json(
        client,
        reqwest::Method::POST,
        &format!("{base_url}/v1/sessions/{session_id}/events"),
        Some(json!({"events": [{"type": "user.message", "content": [{"type": "text", "text": case.prompt}]}]})),
    )
    .await?;

    for _ in 0..45 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let events = request_json(
            client,
            reqwest::Method::GET,
            &format!("{base_url}/v1/sessions/{session_id}/events"),
            None,
        )
        .await?;
        let types = events
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|event| event.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if types.contains(&"session.error") {
            return Err("Admin Assistant session ended with session.error".into());
        }
        if types.last() == Some(&"session.status_idle") {
            let config = request_json(
                client,
                reqwest::Method::GET,
                &format!("{base_url}/v1/config/agents/{}", case.id),
                None,
            )
            .await?;
            return Ok(
                (config.get("id").and_then(Value::as_str) == Some(case.id.as_str()))
                    .then_some(config),
            );
        }
    }
    Err("Admin Assistant session did not become idle within 90 seconds".into())
}

async fn request_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let mut last_error = String::new();
    for attempt in 0..4 {
        let mut request = client.request(method.clone(), url);
        if let Some(body) = &body {
            request = request.json(body);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| format!("read {url}: {error}"))?;
                return if bytes.is_empty() {
                    Ok(Value::Null)
                } else {
                    serde_json::from_slice(&bytes).map_err(|error| format!("parse {url}: {error}"))
                };
            }
            Ok(response) => {
                return Err(format!(
                    "{method} {url} returned HTTP {}",
                    response.status()
                ));
            }
            Err(error) => last_error = error.to_string(),
        }
        if attempt < 3 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    Err(format!("{method} {url} failed after retries: {last_error}"))
}

fn string_array<'a>(config: &'a Value, key: &str) -> Vec<&'a str> {
    config
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn tool_override<'a>(config: &'a Value, target: &str) -> Option<&'a Value> {
    config
        .get("tool_overrides")?
        .as_array()?
        .iter()
        .find(|value| value.get("target").and_then(Value::as_str) == Some(target))
}

fn plugin_config<'a>(config: &'a Value, plugin_id: &str) -> Option<&'a Value> {
    config.get("plugin_config")?.get(plugin_id)
}

fn permission(config: &Value) -> Option<&Value> {
    plugin_config(config, "permission")
}

fn rules(value: &Value) -> impl Iterator<Item = &Value> {
    value
        .get("rules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn pattern_head(pattern: &str) -> &str {
    pattern.split('(').next().unwrap_or(pattern).trim()
}

fn has_transitions(value: &Value) -> bool {
    value
        .get("transitions")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty())
        || value
            .get("machines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(has_transitions)
}

fn has_reminder(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("on_violation").is_some_and(|violation| {
                violation.get("action").and_then(Value::as_str) == Some("warn")
                    && violation
                        .get("reason")
                        .and_then(Value::as_str)
                        .is_some_and(|reason| !reason.trim().is_empty())
            }) || object.get("emit").is_some_and(nonempty_json)
                || ["message", "reminder"].iter().any(|key| {
                    object
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
                })
                || object.values().any(has_reminder)
        }
        Value::Array(values) => values.iter().any(has_reminder),
        _ => false,
    }
}

fn nonempty_json(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        Value::Bool(value) => *value,
        Value::Number(_) => true,
    }
}

fn compact_instructions(config: &Value) -> Option<&str> {
    let compact = plugin_config(config, "compact")?;
    compact
        .get("instructions")
        .or_else(|| compact.get("prompt"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_criteria_preserve_the_old_golden_checks() {
        let config = json!({
            "id": "eval-c2",
            "tools": ["bash", "read"],
            "plugins": ["permission", "state_machine", "compact"],
            "plugin_config": {
                "permission": {"rules": [{"pattern": "bash(command ~ '*rm*')", "behavior": "deny"}]},
                "state_machine": {"transitions": [{"on_violation": {"action": "warn", "reason": "read first"}}]},
                "compact": {"instructions": "Keep key findings and all open questions.", "trigger_ratio": 0.8}
            }
        });
        for criterion in [
            AdminCriterion::PermissionGated,
            AdminCriterion::PermissionUsesRealLowercaseToolIds,
            AdminCriterion::PermissionDeniesToolTerm {
                tool_id: "bash".into(),
                term: "rm".into(),
            },
            AdminCriterion::StateMachineHasTransitions,
            AdminCriterion::StateMachineHasReminder,
            AdminCriterion::CompactHasInstructions,
            AdminCriterion::CompactInstructionsAreProse,
            AdminCriterion::CompactTriggerRatioSet,
        ] {
            assert!(criterion.evaluate(Some(&config)), "{}", criterion.label());
        }
    }

    #[test]
    fn scoring_fails_closed_at_the_same_default_floors() {
        let dataset = AdminAuthoringDataset {
            schema_version: 1,
            name: "admin".into(),
            cases: vec![AdminAuthoringCase {
                id: "c1".into(),
                prompt: "draft".into(),
                criteria: vec![AdminCriterion::Persisted, AdminCriterion::ToolsEmpty],
            }],
        };
        let passing = vec![AdminAuthoringObservation {
            case_id: "c1".into(),
            repetition: 0,
            config: Some(json!({"tools": []})),
            error: None,
        }];
        assert!(score(&dataset, &passing, AdminAuthoringFloors::default()).passed());
        let failing = vec![AdminAuthoringObservation {
            case_id: "c1".into(),
            repetition: 0,
            config: None,
            error: Some("failed".into()),
        }];
        assert!(!score(&dataset, &failing, AdminAuthoringFloors::default()).passed());
    }

    #[test]
    fn committed_admin_fixture_is_valid_and_complete() {
        let dataset: AdminAuthoringDataset =
            serde_json::from_str(include_str!("../fixtures/admin-authoring-gold-v1.json")).unwrap();
        dataset.validate().unwrap();
        assert_eq!(dataset.cases.len(), 5);
    }

    #[test]
    fn duplicate_or_partial_observations_cannot_pass_the_gate() {
        let dataset = AdminAuthoringDataset {
            schema_version: 1,
            name: "admin".into(),
            cases: vec![
                AdminAuthoringCase {
                    id: "c1".into(),
                    prompt: "draft one".into(),
                    criteria: vec![AdminCriterion::Persisted],
                },
                AdminAuthoringCase {
                    id: "c2".into(),
                    prompt: "draft two".into(),
                    criteria: vec![AdminCriterion::Persisted],
                },
            ],
        };
        let observation = AdminAuthoringObservation {
            case_id: "c1".into(),
            repetition: 0,
            config: Some(json!({})),
            error: None,
        };
        let report = score(
            &dataset,
            &[observation.clone(), observation],
            AdminAuthoringFloors::default(),
        );
        assert!(!report.passed());
        assert_eq!(report.observation_contract_failures.len(), 2);
    }
}
