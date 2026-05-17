//! Declarative success criteria for a fixture run.

use serde::{Deserialize, Serialize};

use crate::outcome::ReplayRuntimeFailure;

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

    /// Tool names the agent must never invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_tools: Vec<String>,

    /// Upper bound on input + output tokens summed across the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_total: Option<u32>,

    /// Upper bound on session duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,

    /// Minimum LLM-judge score in `[0.0, 1.0]`. Reserved for the optional
    /// `llm-judge` feature — the pure `score` function ignores it.
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
            && self.max_tokens_total.is_none()
            && self.max_duration_ms.is_none()
            && self.min_judge_score.is_none()
            && self.expected_error_type.is_none()
    }
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
    ToolSequenceMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    /// A tool listed in `forbidden_tools` was invoked.
    ForbiddenToolUsed { tool: String },
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
            Failure::TokenBudgetExceeded { .. } => "token_budget_exceeded",
            Failure::DurationExceeded { .. } => "duration_exceeded",
            Failure::JudgeBelowThreshold { .. } => "judge_below_threshold",
            Failure::ExpectedErrorMissing { .. } => "expected_error_missing",
            Failure::ErrorTypeMismatch { .. } => "error_type_mismatch",
            Failure::ReplayRuntimeFailure { .. } => "replay_runtime_failure",
        }
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
            forbidden_tools: vec!["delete".into()],
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
    fn expectation_with_expected_error_type_is_not_empty() {
        let e = Expectation {
            expected_error_type: Some("rate_limit".into()),
            ..Expectation::default()
        };
        assert!(!e.is_empty());
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
                },
                "tool_sequence_mismatch",
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
