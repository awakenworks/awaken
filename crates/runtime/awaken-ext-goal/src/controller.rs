//! Outcome application service coordinating ordinary Worker and Grader Runs.
//!
//! The controller owns iteration, recovery, prompts, and state transitions. It
//! depends only on the existing Runtime Run/Thread ports; applications retain
//! backend selection, durable ingress, live context construction, locking, and
//! protocol projection.

use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::{
    EndCause, Message, MessageId, Role, RunActivation, RunId, RunState, RuntimeRunContext,
    ThreadId, ThreadReader,
};

use crate::outcome::{
    Definition, Evaluation, EvaluationResult, ExecutionFailure, Grade, GradeDecision, Grader,
    GraderError, GradingInput, Id, Phase, State, WorkerRunKind,
};
use crate::state::{
    Aggregate, Binding, Error as StateError, ThreadOutcomeState, acknowledgment_run_id,
    grader_run_id, worker_run_id,
};

#[derive(Debug, Clone, PartialEq)]
pub struct IterationReport {
    pub messages: Vec<Message>,
    pub outcome_id: Id,
    pub iteration: u32,
    pub result: EvaluationResult,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub iterations: Vec<IterationReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    ActiveDefinitionConflict { outcome_id: Id },
    WorkerAwaiting { run_id: RunId },
    Domain(String),
    Persistence(String),
    Execution(ExecutionFailure),
    Serialization(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ActiveDefinitionConflict { outcome_id } => write!(
                formatter,
                "Worker Thread already has active Outcome {} with a different definition",
                outcome_id.0
            ),
            Self::WorkerAwaiting { run_id } => write!(
                formatter,
                "Outcome Worker {} awaits external input; resume it before driving the Outcome",
                run_id.0
            ),
            Self::Domain(message) => write!(formatter, "Outcome transition failed: {message}"),
            Self::Persistence(message) => {
                write!(formatter, "Outcome persistence failed: {message}")
            }
            Self::Execution(failure) => write!(formatter, "Outcome execution failed: {failure:?}"),
            Self::Serialization(message) => {
                write!(formatter, "Outcome serialization failed: {message}")
            }
        }
    }
}

impl std::error::Error for Error {}

struct WorkerExecution {
    state: RunState,
    message_start: usize,
}

/// One Outcome controller bound to its Worker Thread and ordinary Run ports.
/// The object holds no mutable truth; every call rebuilds from committed Thread
/// state, so a replacement process may construct another controller and resume.
pub struct Controller<'a> {
    thread_id: &'a ThreadId,
    reader: &'a dyn ThreadReader,
    state: ThreadOutcomeState<'a>,
    executor: &'a dyn RunExecutor,
    run_context: RuntimeRunContext,
    grader: &'a dyn Grader,
}

impl<'a> Controller<'a> {
    #[must_use]
    pub fn new(
        thread_id: &'a ThreadId,
        reader: &'a dyn ThreadReader,
        coordinator: &'a dyn awaken_runtime_contract::CommitCoordinator,
        executor: &'a dyn RunExecutor,
        run_context: RuntimeRunContext,
        grader: &'a dyn Grader,
    ) -> Self {
        Self {
            thread_id,
            reader,
            state: ThreadOutcomeState::new(thread_id, reader, coordinator),
            executor,
            run_context,
            grader,
        }
    }

    /// Define a new Outcome or recover the active matching definition, then
    /// drive it until a terminal result or an external-input boundary.
    pub async fn define_or_resume(
        &self,
        outcome_id: Id,
        definition: Definition,
        binding: Binding,
    ) -> Result<Report, Error> {
        definition
            .validate()
            .map_err(|error| Error::Domain(error.to_string()))?;
        let mut aggregate = match self.state.active().map_err(state_error)? {
            Some(active) => {
                if active.definition != definition {
                    return Err(Error::ActiveDefinitionConflict {
                        outcome_id: active.state.outcome_id,
                    });
                }
                active
            }
            None => {
                let state = State::new(
                    outcome_id,
                    self.reader.committed_messages(self.thread_id).len(),
                );
                self.state
                    .create(&definition, &binding, &state)
                    .await
                    .map_err(state_error)?;
                Aggregate {
                    definition: definition.clone(),
                    binding,
                    state,
                    evaluations: Vec::new(),
                }
            }
        };

        loop {
            match aggregate.state.phase.clone() {
                Phase::Defined => {
                    let expected = aggregate.state.version;
                    aggregate
                        .state
                        .start(worker_run_id(&aggregate.state.outcome_id, 0))
                        .map_err(domain_error)?;
                    self.commit(expected, &aggregate.state, None).await?;
                }
                Phase::RunningWorker {
                    iteration,
                    run_id,
                    kind,
                } => {
                    let expected = aggregate.state.version;
                    let worker = self
                        .drive_worker(&aggregate, iteration, kind, run_id.clone())
                        .await?;
                    match worker.state {
                        RunState::Ended(EndCause::NaturalEnd) => {
                            let cursor = self.reader.committed_messages(self.thread_id).len();
                            aggregate
                                .state
                                .worker_completed(
                                    &run_id,
                                    grader_run_id(&aggregate.state.outcome_id, iteration),
                                    worker.message_start,
                                    cursor,
                                )
                                .map_err(domain_error)?;
                            self.commit(expected, &aggregate.state, None).await?;
                        }
                        RunState::Ended(EndCause::Cancelled) => {
                            aggregate.state.interrupt();
                            self.commit(expected, &aggregate.state, None).await?;
                        }
                        RunState::Awaiting => return Err(Error::WorkerAwaiting { run_id }),
                        state => {
                            aggregate
                                .state
                                .fail(ExecutionFailure::WorkerFailed(format!(
                                    "Worker Run ended in {state:?}"
                                )))
                                .map_err(domain_error)?;
                            self.commit(expected, &aggregate.state, None).await?;
                        }
                    }
                }
                Phase::Evaluating {
                    iteration,
                    grader_run_id,
                    message_start,
                } => {
                    let expected = aggregate.state.version;
                    let input = self.grading_input(&aggregate, iteration, message_start)?;
                    let grade = match self.grader.grade(&input).await {
                        Ok(grade) => grade,
                        Err(GraderError::Interrupted) => {
                            aggregate.state.interrupt();
                            self.commit(expected, &aggregate.state, None).await?;
                            continue;
                        }
                        Err(error) => {
                            let failure = match error {
                                GraderError::InvalidOutput(message) => {
                                    ExecutionFailure::InvalidGraderOutput(message)
                                }
                                GraderError::Execution(message) => {
                                    ExecutionFailure::GraderUnavailable(message)
                                }
                                GraderError::Interrupted => unreachable!(),
                            };
                            aggregate.state.fail(failure).map_err(domain_error)?;
                            self.commit(expected, &aggregate.state, None).await?;
                            continue;
                        }
                    };
                    let evaluation = Evaluation {
                        iteration,
                        worker_run_id: worker_run_id(&aggregate.state.outcome_id, iteration),
                        grader_run_id: grader_run_id.clone(),
                        message_start,
                        message_end: aggregate.state.transcript_cursor,
                        grade: grade.clone(),
                    };
                    aggregate
                        .state
                        .apply_grade(
                            &aggregate.definition,
                            &grader_run_id,
                            &grade,
                            next_run_id(&aggregate, iteration, &grade),
                        )
                        .map_err(domain_error)?;
                    self.commit(expected, &aggregate.state, Some(&evaluation))
                        .await?;
                    aggregate.evaluations.push(evaluation);
                }
                Phase::Acknowledging { run_id } => {
                    let expected = aggregate.state.version;
                    match self
                        .drive_acknowledgment(&aggregate, run_id.clone())
                        .await?
                    {
                        RunState::Ended(EndCause::NaturalEnd) => aggregate
                            .state
                            .acknowledgment_completed(&run_id)
                            .map_err(domain_error)?,
                        RunState::Ended(EndCause::Cancelled) => {
                            aggregate.state.interrupt();
                        }
                        other => aggregate
                            .state
                            .fail(ExecutionFailure::WorkerFailed(format!(
                                "acknowledgment Run ended in {other:?}"
                            )))
                            .map_err(domain_error)?,
                    }
                    self.commit(expected, &aggregate.state, None).await?;
                }
                Phase::Completed { .. } => return self.report(&aggregate),
                Phase::Errored { failure } => return Err(Error::Execution(failure)),
            }
        }
    }

    async fn drive_worker(
        &self,
        aggregate: &Aggregate,
        iteration: u32,
        kind: WorkerRunKind,
        run_id: RunId,
    ) -> Result<WorkerExecution, Error> {
        if let Some(state) = self.reader.run_state(&run_id)
            && state != RunState::Running
        {
            let transcript = self.reader.committed_messages(self.thread_id);
            let input_id = worker_input_id(&aggregate.state.outcome_id, iteration);
            let message_start = transcript
                .iter()
                .enumerate()
                .skip(aggregate.state.transcript_cursor)
                .find(|(_, message)| message.id.0 == input_id)
                .map(|(index, _)| index + 1)
                .ok_or_else(|| {
                    Error::Serialization(
                        "recovered Worker Run is missing its deterministic input".into(),
                    )
                })?;
            return Ok(WorkerExecution {
                state,
                message_start,
            });
        }
        let before = self.reader.committed_messages(self.thread_id).len();
        let input = Message::text(
            MessageId(worker_input_id(&aggregate.state.outcome_id, iteration)),
            Role::User,
            worker_prompt(aggregate, kind),
        );
        let activation = RunActivation::new(
            run_id,
            self.thread_id.clone(),
            aggregate.binding.worker.clone(),
            vec![input],
        );
        let state = self
            .executor
            .execute(activation, self.run_context.clone())
            .await
            .map_err(|error| Error::Execution(ExecutionFailure::WorkerFailed(error.to_string())))?;
        Ok(WorkerExecution {
            state,
            message_start: before.saturating_add(1),
        })
    }

    async fn drive_acknowledgment(
        &self,
        aggregate: &Aggregate,
        run_id: RunId,
    ) -> Result<RunState, Error> {
        if let Some(state) = self.reader.run_state(&run_id)
            && state != RunState::Running
        {
            return Ok(state);
        }
        let activation = RunActivation::new(
            run_id,
            self.thread_id.clone(),
            aggregate.binding.worker.clone(),
            vec![Message::text(
                MessageId(format!(
                    "outcome/{}/ack/input",
                    aggregate.state.outcome_id.0
                )),
                Role::User,
                acknowledgment_prompt(),
            )],
        );
        self.executor
            .execute(activation, self.run_context.clone())
            .await
            .map_err(|error| Error::Execution(ExecutionFailure::WorkerFailed(error.to_string())))
    }

    fn grading_input(
        &self,
        aggregate: &Aggregate,
        iteration: u32,
        message_start: usize,
    ) -> Result<GradingInput, Error> {
        Ok(GradingInput {
            outcome_id: aggregate.state.outcome_id.clone(),
            iteration,
            description: aggregate.definition.description.clone(),
            rubric: aggregate.definition.rubric.clone(),
            transcript: self.reader.committed_messages(self.thread_id),
            message_start,
            message_end: aggregate.state.transcript_cursor,
            worker_state: serde_json::to_value(&aggregate.state)
                .map_err(|error| Error::Serialization(error.to_string()))?,
            evidence: Vec::new(),
        })
    }

    async fn commit(
        &self,
        expected: u64,
        state: &State,
        evaluation: Option<&Evaluation>,
    ) -> Result<(), Error> {
        self.state
            .commit_if_current(expected, state, evaluation)
            .await
            .map_err(state_error)
    }

    fn report(&self, aggregate: &Aggregate) -> Result<Report, Error> {
        let transcript = self.reader.committed_messages(self.thread_id);
        let mut iterations = aggregate
            .evaluations
            .iter()
            .map(|evaluation| IterationReport {
                messages: transcript[evaluation.message_start.min(transcript.len())
                    ..evaluation.message_end.min(transcript.len())]
                    .to_vec(),
                outcome_id: aggregate.state.outcome_id.clone(),
                iteration: evaluation.iteration,
                result: evaluation_result(evaluation, &aggregate.definition),
                explanation: evaluation.grade.explanation.clone(),
            })
            .collect::<Vec<_>>();
        if matches!(
            aggregate.state.phase,
            Phase::Completed {
                result: EvaluationResult::MaxIterationsReached
            }
        ) && let Some(last) = iterations.last_mut()
        {
            last.messages.extend_from_slice(
                &transcript[aggregate.state.transcript_cursor.min(transcript.len())..],
            );
        }
        if matches!(
            aggregate.state.phase,
            Phase::Completed {
                result: EvaluationResult::Interrupted
            }
        ) {
            iterations.push(IterationReport {
                messages: Vec::new(),
                outcome_id: aggregate.state.outcome_id.clone(),
                iteration: aggregate.state.iteration,
                result: EvaluationResult::Interrupted,
                explanation: "the outcome was interrupted".into(),
            });
        }
        Ok(Report { iterations })
    }
}

fn worker_prompt(aggregate: &Aggregate, kind: WorkerRunKind) -> String {
    match kind {
        WorkerRunKind::Initial => format!(
            "Work toward this Outcome and produce the deliverable.\nDescription: {}\nRubric: {}",
            aggregate.definition.description, aggregate.definition.rubric.0
        ),
        WorkerRunKind::Revision => format!(
            "Revise the deliverable for this Outcome. Grader feedback: {}",
            aggregate
                .evaluations
                .last()
                .map(|evaluation| evaluation.grade.explanation.as_str())
                .unwrap_or("the rubric is not yet satisfied")
        ),
    }
}

fn acknowledgment_prompt() -> &'static str {
    "The Outcome iteration limit was reached. Briefly acknowledge the remaining Grader feedback without starting another graded revision."
}

fn next_run_id(aggregate: &Aggregate, iteration: u32, grade: &Grade) -> RunId {
    if grade.decision == GradeDecision::NeedsRevision
        && iteration.saturating_add(1) >= aggregate.definition.max_iterations
    {
        acknowledgment_run_id(&aggregate.state.outcome_id)
    } else {
        worker_run_id(&aggregate.state.outcome_id, iteration.saturating_add(1))
    }
}

fn evaluation_result(evaluation: &Evaluation, definition: &Definition) -> EvaluationResult {
    match evaluation.grade.decision {
        GradeDecision::Satisfied => EvaluationResult::Satisfied,
        GradeDecision::NeedsRevision
            if evaluation.iteration.saturating_add(1) >= definition.max_iterations =>
        {
            EvaluationResult::MaxIterationsReached
        }
        GradeDecision::NeedsRevision => EvaluationResult::NeedsRevision,
        GradeDecision::Failed => EvaluationResult::Failed,
    }
}

fn worker_input_id(id: &Id, iteration: u32) -> String {
    format!("outcome/{}/worker/{iteration}/input", id.0)
}

fn domain_error(error: crate::outcome::Error) -> Error {
    Error::Domain(error.to_string())
}

fn state_error(error: StateError) -> Error {
    Error::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use awaken_runtime_contract::{
        CommitCoordinator, CommitError, CommitRecord, ResumeTicket, StateCommand, ThreadCommit,
    };

    use super::*;
    use crate::outcome::{KeywordGrader, Rubric};

    struct World {
        commits: Mutex<Vec<ThreadCommit>>,
        replies: Mutex<VecDeque<String>>,
        states: Mutex<VecDeque<RunState>>,
        executions: AtomicUsize,
    }

    impl World {
        fn new(replies: &[&str]) -> Self {
            Self {
                commits: Mutex::new(Vec::new()),
                replies: Mutex::new(replies.iter().map(|reply| (*reply).into()).collect()),
                states: Mutex::new(VecDeque::new()),
                executions: AtomicUsize::new(0),
            }
        }

        fn with_states(self, states: Vec<RunState>) -> Self {
            *self.states.lock().unwrap() = states.into();
            self
        }
    }

    #[async_trait]
    impl awaken_runtime_contract::CommitCoordinator for World {
        async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
            let mut commits = self.commits.lock().unwrap();
            commits.push(commit);
            Ok(CommitRecord {
                sequence: commits.len() as u64,
            })
        }
    }

    impl ThreadReader for World {
        fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
            self.commits
                .lock()
                .unwrap()
                .iter()
                .filter(|commit| &commit.thread_id == thread_id)
                .flat_map(|commit| commit.messages.iter().cloned())
                .collect()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn run_state(&self, run_id: &RunId) -> Option<RunState> {
            self.commits
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run_id() == run_id)
                .map(ThreadCommit::run_state)
        }

        fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
            self.commits
                .lock()
                .unwrap()
                .iter()
                .filter(|commit| &commit.thread_id == thread_id)
                .flat_map(|commit| commit.state.iter().cloned())
                .collect()
        }
    }

    #[async_trait]
    impl RunExecutor for World {
        async fn execute(
            &self,
            activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> awaken_runtime_contract::execution::Result<RunState> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let state = self
                .states
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(RunState::Ended(EndCause::NaturalEnd));
            let mut messages = activation.input;
            if state == RunState::Ended(EndCause::NaturalEnd) {
                let reply = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "done".into());
                messages.push(Message::text(
                    MessageId(format!("{}/reply", activation.run_id.0)),
                    Role::Assistant,
                    reply,
                ));
            }
            self.commit(ThreadCommit::assemble(
                activation.thread_id,
                match &state {
                    RunState::Ended(cause) => awaken_runtime_contract::RunDisposition::ended(
                        activation.run_id,
                        cause.clone(),
                    ),
                    RunState::Awaiting => return Ok(RunState::Awaiting),
                    RunState::Running => {
                        awaken_runtime_contract::RunDisposition::running(activation.run_id)
                    }
                },
                false,
                messages,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .map_err(|error| {
                awaken_runtime_contract::execution::Error::Execution(error.to_string())
            })?;
            Ok(state)
        }
    }

    fn binding() -> Binding {
        Binding {
            worker: awaken_runtime_contract::ExecutableAgentSnapshot::builder("worker").build(),
            grader: awaken_runtime_contract::ExecutableAgentSnapshot::builder("grader").build(),
        }
    }

    async fn drive(world: &World, max_iterations: u32) -> Result<Report, Error> {
        let thread = ThreadId("worker-thread".into());
        let grader = KeywordGrader;
        Controller::new(
            &thread,
            world,
            world,
            world,
            RuntimeRunContext::new(),
            &grader,
        )
        .define_or_resume(
            Id("outcome-1".into()),
            Definition {
                description: "ship".into(),
                rubric: Rubric("FINAL".into()),
                max_iterations,
            },
            binding(),
        )
        .await
    }

    #[tokio::test]
    async fn controller_drives_satisfaction_revision_and_acknowledgment() {
        let satisfied = World::new(&["FINAL"]);
        let report = drive(&satisfied, 3).await.unwrap();
        assert_eq!(report.iterations.len(), 1);
        assert_eq!(report.iterations[0].result, EvaluationResult::Satisfied);

        let revised = World::new(&["draft", "FINAL"]);
        let report = drive(&revised, 3).await.unwrap();
        assert_eq!(
            report
                .iterations
                .iter()
                .map(|iteration| iteration.result)
                .collect::<Vec<_>>(),
            vec![EvaluationResult::NeedsRevision, EvaluationResult::Satisfied]
        );

        let exhausted = World::new(&["draft", "acknowledged"]);
        let report = drive(&exhausted, 1).await.unwrap();
        assert_eq!(
            report.iterations[0].result,
            EvaluationResult::MaxIterationsReached
        );
        assert!(
            report.iterations[0]
                .messages
                .iter()
                .any(|message| message.text_content() == "acknowledged")
        );
    }

    #[tokio::test]
    async fn awaiting_worker_is_an_external_boundary_not_a_business_failure() {
        let world = World::new(&[]).with_states(vec![RunState::Awaiting]);
        assert!(matches!(
            drive(&world, 2).await,
            Err(Error::WorkerAwaiting { .. })
        ));
    }

    #[tokio::test]
    async fn restart_after_worker_commit_reuses_the_stable_run() {
        let world = World::new(&["FINAL"]);
        let thread = ThreadId("worker-thread".into());
        let definition = Definition::new("ship", "FINAL", 2).unwrap();
        let binding = binding();
        let outcome_id = Id("outcome-1".into());
        let mut state = State::new(outcome_id.clone(), 0);
        let persistence = ThreadOutcomeState::new(&thread, &world, &world);
        persistence
            .create(&definition, &binding, &state)
            .await
            .unwrap();
        state.start(worker_run_id(&outcome_id, 0)).unwrap();
        persistence
            .commit_if_current(0, &state, None)
            .await
            .unwrap();

        world
            .execute(
                RunActivation::new(
                    worker_run_id(&outcome_id, 0),
                    thread.clone(),
                    binding.worker.clone(),
                    vec![Message::text(
                        MessageId(worker_input_id(&outcome_id, 0)),
                        Role::User,
                        "work",
                    )],
                ),
                RuntimeRunContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(world.executions.load(Ordering::SeqCst), 1);

        let grader = KeywordGrader;
        let report = Controller::new(
            &thread,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        )
        .define_or_resume(outcome_id, definition, binding)
        .await
        .unwrap();
        assert_eq!(report.iterations[0].result, EvaluationResult::Satisfied);
        assert_eq!(
            world.executions.load(Ordering::SeqCst),
            1,
            "the committed Worker Run must be observed, not inferred twice"
        );
    }
}
