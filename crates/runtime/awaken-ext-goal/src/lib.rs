//! `awaken-ext-goal` — goal / outcome evaluation as an extension (never in the
//! kernel). At a run's natural end a [`GoalGuard`] judges the deliverable against
//! a [`GoalSpec`] through a [`Grader`]; if it needs revision the guard steers one
//! more turn with feedback, bounded by `max_iterations`. The runtime owns *when*
//! the loop stops (the run-end guard hook); this crate owns the break decision.
//!
//! The grader is async: [`KeywordGrader`] is a deterministic offline judge, while
//! [`AgentToolGrader`] invokes a host-supplied ordinary Agent-backed tool.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginConfigError, PluginManifest,
    RunEndContext, RunEndDecision, RunEndGuard,
};
use awaken_runtime_contract::tool::{RawTool, ToolCall, invoke_raw_tool};
use awaken_runtime_contract::{CancellationToken, Message, MessageId, Role};
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

/// A goal to grade a deliverable against. `rubric` is plain requirement text; any
/// managed-plane rubric wire shape is normalized to it by the host/managed adapter
/// that builds the goal, never by this neutral crate.
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

/// The **grader's** judgement for one deliverable — nothing about iteration
/// budgets or lifecycle (those are the loop's concern, [`GoalOutcome`]). Keeping
/// the grader verdict separate from the loop result is the boundary that lets a
/// grader stay a faithful judge: it never needs to know how many revisions are
/// left (separation of concerns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GradeResult {
    /// The deliverable meets every rubric criterion.
    Satisfied,
    /// The deliverable misses one or more criteria and should be revised.
    NeedsRevision,
    /// The rubric fundamentally does not match the task (not a revision matter).
    Failed,
}

/// The **loop's** classification of one evaluation round: a grader verdict folded
/// with the iteration budget and lifecycle. The host projects it to a public
/// result token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalOutcome {
    /// The deliverable met the rubric.
    Satisfied,
    /// Not yet met; revise and try again.
    NeedsRevision,
    /// The iteration budget is spent without meeting the rubric.
    MaxIterationsReached,
    /// The grader could not judge, or the rubric does not fit the task.
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

    /// Whether the goal loop should stop (terminal) at this outcome. Only a
    /// revision keeps it going.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, GoalOutcome::NeedsRevision)
    }
}

/// One graded verdict: the grader's [`GradeResult`] plus the rationale handed
/// back to the agent as revision guidance.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub result: GradeResult,
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
    /// Grade `deliverable` against `goal`. `cancellation`, when set, is the parent
    /// run's token: a grader that spawns a judge sub-run forwards it so cancelling
    /// the run cancels the judge too.
    async fn grade(
        &self,
        goal: &GoalSpec,
        deliverable: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Verdict, GraderError>;
}

/// A deterministic grader: the deliverable meets the goal iff it contains the
/// rubric text (an empty rubric is met — fail-open). Network-free, so the outcome
/// loop runs in CI and under the SDK e2e without an API key.
pub struct KeywordGrader;

#[async_trait]
impl Grader for KeywordGrader {
    async fn grade(
        &self,
        goal: &GoalSpec,
        deliverable: &str,
        _cancellation: Option<&CancellationToken>,
    ) -> Result<Verdict, GraderError> {
        let verdict = if goal.rubric.is_empty() || deliverable.contains(&goal.rubric) {
            Verdict {
                result: GradeResult::Satisfied,
                explanation: format!("deliverable satisfies the rubric ({:?})", goal.rubric),
            }
        } else {
            Verdict {
                result: GradeResult::NeedsRevision,
                explanation: format!("deliverable must satisfy the rubric ({:?})", goal.rubric),
            }
        };
        Ok(verdict)
    }
}

/// A grader backed by an ordinary Agent tool. It routes to the judge named by
/// the goal's [`GraderRef`] (`Default` → the
/// configured default judge, `Agent` → a specific one), runs it in its own child
/// context, and parses a structured [`Verdict`] from the reply. A run/parse
/// failure is a [`GraderError`]; the fail-open policy lives in the guard, so this
/// stays a faithful judge.
pub struct AgentToolGrader {
    agent_tool: Arc<dyn RawTool>,
    default_judge_agent_id: String,
}

impl AgentToolGrader {
    /// Grade through `agent_tool`, defaulting to `default_judge_agent_id` when a goal
    /// selects [`GraderRef::Default`].
    pub fn new(agent_tool: Arc<dyn RawTool>, default_judge_agent_id: impl Into<String>) -> Self {
        Self {
            agent_tool,
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
        "You are an impartial grader. Score the artifact against the rubric. Respond with \
         ONLY a JSON object of the form {{\"result\": \"satisfied\"|\"needs_revision\"|\"failed\", \
         \"explanation\": <string>}} and nothing else. Use \"satisfied\" only when every criterion \
         is met; \"needs_revision\" when one or more fail but the rubric fits the task; \"failed\" \
         only when the rubric fundamentally does not match the task.\n\nGoal: {goal}\n\nRubric:\n\
         {rubric}\n\nArtifact:\n{deliverable}",
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
    let result = parsed
        .get("result")
        .and_then(Value::as_str)
        .and_then(|s| match s {
            "satisfied" => Some(GradeResult::Satisfied),
            "needs_revision" => Some(GradeResult::NeedsRevision),
            "failed" => Some(GradeResult::Failed),
            _ => None,
        })
        .ok_or_else(|| GraderError("judge verdict missing a valid `result`".into()))?;
    let explanation = parsed
        .get("explanation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Verdict {
        result,
        explanation,
    })
}

#[async_trait]
impl Grader for AgentToolGrader {
    async fn grade(
        &self,
        goal: &GoalSpec,
        deliverable: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Verdict, GraderError> {
        let output = invoke_raw_tool(
            self.agent_tool.as_ref(),
            ToolCall {
                call_id: "goal-judge-agent-run".into(),
                tool_id: self.agent_tool.id().to_string(),
                arguments: serde_json::json!({
                    "agent_id": self.judge_agent_id(goal),
                    "seed": vec![Message::text(
                    MessageId("judge-prompt".into()),
                    Role::User,
                    judge_prompt(goal, deliverable),
                    )],
                }),
            },
            cancellation,
        )
        .await
        .map_err(|error| GraderError(format!("judge run failed: {error}")))?;
        if output.is_error {
            return Err(GraderError(format!("judge run failed: {}", output.content)));
        }
        parse_verdict(Some(&output.content))
    }
}

/// Fold a grader [`Verdict`] and the iteration budget into a loop [`GoalOutcome`]
/// (`iteration` is 1-based). This is the **only** place the budget meets the
/// verdict — the grader itself never sees iteration counts.
pub fn classify(verdict: &Verdict, iteration: u32, max_iterations: u32) -> GoalOutcome {
    match verdict.result {
        GradeResult::Satisfied => GoalOutcome::Satisfied,
        // A rubric that does not fit the task will never be met; end, do not burn
        // the budget revising against it.
        GradeResult::Failed => GoalOutcome::Failed,
        GradeResult::NeedsRevision if iteration >= max_iterations => {
            GoalOutcome::MaxIterationsReached
        }
        GradeResult::NeedsRevision => GoalOutcome::NeedsRevision,
    }
}

/// The deliverable the grader judges: the newest assistant message with
/// non-empty text, scanning from the tail so a trailing tool message or an empty
/// assistant turn is skipped. `None` when the run produced nothing substantive —
/// the guard then ends the run without grading rather than judging an empty
/// string.
fn last_deliverable(conversation: &[Message]) -> Option<String> {
    conversation
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.text_content().trim().is_empty())
        .map(Message::text_content)
}

/// The opaque round detail crossing into the kernel: this crate's classification
/// as data, so the runtime forwards it without learning goal vocabulary (ACL).
fn round_detail(outcome: GoalOutcome, explanation: &str) -> Value {
    serde_json::json!({ "result": outcome.token(), "explanation": explanation })
}

/// A terminal decision: end the run classified `outcome`, carrying the
/// classification as opaque `detail`. The single place the guard builds a
/// `Complete`, so every terminal path — a met/failed verdict, a spent budget, an
/// empty run — reads the same.
fn conclude(outcome: GoalOutcome, explanation: &str) -> RunEndDecision {
    RunEndDecision::Complete {
        detail: round_detail(outcome, explanation),
    }
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
        // Degenerate inputs are settled before the grader is consulted, so a spent
        // budget or an empty run never costs a judge call.

        // A zero revision budget is spent before it starts: end now, classified as
        // the budget being reached. Handles a `max_iterations` of 0 from any
        // construction path, not just the clamped `GoalSpec::new`.
        if self.spec.max_iterations == 0 {
            return conclude(
                GoalOutcome::MaxIterationsReached,
                "revision budget exhausted",
            );
        }

        // Nothing substantive was produced → there is no deliverable to judge. End
        // rather than grade an empty string (which a keyword grader would steer a
        // pointless revision over). Folded into `Failed`, the same fail-open
        // terminal bucket as an unjudgeable grade, so no non-Managed token leaks.
        let Some(deliverable) = last_deliverable(ctx.conversation) else {
            return conclude(GoalOutcome::Failed, "nothing produced to judge");
        };

        let verdict = match self
            .grader
            .grade(&self.spec, &deliverable, ctx.cancellation)
            .await
        {
            Ok(verdict) => verdict,
            // Fail-open: a grader that errors or can't parse never traps the run
            // in unbounded revision — it ends, classified Failed.
            Err(GraderError(reason)) => return conclude(GoalOutcome::Failed, &reason),
        };
        // `forced_continuations` counts the steers already taken; this consult is
        // the next iteration (1-based).
        let iteration = ctx.forced_continuations as u32 + 1;
        let outcome = classify(&verdict, iteration, self.spec.max_iterations);
        if outcome.is_terminal() {
            conclude(outcome, &verdict.explanation)
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
            config_sections: vec![GOAL_PLUGIN_ID.into()],
            bound: CapabilityBound {
                run_end_guards: IdBound::Exact(vec![GOAL_PLUGIN_ID.into()]),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        self.contribute(self.spec.clone())
    }

    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        // The grader is a live dependency held on the plugin; only the goal spec
        // (pure data) comes from the agent's config section.
        let Some(value) = config else {
            return Ok(self.resolve());
        };
        let spec: GoalSpec = serde_json::from_value(value.clone())
            .map_err(|e| PluginConfigError::new(GOAL_PLUGIN_ID, e.to_string()))?;
        Ok(self.contribute(spec))
    }
}

impl GoalPlugin {
    fn contribute(&self, spec: GoalSpec) -> Contributions {
        let mut contributions = Contributions::new(GOAL_PLUGIN_ID);
        contributions
            .run_end_guards
            .push(Arc::new(GoalGuard::new(spec, Arc::clone(&self.grader))));
        contributions
    }
}

#[cfg(test)]
mod tests;
