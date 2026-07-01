//! `awaken-ext-goal` — goal / outcome evaluation as an extension (never in the
//! kernel, ADR-0034 D6 / ADR-0062). A run reaches a terminal deliverable; a
//! [`Grader`] judges it against a [`GoalSpec`]; if it needs revision an
//! above-kernel coordinator re-dispatches a round with feedback, bounded by
//! `max_iterations`. The kernel knows none of this — it just runs turns.
//!
//! The neutral crate owns the vocabulary and the grader contract; the loop that
//! re-dispatches turns lives in the host (server), which drives the runtime.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, Plugin, PluginManifest, RunEndContext, RunEndDecision,
    RunEndGuard,
};
use awaken_runtime_contract::{Message, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Plugin/guard id. The registration key, the guard's `id()`, and the capability
/// bound all share this one constant.
const GOAL_PLUGIN_ID: &str = "goal";

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

/// The deliverable the grader judges: the newest assistant message with
/// non-empty text. The run-end guard fires after a text turn, so there is always
/// one; an empty transcript yields an empty deliverable (the grader decides).
fn last_deliverable(conversation: &[Message]) -> String {
    conversation
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.text_content().trim().is_empty())
        .map(Message::text_content)
        .unwrap_or_default()
}

/// The opaque round detail crossing into the kernel: this crate's classification
/// as data, so the runtime forwards it without learning goal vocabulary (ACL).
fn round_detail(outcome: GoalOutcome, explanation: &str) -> Value {
    serde_json::json!({ "result": outcome.token(), "explanation": explanation })
}

/// Run-end continuation guard that drives the grade→revise loop. This crate owns
/// the goal semantics (the loop's *break decision*); the runtime owns *when* the
/// loop stops and only forwards the guard's opaque `detail` (ADR: run-end guard).
pub struct GoalGuard {
    spec: Arc<GoalSpec>,
    grader: Arc<dyn Grader>,
}

impl GoalGuard {
    /// Build a guard for `spec`, judged by `grader`.
    pub fn new(spec: GoalSpec, grader: Arc<dyn Grader>) -> Self {
        Self {
            spec: Arc::new(spec),
            grader,
        }
    }
}

#[async_trait]
impl RunEndGuard for GoalGuard {
    fn id(&self) -> &str {
        GOAL_PLUGIN_ID
    }

    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision {
        let deliverable = last_deliverable(ctx.conversation);
        let verdict = self.grader.grade(&self.spec, &deliverable);
        // `forced_continuations` counts the steers already taken; this consult is
        // the next iteration (1-based).
        let iteration = ctx.forced_continuations as u32 + 1;
        let outcome = classify(&verdict, iteration, self.spec.max_iterations);
        if outcome.is_terminal() {
            RunEndDecision::Complete {
                detail: round_detail(outcome, &verdict.explanation),
            }
        } else {
            // Unmet within budget → steer one revision turn. The feedback carries
            // the grader's reason so the agent knows what to fix.
            let feedback = format!(
                "Your previous answer did not meet the goal ({}). {} Revise it.",
                self.spec.description, verdict.explanation
            );
            RunEndDecision::Steer {
                feedback,
                detail: round_detail(GoalOutcome::NeedsRevision, &verdict.explanation),
            }
        }
    }
}

/// Plugin contributing the goal run-end guard. The default goal comes from the
/// constructor; a host builds one per outcome request with the concrete spec and
/// its grader, and selects it by id in the run's `plugin_ids`.
pub struct GoalPlugin {
    spec: GoalSpec,
    grader: Arc<dyn Grader>,
}

impl GoalPlugin {
    /// Build a goal plugin with a fixed spec and a grader implementation.
    pub fn new(spec: GoalSpec, grader: Arc<dyn Grader>) -> Self {
        Self { spec, grader }
    }
}

impl Plugin for GoalPlugin {
    fn manifest(&self) -> PluginManifest {
        // One run-end guard; no tools, state keys, phase hooks, or action kinds.
        PluginManifest {
            id: GOAL_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                run_end_guards: vec![GOAL_PLUGIN_ID.into()],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new(GOAL_PLUGIN_ID);
        contributions.run_end_guards.push(Arc::new(GoalGuard::new(
            self.spec.clone(),
            Arc::clone(&self.grader),
        )));
        contributions
    }
}

#[cfg(test)]
mod tests;
