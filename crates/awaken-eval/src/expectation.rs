//! Declarative success criteria for a fixture run.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::outcome::ReplayRuntimeFailure;

/// How [`Expectation::tool_sequence`] is matched against the recorded tool
/// invocations.
///
/// `Exact` (default) requires the recorded names to equal the expected list
/// element-wise. `OrderedSubseq` only requires the expected names to appear
/// as a subsequence — useful when intermediate, unrelated tool calls are
/// permitted but the *required* ones must still happen in order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSequenceMode {
    #[default]
    Exact,
    OrderedSubseq,
}

impl ToolSequenceMode {
    pub fn is_default(&self) -> bool {
        matches!(self, ToolSequenceMode::Exact)
    }
}

/// Constraint on how many times a [`ToolCallMatcher`] must match.
///
/// `Exactly(0)` is a "must never happen" constraint that complements the
/// existing [`Expectation::forbidden_tools`] (which is name-only) by adding
/// an `args_subset` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "n", rename_all = "snake_case")]
pub enum CountConstraint {
    Exactly(u32),
    AtLeast(u32),
    AtMost(u32),
}

impl CountConstraint {
    /// Returns `true` when `actual` satisfies the constraint.
    pub fn allows(self, actual: u32) -> bool {
        match self {
            CountConstraint::Exactly(n) => actual == n,
            CountConstraint::AtLeast(n) => actual >= n,
            CountConstraint::AtMost(n) => actual <= n,
        }
    }
}

/// A per-call matcher for [`Expectation::required_tool_calls`].
///
/// `name` is matched against [`awaken_ext_observability::ToolSpan::name`].
/// `args_subset`, when set, must be a structural subset of the recorded
/// `call_arguments`: every key in a JSON object must be present in the
/// recorded payload with a subset-matching value, primitives must equal
/// verbatim, and arrays must be the same length and elementwise subset-match
/// (strict to keep ordering-sensitive args unambiguous).
///
/// `count` defaults to "at least one matching call" when omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallMatcher {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_subset: Option<JsonValue>,
    /// Optional structural subset to match against
    /// [`awaken_ext_observability::ToolSpan::call_result`]. Same semantics
    /// as `args_subset`. Set this to assert the tool's *outcome* (what
    /// it returned) rather than its inputs — Anthropic Managed Agents'
    /// "grade what was produced, not the path it took" principle applied
    /// at the tool boundary. Requires the originating run to have
    /// `ToolIoCapture::Enabled`; matchers with `result_subset` against a
    /// call whose result wasn't captured count as non-matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_subset: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<CountConstraint>,
}

/// What a passing replay looks like.
///
/// `Expectation` is intentionally a flat data struct: it is loaded from JSON
/// next to a [`crate::Fixture`] and consumed by the pure [`crate::score`]
/// function. Adding a new criterion means adding a field, a [`Failure`]
/// variant, and a corresponding check in `score`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    /// Substrings that must appear in the assistant's final answer.
    /// Matching is case-sensitive; callers normalise upstream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub final_answer_contains: Vec<String>,

    /// Substrings that must NOT appear in the assistant's final answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub final_answer_excludes: Vec<String>,

    /// Tool names the agent must invoke, in order.  Empty = no constraint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_sequence: Vec<String>,

    /// How to interpret `tool_sequence`. Defaults to [`ToolSequenceMode::Exact`]
    /// so existing fixtures behave unchanged.
    #[serde(default, skip_serializing_if = "ToolSequenceMode::is_default")]
    pub tool_sequence_mode: ToolSequenceMode,

    /// Tool names the agent must never invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_tools: Vec<String>,

    /// Per-call matchers covering tool name + optional args-subset + optional
    /// count constraint. Independent of `tool_sequence` (which only checks
    /// names). Empty = no constraint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_tool_calls: Vec<ToolCallMatcher>,

    /// Upper bound on input + output tokens summed across the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_total: Option<u32>,

    /// Upper bound on session duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,

    /// Minimum LLM-judge score in `[0.0, 1.0]`.
    ///
    /// The pure deterministic [`crate::score`] function ignores this
    /// field; offline CLI replay/curation rejects it, and server
    /// endpoints require Live mode with judge config and explicit rubric
    /// so the criterion is never silently skipped or graded against a
    /// vague default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_judge_score: Option<f32>,

    /// Fixture-author-supplied `error_type` the run must surface.
    /// Matched verbatim against [`crate::ReplayOutcome::error_type`].
    /// Mostly used by failure-path fixtures (e.g. `rate_limit`) so that a
    /// silently-swallowed error doesn't get away with an empty `final_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_error_type: Option<String>,
}

impl Expectation {
    /// Returns `true` when no criterion is set; useful for sanity-checking
    /// hand-authored fixture files.
    pub fn is_empty(&self) -> bool {
        self.final_answer_contains.is_empty()
            && self.final_answer_excludes.is_empty()
            && self.tool_sequence.is_empty()
            && self.forbidden_tools.is_empty()
            && self.required_tool_calls.is_empty()
            && self.max_tokens_total.is_none()
            && self.max_duration_ms.is_none()
            && self.min_judge_score.is_none()
            && self.expected_error_type.is_none()
    }
}

/// Validate the numeric range promised by [`Expectation::min_judge_score`].
///
/// JSON and Rust callers can both construct values outside the documented
/// `[0.0, 1.0]` interval. Since judge implementations clamp their result into
/// that same interval, accepting a threshold above `1.0` creates an impossible
/// fixture and accepting a threshold below `0.0` creates an always-pass one.
/// Validate at every ingestion boundary instead of letting scoring semantics
/// depend on malformed data.
pub fn validate_min_judge_score(expect: &Expectation, label: &str) -> Result<(), String> {
    let Some(threshold) = expect.min_judge_score else {
        return Ok(());
    };
    if threshold.is_finite() && (0.0..=1.0).contains(&threshold) {
        return Ok(());
    }
    Err(format!(
        "{label} sets expect.min_judge_score={threshold}; value must be finite and within [0.0, 1.0]"
    ))
}

/// Validate an expectation before it enters the offline CLI replay path.
///
/// `awaken-eval replay` intentionally has no provider/judge configuration:
/// it runs deterministic fixture scripts through the pure scorer. Therefore a
/// fixture that asks for `min_judge_score` cannot be evaluated correctly in the
/// CLI and must fail closed instead of being silently green.
pub fn validate_offline_expectation(expect: &Expectation, label: &str) -> Result<(), String> {
    validate_min_judge_score(expect, label)?;
    if expect.min_judge_score.is_some() {
        return Err(format!(
            "{label} sets expect.min_judge_score; offline awaken-eval replay/curate cannot run an LLM judge — use server live eval with `judge.rubric`, or remove the judge-only criterion"
        ));
    }
    Ok(())
}

/// A specific way a replay deviated from its expectation.
///
/// Each variant carries enough context to be human-readable in the NDJSON
/// report without requiring the original fixture to be re-loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    /// A required substring was absent from the assistant answer.
    AnswerMissingPhrase { phrase: String },
    /// A forbidden substring appeared in the assistant answer.
    AnswerContainsExcludedPhrase { phrase: String },
    /// The recorded tool sequence does not match the expected order.
    /// `mode` records which match strategy was used; defaults to `exact` for
    /// legacy NDJSON lines without the field.
    ToolSequenceMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
        #[serde(default, skip_serializing_if = "ToolSequenceMode::is_default")]
        mode: ToolSequenceMode,
    },
    /// A tool listed in `forbidden_tools` was invoked.
    ForbiddenToolUsed { tool: String },
    /// A [`ToolCallMatcher`] in `required_tool_calls` did not find a
    /// satisfying invocation. `actual_matches` counts the calls that
    /// matched `name` AND every set sub-filter (`args_subset` /
    /// `result_subset`), letting diffs see which filter failed.
    RequiredToolCallNotSatisfied {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_count: Option<CountConstraint>,
        actual_matches: u32,
        #[serde(default, skip_serializing_if = "is_false")]
        args_filter_set: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        result_filter_set: bool,
    },
    /// The combined token count exceeded the budget.
    TokenBudgetExceeded { budget: u32, actual: u32 },
    /// The recorded session duration exceeded the budget.
    DurationExceeded { budget_ms: u64, actual_ms: u64 },
    /// The judge score fell below the configured threshold.
    /// (Emitted only by the `llm-judge` feature.)
    JudgeBelowThreshold { threshold: f32, actual: f32 },
    /// `expected_error_type` was set but the run did not raise an inference
    /// error (i.e. `ReplayOutcome::error_type` was `None`).
    ExpectedErrorMissing { expected: String },
    /// `expected_error_type` was set and an error did fire, but its
    /// `error_type` did not match.
    ErrorTypeMismatch { expected: String, actual: String },
    /// The replay itself misbehaved: the runtime over-called the
    /// scripted executor, left scripted events unused, or returned a
    /// non-scripted error. Promoted from
    /// [`crate::ReplayOutcome::runtime_failure`] so the NDJSON report
    /// stays complete instead of the replayer aborting the batch.
    ReplayRuntimeFailure { failure: ReplayRuntimeFailure },
}

impl Failure {
    /// Stable string discriminator used for grouping in reports.
    pub fn kind(&self) -> &'static str {
        match self {
            Failure::AnswerMissingPhrase { .. } => "answer_missing_phrase",
            Failure::AnswerContainsExcludedPhrase { .. } => "answer_contains_excluded_phrase",
            Failure::ToolSequenceMismatch { .. } => "tool_sequence_mismatch",
            Failure::ForbiddenToolUsed { .. } => "forbidden_tool_used",
            Failure::RequiredToolCallNotSatisfied { .. } => "required_tool_call_not_satisfied",
            Failure::TokenBudgetExceeded { .. } => "token_budget_exceeded",
            Failure::DurationExceeded { .. } => "duration_exceeded",
            Failure::JudgeBelowThreshold { .. } => "judge_below_threshold",
            Failure::ExpectedErrorMissing { .. } => "expected_error_missing",
            Failure::ErrorTypeMismatch { .. } => "error_type_mismatch",
            Failure::ReplayRuntimeFailure { .. } => "replay_runtime_failure",
        }
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Recursive structural subset: every leaf in `subset` must equal the
/// corresponding leaf in `actual`. Objects are subset-matched key-by-key;
/// arrays must be the same length with element-wise subset matching;
/// primitives must equal. Returns `false` when `actual` is `Null` and
/// `subset` is not.
pub(crate) fn json_subset(subset: &JsonValue, actual: &JsonValue) -> bool {
    match (subset, actual) {
        (JsonValue::Object(s), JsonValue::Object(a)) => s
            .iter()
            .all(|(k, sv)| a.get(k).map(|av| json_subset(sv, av)).unwrap_or(false)),
        (JsonValue::Array(s), JsonValue::Array(a)) => {
            s.len() == a.len() && s.iter().zip(a).all(|(sv, av)| json_subset(sv, av))
        }
        (s, a) => s == a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_expectation_is_empty() {
        assert!(Expectation::default().is_empty());
    }

    #[test]
    fn expectation_with_phrase_is_not_empty() {
        let e = Expectation {
            final_answer_contains: vec!["banana".into()],
            ..Expectation::default()
        };
        assert!(!e.is_empty());
    }

    #[test]
    fn expectation_serde_roundtrip_preserves_fields() {
        let e = Expectation {
            final_answer_contains: vec!["alpha".into(), "beta".into()],
            final_answer_excludes: vec!["secret".into()],
            tool_sequence: vec!["search".into(), "write".into()],
            tool_sequence_mode: ToolSequenceMode::OrderedSubseq,
            forbidden_tools: vec!["delete".into()],
            required_tool_calls: vec![ToolCallMatcher {
                name: "search".into(),
                args_subset: Some(serde_json::json!({"query": "x"})),
                result_subset: None,
                count: Some(CountConstraint::AtLeast(1)),
            }],
            max_tokens_total: Some(5000),
            max_duration_ms: Some(10_000),
            min_judge_score: Some(0.7),
            expected_error_type: Some("rate_limit".into()),
        };
        let json = serde_json::to_string(&e).unwrap();
        let parsed: Expectation = serde_json::from_str(&json).unwrap();
        assert_eq!(e, parsed);
    }

    #[test]
    fn tool_sequence_mode_omitted_in_default_serialised_form() {
        let e = Expectation {
            tool_sequence: vec!["a".into()],
            ..Expectation::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("tool_sequence_mode"));
    }

    #[test]
    fn tool_sequence_mode_emitted_when_non_default() {
        let e = Expectation {
            tool_sequence: vec!["a".into()],
            tool_sequence_mode: ToolSequenceMode::OrderedSubseq,
            ..Expectation::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains(r#""tool_sequence_mode":"ordered_subseq""#));
    }

    #[test]
    fn count_constraint_allows_matches_semantics() {
        assert!(CountConstraint::Exactly(2).allows(2));
        assert!(!CountConstraint::Exactly(2).allows(1));
        assert!(CountConstraint::AtLeast(2).allows(5));
        assert!(!CountConstraint::AtLeast(2).allows(1));
        assert!(CountConstraint::AtMost(2).allows(2));
        assert!(!CountConstraint::AtMost(2).allows(3));
    }

    #[test]
    fn json_subset_object_subset_matches() {
        let actual = serde_json::json!({"a": 1, "b": 2, "c": {"d": 3, "e": 4}});
        let subset = serde_json::json!({"a": 1, "c": {"d": 3}});
        assert!(json_subset(&subset, &actual));
    }

    #[test]
    fn json_subset_object_missing_key_fails() {
        let actual = serde_json::json!({"a": 1});
        let subset = serde_json::json!({"b": 2});
        assert!(!json_subset(&subset, &actual));
    }

    #[test]
    fn json_subset_array_requires_equal_length_and_element_match() {
        let actual = serde_json::json!([1, 2, 3]);
        assert!(json_subset(&serde_json::json!([1, 2, 3]), &actual));
        assert!(!json_subset(&serde_json::json!([1, 2]), &actual));
        assert!(!json_subset(&serde_json::json!([1, 2, 4]), &actual));
    }

    #[test]
    fn json_subset_primitive_value_mismatch_fails() {
        assert!(!json_subset(
            &serde_json::json!({"x": "a"}),
            &serde_json::json!({"x": "b"}),
        ));
    }

    #[test]
    fn expectation_with_required_tool_calls_is_not_empty() {
        let e = Expectation {
            required_tool_calls: vec![ToolCallMatcher {
                name: "x".into(),
                args_subset: None,
                result_subset: None,
                count: None,
            }],
            ..Expectation::default()
        };
        assert!(!e.is_empty());
    }

    #[test]
    fn expectation_with_expected_error_type_is_not_empty() {
        let e = Expectation {
            expected_error_type: Some("rate_limit".into()),
            ..Expectation::default()
        };
        assert!(!e.is_empty());
    }

    #[test]
    fn min_judge_score_validation_accepts_closed_unit_interval() {
        for threshold in [0.0, 0.5, 1.0] {
            let e = Expectation {
                min_judge_score: Some(threshold),
                ..Expectation::default()
            };
            validate_min_judge_score(&e, "fixture").unwrap();
        }
    }

    #[test]
    fn min_judge_score_validation_rejects_out_of_range_and_non_finite() {
        for threshold in [-0.1, 1.01, f32::NAN, f32::INFINITY] {
            let e = Expectation {
                min_judge_score: Some(threshold),
                ..Expectation::default()
            };
            let err = validate_min_judge_score(&e, "fixture").unwrap_err();
            assert!(err.contains("[0.0, 1.0]"), "err: {err}");
        }
    }

    #[test]
    fn offline_validation_rejects_judge_only_expectation() {
        let e = Expectation {
            min_judge_score: Some(0.8),
            ..Expectation::default()
        };
        let err = validate_offline_expectation(&e, "fixture").unwrap_err();
        assert!(err.contains("offline awaken-eval"), "err: {err}");
    }

    #[test]
    fn expectation_serde_skips_empty_fields() {
        let e = Expectation {
            final_answer_contains: vec!["x".into()],
            ..Expectation::default()
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("final_answer_contains"));
        // Other empty/None fields should not be emitted.
        assert!(!json.contains("forbidden_tools"));
        assert!(!json.contains("max_tokens_total"));
        assert!(!json.contains("min_judge_score"));
    }

    #[test]
    fn expectation_deserializes_from_minimal_json() {
        let e: Expectation = serde_json::from_str(r#"{"final_answer_contains": ["hi"]}"#).unwrap();
        assert_eq!(e.final_answer_contains, vec!["hi".to_string()]);
        assert!(e.tool_sequence.is_empty());
        assert!(e.max_tokens_total.is_none());
    }

    #[test]
    fn expectation_deserializes_from_empty_object() {
        let e: Expectation = serde_json::from_str("{}").unwrap();
        assert!(e.is_empty());
    }

    // ── Failure ─────────────────────────────────────────────────────

    #[test]
    fn failure_kind_strings_are_stable() {
        let cases = [
            (
                Failure::AnswerMissingPhrase { phrase: "x".into() },
                "answer_missing_phrase",
            ),
            (
                Failure::AnswerContainsExcludedPhrase { phrase: "x".into() },
                "answer_contains_excluded_phrase",
            ),
            (
                Failure::ToolSequenceMismatch {
                    expected: vec!["a".into()],
                    actual: vec![],
                    mode: ToolSequenceMode::Exact,
                },
                "tool_sequence_mismatch",
            ),
            (
                Failure::RequiredToolCallNotSatisfied {
                    name: "search".into(),
                    expected_count: Some(CountConstraint::AtLeast(1)),
                    actual_matches: 0,
                    args_filter_set: true,
                    result_filter_set: false,
                },
                "required_tool_call_not_satisfied",
            ),
            (
                Failure::ForbiddenToolUsed { tool: "rm".into() },
                "forbidden_tool_used",
            ),
            (
                Failure::TokenBudgetExceeded {
                    budget: 100,
                    actual: 200,
                },
                "token_budget_exceeded",
            ),
            (
                Failure::DurationExceeded {
                    budget_ms: 100,
                    actual_ms: 200,
                },
                "duration_exceeded",
            ),
            (
                Failure::JudgeBelowThreshold {
                    threshold: 0.7,
                    actual: 0.4,
                },
                "judge_below_threshold",
            ),
            (
                Failure::ExpectedErrorMissing {
                    expected: "rate_limit".into(),
                },
                "expected_error_missing",
            ),
            (
                Failure::ErrorTypeMismatch {
                    expected: "rate_limit".into(),
                    actual: "timeout".into(),
                },
                "error_type_mismatch",
            ),
            (
                Failure::ReplayRuntimeFailure {
                    failure: ReplayRuntimeFailure::ScriptExhausted { extra_calls: 1 },
                },
                "replay_runtime_failure",
            ),
        ];
        for (f, k) in cases {
            assert_eq!(f.kind(), k);
        }
    }

    #[test]
    fn failure_serde_uses_kind_tag() {
        let f = Failure::TokenBudgetExceeded {
            budget: 100,
            actual: 200,
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""kind":"token_budget_exceeded""#));
        let parsed: Failure = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, f);
    }
}
