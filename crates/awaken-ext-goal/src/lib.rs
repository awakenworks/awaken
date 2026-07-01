//! `awaken-ext-goal` — goal / outcome evaluation as an extension (never in the
//! kernel). At a run's natural end a [`GoalGuard`] judges the deliverable against
//! a [`GoalSpec`] through a [`Grader`]; if it needs revision the guard steers one
//! more turn with feedback, bounded by `max_iterations`. The runtime owns *when*
//! the loop stops (the run-end guard hook); this crate owns the break decision.
//!
//! The grader is async: [`KeywordGrader`] is a deterministic offline judge, while
//! [`DelegateGrader`] runs a real judge sub-agent through a host-supplied
//! [`DelegateRunner`]. This crate depends only on the runtime contract; the host
//! wires the concrete delegation.

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

/// Which agent judges the deliverable (used by a delegating grader). A goal that
/// does not say defaults to the grader's own configured judge.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GraderRef {
    /// The grader's configured default judge agent.
    #[default]
    Default,
    /// A specific resolvable judge agent id.
    Agent {
        /// Identifier of the grading agent.
        agent_id: String,
    },
}

/// A goal to grade a deliverable against. `rubric` is the requirement text (a
/// Managed `user.define_outcome` rubric normalizes to this via [`rubric_text`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSpec {
    pub description: String,
    pub rubric: String,
    pub max_iterations: u32,
    /// Which agent grades the deliverable when a delegating grader is used.
    #[serde(default)]
    pub grader: GraderRef,
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
            grader: GraderRef::Default,
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

/// A grader could not judge (a judge run failed, or its reply did not parse). The
/// guard fails open on this: it ends the run classified [`GoalOutcome::Failed`]
/// rather than looping forever.
#[derive(Debug, Clone)]
pub struct GraderError(pub String);

/// Judges a deliverable against a goal. Async so a real grader can run a judge
/// sub-agent before deciding; a stub or the deterministic [`KeywordGrader`]
/// returns immediately. An `Err` means the grader could not judge (fail-open).
#[async_trait]
pub trait Grader: Send + Sync {
    async fn grade(&self, goal: &GoalSpec, deliverable: &str) -> Result<Verdict, GraderError>;
}

/// A deterministic grader: the deliverable meets the goal iff it contains the
/// rubric text (an empty rubric is met — fail-open). Network-free, so the outcome
/// loop runs in CI and under the SDK e2e without an API key.
pub struct KeywordGrader;

#[async_trait]
impl Grader for KeywordGrader {
    async fn grade(&self, goal: &GoalSpec, deliverable: &str) -> Result<Verdict, GraderError> {
        let verdict = if goal.rubric.is_empty() || deliverable.contains(&goal.rubric) {
            Verdict {
                met: true,
                explanation: format!("deliverable satisfies the rubric ({:?})", goal.rubric),
            }
        } else {
            Verdict {
                met: false,
                explanation: format!("deliverable must satisfy the rubric ({:?})", goal.rubric),
            }
        };
        Ok(verdict)
    }
}

/// A request to run one judge sub-agent: the judge agent id and the prompt.
pub struct DelegateRequest {
    /// The agent that grades this deliverable.
    pub agent_id: String,
    /// The judge prompt (goal + rubric + deliverable, asking for a JSON verdict).
    pub prompt: String,
}

/// A judge sub-run's reply: the judge's last assistant text, if any.
pub struct DelegateReply {
    pub text: Option<String>,
}

/// A judge sub-run failed to execute (backend/transport error).
#[derive(Debug, Clone)]
pub struct DelegateError(pub String);

/// Runs a judge sub-agent and returns its reply. This crate declares the
/// capability it needs; the host implements it over the kernel's delegation, so
/// `awaken-ext-goal` stays a single-contract extension with no kernel dependency.
#[async_trait]
pub trait DelegateRunner: Send + Sync {
    async fn run(&self, request: DelegateRequest) -> Result<DelegateReply, DelegateError>;
}

/// A grader backed by a real judge sub-agent. It routes to the judge named by the
/// goal's [`GraderRef`] (`Default` → the configured default judge, `Agent` → a
/// specific one), runs it in its own child context, and parses a structured
/// [`Verdict`] from the reply. A run/parse failure is a [`GraderError`]; the
/// fail-open policy lives in the guard, so this stays a faithful judge.
pub struct DelegateGrader {
    runner: Arc<dyn DelegateRunner>,
    default_judge_agent_id: String,
}

impl DelegateGrader {
    /// Grade through `runner`, defaulting to `default_judge_agent_id` when a goal
    /// selects [`GraderRef::Default`].
    pub fn new(runner: Arc<dyn DelegateRunner>, default_judge_agent_id: impl Into<String>) -> Self {
        Self {
            runner,
            default_judge_agent_id: default_judge_agent_id.into(),
        }
    }

    fn judge_agent_id<'a>(&'a self, goal: &'a GoalSpec) -> &'a str {
        match &goal.grader {
            GraderRef::Default => &self.default_judge_agent_id,
            GraderRef::Agent { agent_id } => agent_id,
        }
    }
}

fn judge_prompt(goal: &GoalSpec, deliverable: &str) -> String {
    format!(
        "You are grading a deliverable against a goal. Respond with ONLY a JSON \
         object of the form {{\"met\": <bool>, \"explanation\": <string>}} and \
         nothing else.\n\nGoal: {goal}\n\nRubric:\n{rubric}\n\nDeliverable:\n{deliverable}",
        goal = goal.description,
        rubric = goal.rubric,
    )
}

/// Parse a verdict from the judge's reply, tolerating prose around the JSON.
fn parse_verdict(reply: Option<&str>) -> Result<Verdict, GraderError> {
    let text = reply.ok_or_else(|| GraderError("judge returned no reply".into()))?;
    let json = match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start <= end => &text[start..=end],
        _ => return Err(GraderError("no JSON object in judge reply".into())),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(json).map_err(|e| GraderError(format!("unparseable verdict: {e}")))?;
    let met = parsed
        .get("met")
        .and_then(Value::as_bool)
        .ok_or_else(|| GraderError("judge verdict missing boolean `met`".into()))?;
    let explanation = parsed
        .get("explanation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Verdict { met, explanation })
}

#[async_trait]
impl Grader for DelegateGrader {
    async fn grade(&self, goal: &GoalSpec, deliverable: &str) -> Result<Verdict, GraderError> {
        let request = DelegateRequest {
            agent_id: self.judge_agent_id(goal).to_string(),
            prompt: judge_prompt(goal, deliverable),
        };
        let reply = self
            .runner
            .run(request)
            .await
            .map_err(|DelegateError(reason)| GraderError(format!("judge run failed: {reason}")))?;
        parse_verdict(reply.text.as_deref())
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
        let verdict = match self.grader.grade(&self.spec, &deliverable).await {
            Ok(verdict) => verdict,
            // Fail-open: a grader that errors or can't parse never traps the run
            // in unbounded revision — it ends, classified Failed.
            Err(GraderError(reason)) => {
                return RunEndDecision::Complete {
                    detail: round_detail(GoalOutcome::Failed, &reason),
                };
            }
        };
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
