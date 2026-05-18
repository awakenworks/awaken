//! Pure scoring: compare a [`ReplayOutcome`] against an [`Expectation`].
//!
//! The function performs no I/O and never panics. It enumerates failures in
//! a stable order so reports diff cleanly across runs.

use crate::expectation::{Expectation, Failure, ToolCallMatcher, ToolSequenceMode, json_subset};
use crate::outcome::ReplayOutcome;

/// Score `outcome` against `expect`, returning the (possibly empty) list of
/// reasons the run did not meet expectations.
///
/// An empty result means "all configured criteria passed". An empty
/// expectation (no criteria set) always returns `vec![]`.
pub fn score(outcome: &ReplayOutcome, expect: &Expectation) -> Vec<Failure> {
    let mut failures = Vec::new();

    // Required substrings.
    for phrase in &expect.final_answer_contains {
        if !outcome.final_text.contains(phrase) {
            failures.push(Failure::AnswerMissingPhrase {
                phrase: phrase.clone(),
            });
        }
    }

    // Forbidden substrings.
    for phrase in &expect.final_answer_excludes {
        if outcome.final_text.contains(phrase) {
            failures.push(Failure::AnswerContainsExcludedPhrase {
                phrase: phrase.clone(),
            });
        }
    }

    // Tool sequence: configurable match mode (Exact | OrderedSubseq).
    let actual_tools = outcome.tool_sequence();
    if !expect.tool_sequence.is_empty() {
        let matched = match expect.tool_sequence_mode {
            ToolSequenceMode::Exact => actual_tools == expect.tool_sequence,
            ToolSequenceMode::OrderedSubseq => {
                is_ordered_subseq(&expect.tool_sequence, &actual_tools)
            }
        };
        if !matched {
            failures.push(Failure::ToolSequenceMismatch {
                expected: expect.tool_sequence.clone(),
                actual: actual_tools.clone(),
                mode: expect.tool_sequence_mode,
            });
        }
    }

    // Forbidden tools.
    for forbidden in &expect.forbidden_tools {
        if actual_tools.iter().any(|t| t == forbidden) {
            failures.push(Failure::ForbiddenToolUsed {
                tool: forbidden.clone(),
            });
        }
    }

    // Per-call matchers (name + optional args_subset + optional count).
    for matcher in &expect.required_tool_calls {
        let actual_matches = count_matching_calls(matcher, outcome);
        let constraint = matcher.count.unwrap_or(
            // Default semantic: "at least one matching call".
            crate::expectation::CountConstraint::AtLeast(1),
        );
        if !constraint.allows(actual_matches) {
            failures.push(Failure::RequiredToolCallNotSatisfied {
                name: matcher.name.clone(),
                expected_count: matcher.count,
                actual_matches,
                args_filter_set: matcher.args_subset.is_some(),
                result_filter_set: matcher.result_subset.is_some(),
            });
        }
    }

    // Token budget.
    if let Some(budget) = expect.max_tokens_total {
        let actual = outcome.total_tokens();
        if actual > budget {
            failures.push(Failure::TokenBudgetExceeded { budget, actual });
        }
    }

    // Duration budget — uses session_duration_ms, not wall-clock elapsed,
    // so judgement is independent of CI host speed.
    if let Some(budget) = expect.max_duration_ms {
        let actual = outcome.metrics.session_duration_ms;
        if actual > budget {
            failures.push(Failure::DurationExceeded {
                budget_ms: budget,
                actual_ms: actual,
            });
        }
    }

    // Error type — verbatim match against the captured fixture error.
    if let Some(expected) = &expect.expected_error_type {
        match outcome.error_type.as_deref() {
            Some(actual) if actual == expected => {}
            Some(actual) => failures.push(Failure::ErrorTypeMismatch {
                expected: expected.clone(),
                actual: actual.to_string(),
            }),
            None => failures.push(Failure::ExpectedErrorMissing {
                expected: expected.clone(),
            }),
        }
    }

    // Replay-time misbehaviour (script exhausted, unused script,
    // non-scripted runtime error) — always a failure, independent of
    // fixture expectations. Promoted last so it sits at the end of the
    // failure list for diff stability.
    if let Some(rf) = &outcome.runtime_failure {
        failures.push(Failure::ReplayRuntimeFailure {
            failure: rf.clone(),
        });
    }

    failures
}

/// Returns `true` when `needle` appears as a (non-contiguous) subsequence of
/// `haystack`, preserving relative order. Empty `needle` trivially matches.
fn is_ordered_subseq(needle: &[String], haystack: &[String]) -> bool {
    let mut i = 0;
    for h in haystack {
        if i >= needle.len() {
            break;
        }
        if h == &needle[i] {
            i += 1;
        }
    }
    i == needle.len()
}

/// Count tool calls in `outcome` that match the matcher's name and (if
/// set) its `args_subset` / `result_subset`. A matcher with either
/// subset set matches only when the corresponding captured field is
/// also `Some(_)` and structurally covers the subset — uncaptured
/// fields never satisfy a sub-filter, so missing `ToolIoCapture` shows
/// up as a failed assertion instead of a silent pass.
fn count_matching_calls(matcher: &ToolCallMatcher, outcome: &ReplayOutcome) -> u32 {
    let mut n: u32 = 0;
    for span in &outcome.metrics.tools {
        if span.name != matcher.name {
            continue;
        }
        if let Some(subset) = &matcher.args_subset {
            match &span.call_arguments {
                Some(actual) if json_subset(subset, actual) => {}
                _ => continue,
            }
        }
        if let Some(subset) = &matcher.result_subset {
            match &span.call_result {
                Some(actual) if json_subset(subset, actual) => {}
                _ => continue,
            }
        }
        n = n.saturating_add(1);
    }
    n
}

#[cfg(test)]
#[path = "score_test.rs"]
mod tests;
