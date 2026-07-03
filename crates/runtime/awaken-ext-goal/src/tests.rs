//! Behavioural tests for the goal run-end guard, its vocabulary, the delegating
//! grader, and its plugin wiring. Adapted from the reference `awaken-ext-goal`
//! suite to this crate's simpler model: the runtime owns the loop (a run-end
//! guard steers/completes) and iteration comes from the kernel's
//! `forced_continuations` (no thread-scoped `GoalState` or `set_goal` tool here).

use super::*;
use awaken_runtime_contract::plugin::{ResolvedExecutionEnv, enforce_bound};
use awaken_runtime_contract::{MessageId, RunId};

// ── Test graders ────────────────────────────────────────────────────────────

/// A grader that returns a fixed verdict regardless of the deliverable.
struct FixedGrader(Verdict);

#[async_trait]
impl Grader for FixedGrader {
    async fn grade(
        &self,
        _goal: &GoalSpec,
        _deliverable: &str,
        _cancellation: Option<&CancellationToken>,
    ) -> Result<Verdict, GraderError> {
        Ok(self.0.clone())
    }
}

fn met(explanation: &str) -> Verdict {
    Verdict {
        result: GradeResult::Satisfied,
        explanation: explanation.into(),
    }
}

fn unmet(explanation: &str) -> Verdict {
    Verdict {
        result: GradeResult::NeedsRevision,
        explanation: explanation.into(),
    }
}

fn unfit(explanation: &str) -> Verdict {
    Verdict {
        result: GradeResult::Failed,
        explanation: explanation.into(),
    }
}

fn spec(max_iterations: u32) -> GoalSpec {
    GoalSpec::new("ship the feature", "FINAL", max_iterations)
}

fn fixed_guard(spec: GoalSpec, verdict: Verdict) -> GoalGuard {
    GoalGuard::new(spec, Arc::new(FixedGrader(verdict)))
}

fn assistant(text: &str) -> Message {
    Message::text(MessageId("a".into()), Role::Assistant, text)
}

fn user(text: &str) -> Message {
    Message::text(MessageId("u".into()), Role::User, text)
}

fn tool_msg(text: &str) -> Message {
    Message::text(MessageId("t".into()), Role::Tool, text)
}

/// Evaluate a guard against a conversation at a given forced-continuation count.
async fn evaluate(guard: &GoalGuard, conversation: &[Message], fc: usize) -> RunEndDecision {
    let state = awaken_runtime_contract::Store::new();
    let ctx = RunEndContext {
        run_id: RunId("run-1".into()),
        conversation,
        forced_continuations: fc,
        cancellation: None,
        state: &state,
    };
    guard.evaluate(&ctx).await
}

/// The opaque detail carried by either decision variant.
fn detail(decision: &RunEndDecision) -> &Value {
    match decision {
        RunEndDecision::Complete { detail } | RunEndDecision::Steer { detail, .. } => detail,
    }
}

fn result_token(decision: &RunEndDecision) -> String {
    detail(decision)["result"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn explanation(decision: &RunEndDecision) -> String {
    detail(decision)["explanation"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

// ── Guard: terminal vs steer classification ─────────────────────────────────

#[tokio::test]
async fn met_deliverable_completes_satisfied() {
    let d = evaluate(
        &fixed_guard(spec(3), met("looks good")),
        &[assistant("done")],
        0,
    )
    .await;
    assert!(matches!(d, RunEndDecision::Complete { .. }));
    assert_eq!(result_token(&d), "satisfied");
    // The grader's rationale is forwarded verbatim in the opaque detail.
    assert_eq!(explanation(&d), "looks good");
}

#[tokio::test]
async fn unmet_within_budget_steers_needs_revision() {
    let d = evaluate(
        &fixed_guard(spec(3), unmet("add an edge-case test")),
        &[assistant("draft")],
        0,
    )
    .await;
    match &d {
        RunEndDecision::Steer { feedback, .. } => {
            // The feedback must carry the trigger phrase the reviser keys on and
            // the grader's reason, so the agent knows what to fix.
            assert!(feedback.contains("did not meet the goal"));
            assert!(feedback.contains("add an edge-case test"));
            assert!(feedback.contains("ship the feature")); // the goal description
        }
        _ => panic!("expected a steer decision"),
    }
    assert_eq!(result_token(&d), "needs_revision");
    assert_eq!(explanation(&d), "add an edge-case test");
}

#[tokio::test]
async fn unmet_at_last_iteration_completes_max_iterations() {
    // forced_continuations = 2 → this is iteration 3, the budget of spec(3).
    let d = evaluate(
        &fixed_guard(spec(3), unmet("nope")),
        &[assistant("draft")],
        2,
    )
    .await;
    assert!(matches!(d, RunEndDecision::Complete { .. }));
    assert_eq!(result_token(&d), "max_iterations_reached");
}

#[tokio::test]
async fn iteration_is_forced_continuations_plus_one() {
    // At fc=1 (iteration 2 of 3) an unmet verdict still steers; at fc=2
    // (iteration 3 of 3) it exhausts the budget instead.
    let steer = evaluate(&fixed_guard(spec(3), unmet("x")), &[assistant("d")], 1).await;
    assert_eq!(result_token(&steer), "needs_revision");
    let done = evaluate(&fixed_guard(spec(3), unmet("x")), &[assistant("d")], 2).await;
    assert_eq!(result_token(&done), "max_iterations_reached");
}

#[tokio::test]
async fn budget_of_one_completes_on_first_unmet_without_steering() {
    let d = evaluate(&fixed_guard(spec(1), unmet("x")), &[assistant("d")], 0).await;
    assert!(matches!(d, RunEndDecision::Complete { .. }));
    assert_eq!(result_token(&d), "max_iterations_reached");
}

#[tokio::test]
async fn met_completes_even_when_budget_is_spent() {
    // A met verdict wins over the budget: satisfied, not exhausted.
    let d = evaluate(&fixed_guard(spec(2), met("ok")), &[assistant("d")], 5).await;
    assert_eq!(result_token(&d), "satisfied");
}

// ── Guard: deliverable selection ────────────────────────────────────────────

#[tokio::test]
async fn guard_grades_the_newest_assistant_deliverable() {
    // A real (keyword) grader over a rubric "FINAL": the newest assistant text
    // decides, so an older matching turn does not make a failing draft pass.
    let guard = GoalGuard::new(spec(3), Arc::new(KeywordGrader));
    let convo = [
        user("write it"),
        assistant("an early FINAL version"),
        assistant("a later rough draft"),
    ];
    let d = evaluate(&guard, &convo, 0).await;
    assert_eq!(result_token(&d), "needs_revision"); // graded the later draft, unmet

    let convo_met = [assistant("a draft"), assistant("the FINAL answer")];
    let d = evaluate(&guard, &convo_met, 0).await;
    assert_eq!(result_token(&d), "satisfied");
}

#[test]
fn last_deliverable_skips_trailing_tool_and_empty_turns() {
    let convo = [
        user("do it"),
        assistant("the real answer"),
        assistant("   "),
        tool_msg("tool output"),
    ];
    assert_eq!(
        last_deliverable(&convo),
        Some("the real answer".to_string())
    );

    // Only a whitespace assistant turn and a tool tail → no deliverable.
    let empty = [user("do it"), assistant("  "), tool_msg("out")];
    assert_eq!(last_deliverable(&empty), None);
}

#[tokio::test]
async fn no_deliverable_completes_failed_without_grading() {
    // The grader would say `satisfied`; it must not be consulted. A whitespace
    // assistant turn and a trailing tool message leave nothing to judge, so the
    // run ends `failed` (short-circuit) rather than the grader's verdict.
    let convo = [user("do it"), assistant("   "), tool_msg("out")];
    let d = evaluate(&fixed_guard(spec(3), met("ok")), &convo, 0).await;
    assert!(matches!(d, RunEndDecision::Complete { .. }));
    assert_eq!(result_token(&d), "failed");
    assert_eq!(explanation(&d), "nothing produced to judge");
}

#[tokio::test]
async fn zero_budget_completes_max_iterations_without_grading() {
    // A zero revision budget settles the run before any grade call: the grader
    // would say `satisfied`, but the budget short-circuit wins.
    let mut goal = spec(3);
    goal.max_iterations = 0;
    let d = evaluate(&fixed_guard(goal, met("ok")), &[assistant("done")], 0).await;
    assert!(matches!(d, RunEndDecision::Complete { .. }));
    assert_eq!(result_token(&d), "max_iterations_reached");
}

// ── GoalSpec / classify / GoalOutcome vocabulary ────────────────────────────

#[test]
fn goal_spec_new_clamps_max_iterations_to_at_least_one() {
    assert_eq!(GoalSpec::new("d", "r", 0).max_iterations, 1);
    assert_eq!(GoalSpec::new("d", "r", 5).max_iterations, 5);
}

#[tokio::test]
async fn keyword_grader_matches_rubric_substring_and_fails_open_on_empty() {
    let grader = KeywordGrader;
    let goal = GoalSpec::new("finish", "FINAL", 3);
    let met = |v: Verdict| v.result == GradeResult::Satisfied;
    assert!(met(grader
        .grade(&goal, "here is the FINAL text", None)
        .await
        .unwrap()));
    assert!(!met(grader.grade(&goal, "a draft", None).await.unwrap()));
    // Case-sensitive substring: a lowercase token does not satisfy the rubric.
    assert!(!met(grader
        .grade(&goal, "the final text", None)
        .await
        .unwrap()));
    // An empty rubric is met by anything (fail-open).
    let empty = GoalSpec::new("finish", "", 3);
    assert!(met(grader.grade(&empty, "", None).await.unwrap()));
    assert!(met(grader.grade(&empty, "whatever", None).await.unwrap()));
}

#[test]
fn classify_folds_grade_result_with_the_budget() {
    // Satisfied is satisfied regardless of iteration.
    assert_eq!(classify(&met("y"), 1, 3), GoalOutcome::Satisfied);
    assert_eq!(classify(&met("y"), 9, 3), GoalOutcome::Satisfied);
    // NeedsRevision below the bound asks for a revision.
    assert_eq!(classify(&unmet("n"), 1, 3), GoalOutcome::NeedsRevision);
    assert_eq!(classify(&unmet("n"), 2, 3), GoalOutcome::NeedsRevision);
    // NeedsRevision at or beyond the bound exhausts it.
    assert_eq!(
        classify(&unmet("n"), 3, 3),
        GoalOutcome::MaxIterationsReached
    );
    assert_eq!(
        classify(&unmet("n"), 4, 3),
        GoalOutcome::MaxIterationsReached
    );
    // A grader `Failed` (rubric does not fit) ends immediately — the budget is
    // never burned revising against an ill-fitting rubric.
    assert_eq!(classify(&unfit("bad rubric"), 1, 3), GoalOutcome::Failed);
    assert_eq!(classify(&unfit("bad rubric"), 3, 3), GoalOutcome::Failed);
}

#[test]
fn goal_outcome_tokens_and_terminality() {
    assert_eq!(GoalOutcome::Satisfied.token(), "satisfied");
    assert_eq!(GoalOutcome::NeedsRevision.token(), "needs_revision");
    assert_eq!(
        GoalOutcome::MaxIterationsReached.token(),
        "max_iterations_reached"
    );
    assert_eq!(GoalOutcome::Failed.token(), "failed");
    assert_eq!(GoalOutcome::Interrupted.token(), "interrupted");
    // Only a revision keeps the loop going; everything else is terminal.
    assert!(!GoalOutcome::NeedsRevision.is_terminal());
    assert!(GoalOutcome::Satisfied.is_terminal());
    assert!(GoalOutcome::MaxIterationsReached.is_terminal());
    assert!(GoalOutcome::Failed.is_terminal());
    assert!(GoalOutcome::Interrupted.is_terminal());
}

#[test]
fn rubric_text_normalizes_string_object_and_file() {
    assert_eq!(rubric_text(&serde_json::json!("X")), "X");
    assert_eq!(
        rubric_text(&serde_json::json!({"type":"text","content":"Y"})),
        "Y"
    );
    // A file rubric has no inline text → empty (fail-open at the grader).
    assert_eq!(
        rubric_text(&serde_json::json!({"type":"file","file_id":"f"})),
        ""
    );
}

#[test]
fn round_detail_carries_result_and_explanation() {
    let d = round_detail(GoalOutcome::NeedsRevision, "why");
    assert_eq!(d["result"], "needs_revision");
    assert_eq!(d["explanation"], "why");
}

// ── Plugin wiring / capability bound ────────────────────────────────────────

#[test]
fn guard_id_is_the_plugin_name() {
    let g = fixed_guard(spec(3), met("x"));
    assert_eq!(g.id(), GOAL_PLUGIN_ID);
    assert_eq!(g.id(), "goal");
}

#[test]
fn plugin_manifest_declares_only_the_run_end_guard() {
    let plugin = GoalPlugin::new(spec(3), Arc::new(FixedGrader(met("ok"))));
    let bound = plugin.manifest().bound;
    assert_eq!(bound.run_end_guards, vec!["goal".to_string()]);
    assert!(bound.tool_ids.is_empty());
    assert!(bound.state_keys.is_empty());
    assert!(bound.phase_hooks.is_empty());
    assert!(bound.action_kinds.is_empty());
}

#[test]
fn plugin_resolves_exactly_one_guard() {
    let plugin = GoalPlugin::new(spec(3), Arc::new(FixedGrader(met("ok"))));
    let contributions = plugin.resolve();
    assert_eq!(contributions.run_end_guards.len(), 1);
    assert_eq!(contributions.run_end_guards[0].id(), "goal");
    assert!(contributions.tools.is_empty());
    assert!(contributions.phase_hooks.is_empty());
}

#[test]
fn manifest_grant_admits_resolved_contributions() {
    // Anti-drift: the declared bound must admit everything `resolve` contributes.
    let plugin = GoalPlugin::new(spec(3), Arc::new(FixedGrader(met("ok"))));
    assert!(enforce_bound(&plugin.manifest(), &plugin.resolve()).is_ok());
}

#[test]
fn resolved_execution_env_surfaces_the_goal_guard() {
    let plugin = GoalPlugin::new(spec(3), Arc::new(FixedGrader(met("ok"))));
    let env = ResolvedExecutionEnv::merge(vec![(plugin.manifest(), plugin.resolve())])
        .expect("goal plugin merges");
    assert_eq!(env.run_end_guards().len(), 1);
    assert_eq!(env.run_end_guards()[0].id(), "goal");
}

// ── Delegating grader (judge sub-agent) ─────────────────────────────────────

/// A runner that returns a fixed reply, and records the agent id it was asked to
/// run — enough to assert both verdict parsing and judge routing.
#[derive(Default)]
struct StubRunner {
    reply: Option<String>,
    fail: bool,
    seen_agent: std::sync::Mutex<Option<String>>,
}

impl StubRunner {
    fn replying(reply: &str) -> Self {
        Self {
            reply: Some(reply.to_string()),
            ..Self::default()
        }
    }
}

#[async_trait]
impl DelegateRunner for StubRunner {
    async fn run(&self, request: DelegateRequest) -> Result<DelegateReply, DelegateError> {
        *self.seen_agent.lock().unwrap() = Some(request.agent_id);
        if self.fail {
            return Err(DelegateError("backend exploded".into()));
        }
        Ok(DelegateReply {
            text: self.reply.clone(),
        })
    }
}

fn agent_goal(grader: GraderRef) -> GoalSpec {
    GoalSpec {
        grader,
        ..GoalSpec::new("ship it", "tests pass", 3)
    }
}

#[tokio::test]
async fn delegate_grader_parses_a_satisfied_verdict() {
    let grader = DelegateGrader::new(
        Arc::new(StubRunner::replying(
            r#"{"result": "satisfied", "explanation": "all good"}"#,
        )),
        "judge",
    );
    let v = grader
        .grade(&agent_goal(GraderRef::Default), "done", None)
        .await
        .unwrap();
    assert_eq!(v.result, GradeResult::Satisfied);
    assert_eq!(v.explanation, "all good");
}

#[tokio::test]
async fn delegate_grader_parses_a_needs_revision_verdict_amid_prose() {
    let grader = DelegateGrader::new(
        Arc::new(StubRunner::replying(
            "Verdict: {\"result\": \"needs_revision\", \"explanation\": \"add an edge case\"}.",
        )),
        "judge",
    );
    let v = grader
        .grade(&agent_goal(GraderRef::Default), "done", None)
        .await
        .unwrap();
    assert_eq!(v.result, GradeResult::NeedsRevision);
    assert_eq!(v.explanation, "add an edge case");
}

#[tokio::test]
async fn delegate_grader_parses_a_failed_verdict() {
    let grader = DelegateGrader::new(
        Arc::new(StubRunner::replying(
            r#"{"result": "failed", "explanation": "rubric does not fit"}"#,
        )),
        "judge",
    );
    let v = grader
        .grade(&agent_goal(GraderRef::Default), "done", None)
        .await
        .unwrap();
    assert_eq!(v.result, GradeResult::Failed);
}

#[tokio::test]
async fn delegate_grader_reports_a_malformed_reply_as_error() {
    let grader = DelegateGrader::new(
        Arc::new(StubRunner::replying("I cannot produce JSON")),
        "judge",
    );
    assert!(
        grader
            .grade(&agent_goal(GraderRef::Default), "done", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn delegate_grader_reports_a_run_failure_as_error() {
    let grader = DelegateGrader::new(
        Arc::new(StubRunner {
            fail: true,
            ..StubRunner::default()
        }),
        "judge",
    );
    assert!(
        grader
            .grade(&agent_goal(GraderRef::Default), "done", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn delegate_grader_routes_default_and_explicit_agent() {
    // Default → the configured default judge.
    let runner = Arc::new(StubRunner::replying(
        r#"{"result": "satisfied", "explanation": "ok"}"#,
    ));
    let grader = DelegateGrader::new(runner.clone(), "default-judge");
    grader
        .grade(&agent_goal(GraderRef::Default), "done", None)
        .await
        .unwrap();
    assert_eq!(
        runner.seen_agent.lock().unwrap().as_deref(),
        Some("default-judge")
    );

    // Agent → the explicitly named judge.
    let runner = Arc::new(StubRunner::replying(
        r#"{"result": "satisfied", "explanation": "ok"}"#,
    ));
    let grader = DelegateGrader::new(runner.clone(), "default-judge");
    let goal = agent_goal(GraderRef::Agent {
        agent_id: "specialist".into(),
    });
    grader.grade(&goal, "done", None).await.unwrap();
    assert_eq!(
        runner.seen_agent.lock().unwrap().as_deref(),
        Some("specialist")
    );
}

#[tokio::test]
async fn delegate_grader_forwards_cancellation_into_the_judge() {
    // A runner that records whether the parent cancellation reached it.
    #[derive(Default)]
    struct CancelCapturingRunner {
        saw_cancellation: std::sync::Mutex<bool>,
    }
    #[async_trait]
    impl DelegateRunner for CancelCapturingRunner {
        async fn run(&self, request: DelegateRequest) -> Result<DelegateReply, DelegateError> {
            *self.saw_cancellation.lock().unwrap() = request.cancellation.is_some();
            Ok(DelegateReply {
                text: Some(r#"{"result": "satisfied", "explanation": "ok"}"#.into()),
            })
        }
    }
    let runner = Arc::new(CancelCapturingRunner::default());
    let grader = DelegateGrader::new(runner.clone(), "judge");
    let token = CancellationToken::new();
    grader
        .grade(&agent_goal(GraderRef::Default), "done", Some(&token))
        .await
        .unwrap();
    assert!(
        *runner.saw_cancellation.lock().unwrap(),
        "parent cancellation must reach the judge sub-run (no orphaned judge)"
    );
}

#[test]
fn judge_prompt_carries_the_rubric_and_deliverable() {
    let prompt = judge_prompt(&agent_goal(GraderRef::Default), "the deliverable");
    assert!(prompt.contains("tests pass")); // rubric
    assert!(prompt.contains("the deliverable"));
    assert!(prompt.contains("\"result\"")); // asks for the JSON shape
}

// ── Guard fail-open on a grader error ───────────────────────────────────────

/// A grader that always fails to judge.
struct ErrGrader;

#[async_trait]
impl Grader for ErrGrader {
    async fn grade(
        &self,
        _goal: &GoalSpec,
        _deliverable: &str,
        _cancellation: Option<&CancellationToken>,
    ) -> Result<Verdict, GraderError> {
        Err(GraderError("judge unavailable".into()))
    }
}

#[tokio::test]
async fn guard_fails_open_to_failed_when_the_grader_errors() {
    let guard = GoalGuard::new(spec(3), Arc::new(ErrGrader));
    let d = evaluate(&guard, &[assistant("done")], 0).await;
    // Fail-open: the run ends (terminal) rather than looping forever.
    assert!(matches!(d, RunEndDecision::Complete { .. }));
    assert_eq!(result_token(&d), "failed");
    assert_eq!(explanation(&d), "judge unavailable");
}

#[test]
fn goal_spec_defaults_grader_to_default() {
    let parsed: GoalSpec = serde_json::from_value(serde_json::json!({
        "description": "x",
        "rubric": "y",
        "max_iterations": 3
    }))
    .unwrap();
    assert_eq!(parsed.grader, GraderRef::Default);
}
