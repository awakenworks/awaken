//! Pure Outcome lifecycle rules.
//!
//! This module owns no Run execution, persistence, protocol projection, prompt,
//! tool, or Agent implementation. An application controller supplies stable Run
//! identities and persists the resulting state transitions.

use async_trait::async_trait;
use awaken_runtime_contract::Message;
use awaken_runtime_contract::RunId;
use serde::{Deserialize, Serialize};

pub const MIN_ITERATIONS: u32 = 1;
pub const MAX_ITERATIONS: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rubric(pub String);

/// Immutable business definition supplied at `define_outcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub description: String,
    pub rubric: Rubric,
    pub max_iterations: u32,
}

impl Definition {
    pub fn new(
        description: impl Into<String>,
        rubric: impl Into<String>,
        max_iterations: u32,
    ) -> Result<Self, Error> {
        let definition = Self {
            description: description.into(),
            rubric: Rubric(rubric.into()),
            max_iterations,
        };
        definition.validate()?;
        Ok(definition)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.description.trim().is_empty() {
            return Err(Error::InvalidDefinition(
                "Outcome description must not be empty".into(),
            ));
        }
        if self.rubric.0.trim().is_empty() {
            return Err(Error::InvalidDefinition(
                "Outcome rubric must not be empty".into(),
            ));
        }
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&self.max_iterations) {
            return Err(Error::InvalidDefinition(format!(
                "Outcome max_iterations must be in {MIN_ITERATIONS}..={MAX_ITERATIONS}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRunKind {
    Initial,
    Revision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GradeDecision {
    Satisfied,
    NeedsRevision,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grade {
    pub decision: GradeDecision,
    pub explanation: String,
}

/// Immutable evidence that one Worker result was graded. The message range is
/// half-open (`start..end`) in the committed Worker transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub iteration: u32,
    pub worker_run_id: RunId,
    pub grader_run_id: RunId,
    pub message_start: usize,
    pub message_end: usize,
    pub grade: Grade,
}

/// A deterministic, already-prepared item the Grader may inspect. Preparing
/// binary/file evidence is an application concern; the Judge receives no Worker
/// workspace or discovery tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverableEvidence {
    pub kind: String,
    pub locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradingInput {
    pub outcome_id: Id,
    pub iteration: u32,
    pub description: String,
    pub rubric: Rubric,
    pub transcript: Vec<Message>,
    pub message_start: usize,
    pub message_end: usize,
    pub worker_state: serde_json::Value,
    pub evidence: Vec<DeliverableEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraderError {
    Execution(String),
    InvalidOutput(String),
    Interrupted,
}

impl std::fmt::Display for GraderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Execution(message) => write!(formatter, "Grader execution failed: {message}"),
            Self::InvalidOutput(message) => write!(formatter, "invalid Grader output: {message}"),
            Self::Interrupted => formatter.write_str("Grader execution was interrupted"),
        }
    }
}

impl std::error::Error for GraderError {}

#[async_trait]
pub trait Grader: Send + Sync {
    async fn grade(&self, input: &GradingInput) -> Result<Grade, GraderError>;
}

/// Offline reference Grader used by deterministic tests and local demos.
pub struct KeywordGrader;

#[async_trait]
impl Grader for KeywordGrader {
    async fn grade(&self, input: &GradingInput) -> Result<Grade, GraderError> {
        let deliverable = input.transcript[input.message_start.min(input.transcript.len())
            ..input.message_end.min(input.transcript.len())]
            .iter()
            .map(Message::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        let satisfied = input.rubric.0.is_empty() || deliverable.contains(&input.rubric.0);
        Ok(Grade {
            decision: if satisfied {
                GradeDecision::Satisfied
            } else {
                GradeDecision::NeedsRevision
            },
            explanation: if satisfied {
                "deliverable satisfies the rubric".into()
            } else {
                format!("deliverable must satisfy the rubric ({:?})", input.rubric.0)
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GradeWire {
    result: GradeDecision,
    explanation: String,
}

/// Parse the complete Judge reply. Markdown fences, surrounding prose, unknown
/// fields, missing explanations, and invalid decision tokens all fail closed.
pub fn parse_grade(reply: &str) -> Result<Grade, GraderError> {
    let wire: GradeWire = serde_json::from_str(reply)
        .map_err(|error| GraderError::InvalidOutput(error.to_string()))?;
    if wire.explanation.trim().is_empty() {
        return Err(GraderError::InvalidOutput(
            "explanation must not be empty".into(),
        ));
    }
    Ok(Grade {
        decision: wire.result,
        explanation: wire.explanation,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationResult {
    Satisfied,
    NeedsRevision,
    MaxIterationsReached,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GradeTransition {
    Complete(EvaluationResult),
    Revise(u32),
    Acknowledge,
}

const fn grade_transition(
    decision: GradeDecision,
    iteration: u32,
    max_iterations: u32,
) -> GradeTransition {
    match decision {
        GradeDecision::Satisfied => GradeTransition::Complete(EvaluationResult::Satisfied),
        GradeDecision::Failed => GradeTransition::Complete(EvaluationResult::Failed),
        GradeDecision::NeedsRevision if iteration.saturating_add(1) >= max_iterations => {
            GradeTransition::Acknowledge
        }
        GradeDecision::NeedsRevision => GradeTransition::Revise(iteration + 1),
    }
}

impl EvaluationResult {
    pub const fn token(self) -> &'static str {
        match self {
            Self::Satisfied => "satisfied",
            Self::NeedsRevision => "needs_revision",
            Self::MaxIterationsReached => "max_iterations_reached",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Infrastructure failures are not Managed's business `failed` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFailure {
    WorkerFailed(String),
    GraderUnavailable(String),
    InvalidGraderOutput(String),
    Persistence(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum Phase {
    Defined,
    RunningWorker {
        iteration: u32,
        run_id: RunId,
        kind: WorkerRunKind,
    },
    Evaluating {
        iteration: u32,
        grader_run_id: RunId,
        message_start: usize,
    },
    Acknowledging {
        run_id: RunId,
    },
    Completed {
        result: EvaluationResult,
    },
    Errored {
        failure: ExecutionFailure,
    },
}

impl Phase {
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Errored { .. })
    }
}

/// Mutable aggregate head. Immutable Definition and execution bindings are
/// persisted separately so this CAS cell stays small.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub outcome_id: Id,
    pub phase: Phase,
    /// Zero-based revision counter, equal to the evaluation index.
    pub iteration: u32,
    pub transcript_cursor: usize,
    pub version: u64,
}

impl State {
    pub fn new(outcome_id: Id, transcript_cursor: usize) -> Self {
        Self {
            outcome_id,
            phase: Phase::Defined,
            iteration: 0,
            transcript_cursor,
            version: 0,
        }
    }

    pub fn start(&mut self, run_id: RunId) -> Result<(), Error> {
        self.require_phase("start", |phase| matches!(phase, Phase::Defined))?;
        self.phase = Phase::RunningWorker {
            iteration: 0,
            run_id,
            kind: WorkerRunKind::Initial,
        };
        self.advance();
        Ok(())
    }

    pub fn worker_completed(
        &mut self,
        expected_run_id: &RunId,
        grader_run_id: RunId,
        message_start: usize,
        transcript_cursor: usize,
    ) -> Result<(), Error> {
        let iteration = match &self.phase {
            Phase::RunningWorker {
                iteration, run_id, ..
            } if run_id == expected_run_id => *iteration,
            Phase::RunningWorker { run_id, .. } => {
                return Err(Error::StaleRun {
                    expected: run_id.clone(),
                    actual: expected_run_id.clone(),
                });
            }
            phase => return Err(Error::invalid_transition("worker_completed", phase)),
        };
        if message_start < self.transcript_cursor {
            return Err(Error::CursorRegression {
                current: self.transcript_cursor,
                proposed: message_start,
            });
        }
        if message_start > transcript_cursor {
            return Err(Error::InvalidMessageRange {
                start: message_start,
                end: transcript_cursor,
            });
        }
        self.transcript_cursor = transcript_cursor;
        self.iteration = iteration;
        self.phase = Phase::Evaluating {
            iteration,
            grader_run_id,
            message_start,
        };
        self.advance();
        Ok(())
    }

    /// Apply one Grade and select the next phase. `next_run_id` is the stable
    /// revision or acknowledgment Run identity minted by the controller.
    pub fn apply_grade(
        &mut self,
        definition: &Definition,
        expected_grader_run_id: &RunId,
        grade: &Grade,
        next_run_id: RunId,
    ) -> Result<EvaluationResult, Error> {
        definition.validate()?;
        let iteration = match &self.phase {
            Phase::Evaluating {
                iteration,
                grader_run_id,
                ..
            } if grader_run_id == expected_grader_run_id => *iteration,
            Phase::Evaluating { grader_run_id, .. } => {
                return Err(Error::StaleRun {
                    expected: grader_run_id.clone(),
                    actual: expected_grader_run_id.clone(),
                });
            }
            phase => return Err(Error::invalid_transition("apply_grade", phase)),
        };

        let result = match grade_transition(grade.decision, iteration, definition.max_iterations) {
            GradeTransition::Complete(result) => {
                self.phase = Phase::Completed { result };
                result
            }
            GradeTransition::Acknowledge => {
                self.phase = Phase::Acknowledging {
                    run_id: next_run_id,
                };
                EvaluationResult::MaxIterationsReached
            }
            GradeTransition::Revise(next_iteration) => {
                self.iteration = next_iteration;
                self.phase = Phase::RunningWorker {
                    iteration: next_iteration,
                    run_id: next_run_id,
                    kind: WorkerRunKind::Revision,
                };
                EvaluationResult::NeedsRevision
            }
        };
        self.advance();
        Ok(result)
    }

    pub fn acknowledgment_completed(&mut self, expected_run_id: &RunId) -> Result<(), Error> {
        match &self.phase {
            Phase::Acknowledging { run_id } if run_id == expected_run_id => {}
            Phase::Acknowledging { run_id } => {
                return Err(Error::StaleRun {
                    expected: run_id.clone(),
                    actual: expected_run_id.clone(),
                });
            }
            phase => return Err(Error::invalid_transition("acknowledgment_completed", phase)),
        }
        self.phase = Phase::Completed {
            result: EvaluationResult::MaxIterationsReached,
        };
        self.advance();
        Ok(())
    }

    /// Idempotent interrupt: the first live transition wins; a repeated command
    /// observes the same terminal truth and reports `false`.
    pub fn interrupt(&mut self) -> bool {
        if self.phase.is_terminal() {
            return false;
        }
        self.phase = Phase::Completed {
            result: EvaluationResult::Interrupted,
        };
        self.advance();
        true
    }

    pub fn fail(&mut self, failure: ExecutionFailure) -> Result<(), Error> {
        if self.phase.is_terminal() {
            return Err(Error::invalid_transition("fail", &self.phase));
        }
        self.phase = Phase::Errored { failure };
        self.advance();
        Ok(())
    }

    fn require_phase(
        &self,
        operation: &'static str,
        predicate: impl FnOnce(&Phase) -> bool,
    ) -> Result<(), Error> {
        if predicate(&self.phase) {
            Ok(())
        } else {
            Err(Error::invalid_transition(operation, &self.phase))
        }
    }

    fn advance(&mut self) {
        self.version = self.version.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidDefinition(String),
    InvalidTransition {
        operation: &'static str,
        phase: &'static str,
    },
    StaleRun {
        expected: RunId,
        actual: RunId,
    },
    CursorRegression {
        current: usize,
        proposed: usize,
    },
    InvalidMessageRange {
        start: usize,
        end: usize,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDefinition(message) => {
                write!(formatter, "invalid Outcome definition: {message}")
            }
            Self::InvalidTransition { operation, phase } => {
                write!(formatter, "cannot {operation} while Outcome is in {phase}")
            }
            Self::StaleRun { expected, actual } => {
                write!(
                    formatter,
                    "stale Run: expected {expected:?}, received {actual:?}"
                )
            }
            Self::CursorRegression { current, proposed } => {
                write!(
                    formatter,
                    "transcript cursor regressed from {current} to {proposed}"
                )
            }
            Self::InvalidMessageRange { start, end } => {
                write!(formatter, "invalid evaluated message range {start}..{end}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    fn invalid_transition(operation: &'static str, phase: &Phase) -> Self {
        let phase = match phase {
            Phase::Defined => "defined",
            Phase::RunningWorker { .. } => "running_worker",
            Phase::Evaluating { .. } => "evaluating",
            Phase::Acknowledging { .. } => "acknowledging",
            Phase::Completed { .. } => "completed",
            Phase::Errored { .. } => "errored",
        };
        Self::InvalidTransition { operation, phase }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn applying_a_grade_obeys_decision_and_budget() {
        let max_iterations: u32 = kani::any();
        let iteration: u32 = kani::any();
        let decision_code: u8 = kani::any();
        kani::assume((MIN_ITERATIONS..=MAX_ITERATIONS).contains(&max_iterations));
        kani::assume(iteration < max_iterations);
        kani::assume(decision_code <= 2);

        let decision = match decision_code {
            0 => GradeDecision::Satisfied,
            1 => GradeDecision::Failed,
            _ => GradeDecision::NeedsRevision,
        };
        let transition = grade_transition(decision, iteration, max_iterations);
        match decision {
            GradeDecision::Satisfied => {
                assert_eq!(
                    transition,
                    GradeTransition::Complete(EvaluationResult::Satisfied)
                );
            }
            GradeDecision::Failed => {
                assert_eq!(
                    transition,
                    GradeTransition::Complete(EvaluationResult::Failed)
                );
            }
            GradeDecision::NeedsRevision if iteration + 1 >= max_iterations => {
                assert_eq!(transition, GradeTransition::Acknowledge);
            }
            GradeDecision::NeedsRevision => {
                assert_eq!(transition, GradeTransition::Revise(iteration + 1));
            }
        }
    }

    #[kani::proof]
    fn terminal_outcomes_are_absorbing() {
        let result_code: u8 = kani::any();
        kani::assume(result_code <= 4);
        let result = match result_code {
            0 => EvaluationResult::Satisfied,
            1 => EvaluationResult::NeedsRevision,
            2 => EvaluationResult::MaxIterationsReached,
            3 => EvaluationResult::Failed,
            _ => EvaluationResult::Interrupted,
        };
        assert!(Phase::Completed { result }.is_terminal());
        assert!(
            Phase::Errored {
                failure: ExecutionFailure::Persistence(String::new())
            }
            .is_terminal()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{MessageId, Role};

    fn definition(max_iterations: u32) -> Definition {
        Definition::new("ship", "all tests pass", max_iterations).unwrap()
    }

    fn run(id: &str) -> RunId {
        RunId(id.into())
    }

    fn grade(decision: GradeDecision) -> Grade {
        Grade {
            decision,
            explanation: "because".into(),
        }
    }

    fn evaluating(max_iterations: u32) -> (Definition, State, RunId) {
        let definition = definition(max_iterations);
        let mut state = State::new(Id("o1".into()), 2);
        let worker = run("worker-0");
        let grader = run("grader-0");
        state.start(worker.clone()).unwrap();
        state
            .worker_completed(&worker, grader.clone(), 2, 4)
            .unwrap();
        (definition, state, grader)
    }

    // Cause-effect table D1: empty description/rubric and budgets outside 1..=20
    // independently cause definition rejection; both inclusive boundaries pass.
    #[test]
    fn definition_validation_decision_table() {
        for (description, rubric, budget, valid) in [
            ("", "r", 1, false),
            ("d", "", 1, false),
            ("d", "r", 0, false),
            ("d", "r", 1, true),
            ("d", "r", 20, true),
            ("d", "r", 21, false),
        ] {
            assert_eq!(Definition::new(description, rubric, budget).is_ok(), valid);
        }
    }

    #[test]
    fn initial_worker_and_evaluation_use_zero_based_iteration() {
        let mut state = State::new(Id("o".into()), 3);
        state.start(run("w0")).unwrap();
        assert!(matches!(
            state.phase,
            Phase::RunningWorker {
                iteration: 0,
                kind: WorkerRunKind::Initial,
                ..
            }
        ));
        state.worker_completed(&run("w0"), run("g0"), 3, 8).unwrap();
        assert!(matches!(
            state.phase,
            Phase::Evaluating { iteration: 0, .. }
        ));
        assert_eq!(state.transcript_cursor, 8);
        assert_eq!(state.version, 2);
    }

    // Cause-effect table D2: the three mutually exclusive Grade decisions select
    // satisfied, failed, or revision; budget exhaustion masks revision.
    #[test]
    fn grade_decision_table() {
        let (definition, mut satisfied, grader) = evaluating(3);
        assert_eq!(
            satisfied
                .apply_grade(
                    &definition,
                    &grader,
                    &grade(GradeDecision::Satisfied),
                    run("unused")
                )
                .unwrap(),
            EvaluationResult::Satisfied
        );
        assert!(matches!(
            satisfied.phase,
            Phase::Completed {
                result: EvaluationResult::Satisfied
            }
        ));

        let (definition, mut failed, grader) = evaluating(3);
        assert_eq!(
            failed
                .apply_grade(
                    &definition,
                    &grader,
                    &grade(GradeDecision::Failed),
                    run("unused")
                )
                .unwrap(),
            EvaluationResult::Failed
        );

        let (definition, mut revision, grader) = evaluating(3);
        assert_eq!(
            revision
                .apply_grade(
                    &definition,
                    &grader,
                    &grade(GradeDecision::NeedsRevision),
                    run("w1")
                )
                .unwrap(),
            EvaluationResult::NeedsRevision
        );
        assert!(matches!(
            revision.phase,
            Phase::RunningWorker {
                iteration: 1,
                kind: WorkerRunKind::Revision,
                ..
            }
        ));
    }

    #[test]
    fn last_allowed_evaluation_selects_ungraded_acknowledgment() {
        let (definition, mut state, grader0) = evaluating(2);
        state
            .apply_grade(
                &definition,
                &grader0,
                &grade(GradeDecision::NeedsRevision),
                run("w1"),
            )
            .unwrap();
        state.worker_completed(&run("w1"), run("g1"), 4, 6).unwrap();
        assert_eq!(
            state
                .apply_grade(
                    &definition,
                    &run("g1"),
                    &grade(GradeDecision::NeedsRevision),
                    run("ack")
                )
                .unwrap(),
            EvaluationResult::MaxIterationsReached
        );
        assert!(matches!(state.phase, Phase::Acknowledging { .. }));
        state.acknowledgment_completed(&run("ack")).unwrap();
        assert!(matches!(
            state.phase,
            Phase::Completed {
                result: EvaluationResult::MaxIterationsReached
            }
        ));
    }

    #[test]
    fn budget_one_moves_first_unmet_grade_to_acknowledgment() {
        let (definition, mut state, grader) = evaluating(1);
        let result = state
            .apply_grade(
                &definition,
                &grader,
                &grade(GradeDecision::NeedsRevision),
                run("ack"),
            )
            .unwrap();
        assert_eq!(result, EvaluationResult::MaxIterationsReached);
        assert!(matches!(state.phase, Phase::Acknowledging { .. }));
    }

    #[test]
    fn stale_worker_and_grader_results_cannot_advance_state() {
        let (definition, mut state, grader) = evaluating(3);
        let version = state.version;
        assert!(matches!(
            state.apply_grade(
                &definition,
                &run("wrong-grader"),
                &grade(GradeDecision::Satisfied),
                run("unused")
            ),
            Err(Error::StaleRun { .. })
        ));
        assert_eq!(state.version, version);

        state
            .apply_grade(
                &definition,
                &grader,
                &grade(GradeDecision::NeedsRevision),
                run("w1"),
            )
            .unwrap();
        let version = state.version;
        assert!(matches!(
            state.worker_completed(&run("wrong-worker"), run("g1"), 4, 7),
            Err(Error::StaleRun { .. })
        ));
        assert_eq!(state.version, version);
    }

    #[test]
    fn transcript_cursor_never_regresses() {
        let mut state = State::new(Id("o".into()), 10);
        state.start(run("w0")).unwrap();
        assert_eq!(
            state.worker_completed(&run("w0"), run("g0"), 9, 9),
            Err(Error::CursorRegression {
                current: 10,
                proposed: 9
            })
        );
        assert_eq!(state.version, 1);
    }

    #[test]
    fn interrupt_is_live_once_and_idempotent_after_terminal() {
        for mut state in [
            State::new(Id("defined".into()), 0),
            {
                let mut state = State::new(Id("worker".into()), 0);
                state.start(run("w")).unwrap();
                state
            },
            {
                let (_, state, _) = evaluating(2);
                state
            },
        ] {
            assert!(state.interrupt());
            let version = state.version;
            assert!(!state.interrupt());
            assert_eq!(state.version, version);
            assert!(matches!(
                state.phase,
                Phase::Completed {
                    result: EvaluationResult::Interrupted
                }
            ));
        }
    }

    #[test]
    fn infrastructure_failure_is_distinct_and_terminal() {
        let mut state = State::new(Id("o".into()), 0);
        state.start(run("w")).unwrap();
        state
            .fail(ExecutionFailure::WorkerFailed("boom".into()))
            .unwrap();
        assert!(matches!(state.phase, Phase::Errored { .. }));
        assert!(
            state
                .fail(ExecutionFailure::Persistence("late".into()))
                .is_err()
        );
        assert!(!state.interrupt());
    }

    #[test]
    fn terminal_business_results_are_immutable() {
        let (definition, mut state, grader) = evaluating(2);
        state
            .apply_grade(
                &definition,
                &grader,
                &grade(GradeDecision::Satisfied),
                run("unused"),
            )
            .unwrap();
        let version = state.version;
        assert!(state.start(run("again")).is_err());
        assert!(
            state
                .worker_completed(&run("again"), run("g"), 20, 20)
                .is_err()
        );
        assert_eq!(state.version, version);
    }

    #[test]
    fn wire_tokens_match_managed_outcome_results() {
        assert_eq!(EvaluationResult::Satisfied.token(), "satisfied");
        assert_eq!(EvaluationResult::NeedsRevision.token(), "needs_revision");
        assert_eq!(
            EvaluationResult::MaxIterationsReached.token(),
            "max_iterations_reached"
        );
        assert_eq!(EvaluationResult::Failed.token(), "failed");
        assert_eq!(EvaluationResult::Interrupted.token(), "interrupted");
    }

    #[test]
    fn state_round_trips_for_durable_thread_storage() {
        let (_, state, _) = evaluating(3);
        let json = serde_json::to_string(&state).unwrap();
        let restored: State = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, state);
    }

    #[test]
    fn strict_grade_parser_accepts_exact_schema_and_all_decisions() {
        for (token, decision) in [
            ("satisfied", GradeDecision::Satisfied),
            ("needs_revision", GradeDecision::NeedsRevision),
            ("failed", GradeDecision::Failed),
        ] {
            let grade =
                parse_grade(&format!(r#"{{"result":"{token}","explanation":"reason"}}"#)).unwrap();
            assert_eq!(grade.decision, decision);
        }
    }

    #[test]
    fn strict_grade_parser_rejects_non_schema_outputs() {
        for reply in [
            "",
            "prose {\"result\":\"satisfied\",\"explanation\":\"ok\"}",
            "```json\n{\"result\":\"satisfied\",\"explanation\":\"ok\"}\n```",
            r#"{"result":"unknown","explanation":"x"}"#,
            r#"{"result":"satisfied"}"#,
            r#"{"result":"satisfied","explanation":""}"#,
            r#"{"result":"satisfied","explanation":"x","extra":true}"#,
        ] {
            assert!(
                parse_grade(reply).is_err(),
                "unexpectedly accepted {reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn keyword_grader_uses_only_the_evaluated_message_range() {
        let input = GradingInput {
            outcome_id: Id("o".into()),
            iteration: 0,
            description: "ship".into(),
            rubric: Rubric("PASS".into()),
            transcript: vec![
                Message::text(MessageId("old".into()), Role::Assistant, "PASS"),
                Message::text(MessageId("new".into()), Role::Assistant, "not yet"),
            ],
            message_start: 1,
            message_end: 2,
            worker_state: serde_json::json!({}),
            evidence: Vec::new(),
        };
        assert_eq!(
            KeywordGrader.grade(&input).await.unwrap().decision,
            GradeDecision::NeedsRevision
        );
    }
}
