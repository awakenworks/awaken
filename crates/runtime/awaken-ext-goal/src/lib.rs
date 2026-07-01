//! `awaken-ext-goal` — goal / outcome evaluation as an extension (never in the
//! kernel, ADR-0034 D6 / ADR-0062). A run reaches a terminal deliverable; a
//! [`Grader`] judges it against a [`GoalSpec`]; if it needs revision an
//! above-kernel coordinator re-dispatches a round with feedback, bounded by
//! `max_iterations`. The kernel knows none of this — it just runs turns.
//!
//! The neutral crate owns the vocabulary and the grader contract; the loop that
//! re-dispatches turns lives in the host (server), which drives the runtime.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A goal to grade a deliverable against. `rubric` is the requirement text (a
/// Managed `user.define_outcome` rubric normalizes to this via [`rubric_text`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSpec {
    pub description: String,
    pub rubric: String,
    pub max_iterations: u32,
}

impl GoalSpec {
    pub fn new(
        description: impl Into<String>,
        rubric: impl Into<String>,
        max_iterations: u32,
    ) -> Self {
        Self {
            description: description.into(),
            rubric: rubric.into(),
            max_iterations: max_iterations.max(1),
        }
    }
}

/// Normalize a Managed rubric (a bare string, `{type:"text",content}`, or
/// `{type:"file",...}`) to requirement text. A file rubric has no inline text, so
/// it yields an empty requirement (fail-open at the grader).
pub fn rubric_text(rubric: &Value) -> String {
    match rubric {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// The classification of one evaluation round. Producer-defined (ADR-0062 D4);
/// the host projects it to a public result token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalOutcome {
    /// The deliverable met the rubric.
    Satisfied,
    /// Not yet met; revise and try again.
    NeedsRevision,
    /// The iteration budget is spent without meeting the rubric.
    MaxIterationsReached,
    /// The grader could not judge (fail-open: end rather than loop forever).
    Failed,
}

impl GoalOutcome {
    /// The public Managed result token.
    pub fn token(&self) -> &'static str {
        match self {
            GoalOutcome::Satisfied => "satisfied",
            GoalOutcome::NeedsRevision => "needs_revision",
            GoalOutcome::MaxIterationsReached => "max_iterations_reached",
            GoalOutcome::Failed => "failed",
        }
    }

    /// Whether the goal loop should stop (terminal) at this outcome.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, GoalOutcome::NeedsRevision)
    }
}

/// One graded verdict: whether the deliverable is met, plus the rationale.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub met: bool,
    pub explanation: String,
}

/// Judges a deliverable against a goal. A concrete grader is any resolvable judge
/// (ADR-0062 D4); the host may run a sub-agent instead. This crate ships a
/// deterministic [`KeywordGrader`] for tests and offline use.
pub trait Grader: Send + Sync {
    fn grade(&self, goal: &GoalSpec, deliverable: &str) -> Verdict;
}

/// A deterministic grader: the deliverable meets the goal iff it contains the
/// rubric text (an empty rubric is met — fail-open). Network-free, so the outcome
/// loop runs in CI and under the SDK e2e without an API key.
pub struct KeywordGrader;

impl Grader for KeywordGrader {
    fn grade(&self, goal: &GoalSpec, deliverable: &str) -> Verdict {
        if goal.rubric.is_empty() || deliverable.contains(&goal.rubric) {
            Verdict {
                met: true,
                explanation: format!("deliverable satisfies the rubric ({:?})", goal.rubric),
            }
        } else {
            Verdict {
                met: false,
                explanation: format!("deliverable must satisfy the rubric ({:?})", goal.rubric),
            }
        }
    }
}

/// Classify a verdict at a given iteration into an outcome, applying the
/// iteration budget. `iteration` is 1-based.
pub fn classify(verdict: &Verdict, iteration: u32, max_iterations: u32) -> GoalOutcome {
    if verdict.met {
        GoalOutcome::Satisfied
    } else if iteration >= max_iterations {
        GoalOutcome::MaxIterationsReached
    } else {
        GoalOutcome::NeedsRevision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_grader_and_classify() {
        let goal = GoalSpec::new("finish", "FINAL", 3);
        let grader = KeywordGrader;
        assert!(grader.grade(&goal, "here is the FINAL text").met);
        assert!(!grader.grade(&goal, "a draft").met);

        let miss = grader.grade(&goal, "draft");
        assert_eq!(classify(&miss, 1, 3), GoalOutcome::NeedsRevision);
        assert_eq!(classify(&miss, 3, 3), GoalOutcome::MaxIterationsReached);
        let hit = grader.grade(&goal, "FINAL");
        assert_eq!(classify(&hit, 1, 3), GoalOutcome::Satisfied);
    }

    #[test]
    fn rubric_normalizes() {
        assert_eq!(rubric_text(&serde_json::json!("X")), "X");
        assert_eq!(
            rubric_text(&serde_json::json!({"type":"text","content":"Y"})),
            "Y"
        );
        assert_eq!(
            rubric_text(&serde_json::json!({"type":"file","file_id":"f"})),
            ""
        );
    }
}
