//! Outcome application service coordinating ordinary Worker and Grader Runs.
//!
//! The controller owns iteration, recovery, prompts, and state transitions. It
//! depends only on the existing Runtime Run/Thread ports; applications retain
//! backend selection, durable ingress, live context construction, locking, and
//! protocol projection.

#[cfg(test)]
use awaken_runtime_contract::RunRecord;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::{
    CommitOperationCoordinator, CommittedThreadView, EndCause, Message, MessageId, Role,
    RunActivation, RunId, RunRecoverySource, RunState, RuntimeRunContext, ThreadId,
    TranscriptRange, TranscriptSliceSpec, TranscriptView,
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
    pub description: String,
    pub iteration: u32,
    pub result: EvaluationResult,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub iterations: Vec<IterationReport>,
}

/// One exact terminal projection rebuilt from the Outcome's committed Thread
/// prefix. Infrastructure failure stays distinct from a rubric `Failed` report;
/// `source_run_id` correlates the ordinary Run lifecycle without storing a
/// second failure fact.
#[derive(Debug, Clone, PartialEq)]
pub enum CommittedOutcome {
    Completed(Report),
    Errored {
        failure: ExecutionFailure,
        source_run_id: Option<RunId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    ActiveDefinitionConflict { outcome_id: Id },
    Busy { outcome_id: Id },
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
                "Outcome {} was replayed with a different definition",
                outcome_id.0
            ),
            Self::Busy { outcome_id } => write!(
                formatter,
                "Worker Thread already has active Outcome {}",
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
    reader: &'a dyn CommittedThreadView,
    state: ThreadOutcomeState<'a>,
    executor: &'a dyn RunExecutor,
    run_context: RuntimeRunContext,
    grader: &'a dyn Grader,
}

impl<'a> Controller<'a> {
    #[must_use]
    pub fn new(
        thread_id: &'a ThreadId,
        reader: &'a dyn CommittedThreadView,
        recovery: &'a dyn RunRecoverySource,
        coordinator: &'a dyn CommitOperationCoordinator,
        executor: &'a dyn RunExecutor,
        run_context: RuntimeRunContext,
        grader: &'a dyn Grader,
    ) -> Self {
        Self {
            thread_id,
            reader,
            state: ThreadOutcomeState::new(thread_id, recovery, coordinator),
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
        let _ = self.prepare(outcome_id, definition, binding).await?;
        self.resume_active()
            .await?
            .ok_or_else(|| Error::Persistence("prepared Outcome is not active".into()))
    }

    /// Validate and durably create one Outcome, or accept an exact replay of the
    /// stable command. This phase performs no Worker or Grader Run; the command
    /// owner already retains the Outcome identity and needs no process-local
    /// receipt before [`Self::resume_active`].
    pub async fn prepare(
        &self,
        outcome_id: Id,
        definition: Definition,
        binding: Binding,
    ) -> Result<u64, Error> {
        definition
            .validate()
            .map_err(|error| Error::Domain(error.to_string()))?;
        for _ in 0..4 {
            match self.state.active().await.map_err(state_error)? {
                Some(active) => {
                    if active.state.outcome_id != outcome_id {
                        return Err(Error::Busy {
                            outcome_id: active.state.outcome_id,
                        });
                    }
                    if active.definition != definition {
                        return Err(Error::ActiveDefinitionConflict {
                            outcome_id: active.state.outcome_id,
                        });
                    }
                    return self
                        .state
                        .preparation_commit_cursor(&outcome_id)
                        .await
                        .map_err(state_error);
                }
                None => {
                    // A crash may occur after the exact Outcome reached a terminal
                    // Thread state but before the Session command was acknowledged.
                    // Reuse that aggregate by stable id; never create a second
                    // Outcome or infer completion from an adapter receipt.
                    match self.state.load(&outcome_id).await {
                        Ok(existing) if existing.definition == definition => {
                            if !existing.state.phase.is_terminal() {
                                return Err(Error::Persistence(
                                    "Outcome lost its active pointer before reaching a terminal phase"
                                        .into(),
                                ));
                            }
                            return self
                                .state
                                .preparation_commit_cursor(&outcome_id)
                                .await
                                .map_err(state_error);
                        }
                        Ok(_) => {
                            return Err(Error::ActiveDefinitionConflict { outcome_id });
                        }
                        Err(StateError::NotFound(_)) => {}
                        Err(error) => return Err(state_error(error)),
                    }
                    let state = State::new(
                        outcome_id.clone(),
                        self.reader.committed_messages(self.thread_id).len(),
                    );
                    match self.state.create(&definition, &binding, &state).await {
                        Ok(()) => {
                            return self
                                .state
                                .preparation_commit_cursor(&outcome_id)
                                .await
                                .map_err(state_error);
                        }
                        Err(StateError::AlreadyActive(_) | StateError::ConcurrentCommit { .. }) => {
                            continue;
                        }
                        Err(error) => return Err(state_error(error)),
                    }
                }
            }
        }
        Err(Error::Persistence(
            "Outcome preparation did not converge after concurrent Thread commits".into(),
        ))
    }

    /// Continue the one active Outcome from committed Thread state. `None`
    /// means the Thread has no active Outcome; callers must not reconstruct a
    /// definition or keep a parallel continuation registry.
    pub async fn resume_active(&self) -> Result<Option<Report>, Error> {
        let Some(aggregate) = self.state.active().await.map_err(state_error)? else {
            return Ok(None);
        };
        self.drive(aggregate).await.map(Some)
    }

    /// Read one exact terminal Outcome from committed Thread truth. Absent and
    /// active aggregates return `None`; completed and infrastructure-errored
    /// aggregates remain a closed typed projection. This query never drives a
    /// Run or mutates Thread state.
    pub async fn committed_projection(
        &self,
        outcome_id: &Id,
    ) -> Result<Option<CommittedOutcome>, Error> {
        let projection = match self.state.projection(outcome_id).await {
            Ok(projection) => projection,
            Err(StateError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(state_error(error)),
        };
        match &projection.aggregate.state.phase {
            Phase::Completed { .. } => report_from(&projection.aggregate, &projection.messages)
                .map(CommittedOutcome::Completed)
                .map(Some),
            Phase::Errored { failure } => Ok(Some(CommittedOutcome::Errored {
                source_run_id: failure_source_run_id(&projection.aggregate, failure),
                failure: failure.clone(),
            })),
            _ => Ok(None),
        }
    }

    async fn drive(&self, mut aggregate: Aggregate) -> Result<Report, Error> {
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
                    let grade = match self.grader.grade(&aggregate.binding.grader, &input).await {
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
        let snapshot = self
            .reader
            .transcript_snapshot(self.thread_id, TranscriptView::RawCommitted);
        let start = u64::try_from(message_start).map_err(|_| {
            Error::Serialization("message start does not fit transcript index".into())
        })?;
        let end = u64::try_from(aggregate.state.transcript_cursor).map_err(|_| {
            Error::Serialization("message end does not fit transcript index".into())
        })?;
        let ranges = vec![TranscriptRange::new(start, end)];
        let slice = snapshot
            .slice(&TranscriptSliceSpec {
                snapshot: snapshot.reference().clone(),
                ranges: ranges.clone(),
            })
            .map_err(|error| Error::Serialization(error.to_string()))?;
        Ok(GradingInput {
            outcome_id: aggregate.state.outcome_id.clone(),
            iteration,
            description: aggregate.definition.description.clone(),
            rubric: aggregate.definition.rubric.clone(),
            transcript_snapshot: snapshot.reference().clone(),
            transcript_ranges: ranges,
            transcript: slice.messages.to_vec(),
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
        report_from(aggregate, &transcript)
    }
}

fn report_from(aggregate: &Aggregate, transcript: &[Message]) -> Result<Report, Error> {
    let mut iterations = aggregate
        .evaluations
        .iter()
        .map(|evaluation| {
            let start = evaluation.message_start.min(transcript.len());
            let end = evaluation.message_end.min(transcript.len());
            if start > end {
                return Err(Error::Serialization(format!(
                    "Outcome evaluation {} has an inverted transcript range",
                    evaluation.iteration
                )));
            }
            Ok(IterationReport {
                messages: transcript[start..end].to_vec(),
                outcome_id: aggregate.state.outcome_id.clone(),
                description: aggregate.definition.description.clone(),
                iteration: evaluation.iteration,
                result: evaluation_result(evaluation, &aggregate.definition),
                explanation: evaluation.grade.explanation.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
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
        // Managed permits exactly one terminal result for an evaluation cycle.
        // Acknowledgment is not another cycle: its State keeps the last Grade's
        // iteration, so cancellation replaces that cycle's provisional budget
        // result. Worker/Judge cancellation has no Grade at State::iteration and
        // therefore materializes the missing current cycle instead.
        if let Some(last) = iterations.last_mut().filter(|last| {
            last.iteration == aggregate.state.iteration
                && last.result == EvaluationResult::MaxIterationsReached
        }) {
            last.result = EvaluationResult::Interrupted;
            last.explanation = "the outcome was interrupted".into();
        } else {
            iterations.push(IterationReport {
                messages: Vec::new(),
                outcome_id: aggregate.state.outcome_id.clone(),
                description: aggregate.definition.description.clone(),
                iteration: aggregate.state.iteration,
                result: EvaluationResult::Interrupted,
                explanation: "the outcome was interrupted".into(),
            });
        }
    }
    Ok(Report { iterations })
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

/// Correlate an Outcome failure with the ordinary Run that produced it. This is
/// derived from the extension-owned stable identities and committed aggregate;
/// adapters must not parse RunId strings or persist a parallel failure owner.
fn failure_source_run_id(aggregate: &Aggregate, failure: &ExecutionFailure) -> Option<RunId> {
    match failure {
        ExecutionFailure::GraderUnavailable(_) | ExecutionFailure::InvalidGraderOutput(_) => Some(
            grader_run_id(&aggregate.state.outcome_id, aggregate.state.iteration),
        ),
        ExecutionFailure::WorkerFailed(_) => {
            let acknowledgment_failed = aggregate.evaluations.last().is_some_and(|evaluation| {
                evaluation.iteration == aggregate.state.iteration
                    && evaluation.grade.decision == GradeDecision::NeedsRevision
                    && evaluation.iteration.saturating_add(1) >= aggregate.definition.max_iterations
            });
            Some(if acknowledgment_failed {
                acknowledgment_run_id(&aggregate.state.outcome_id)
            } else {
                worker_run_id(&aggregate.state.outcome_id, aggregate.state.iteration)
            })
        }
        ExecutionFailure::Persistence(_) => None,
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
        CommitCoordinator, CommitError, CommitOperation, CommitOperationCoordinator, CommitReceipt,
        CommitRecord, RecoveryError, ResumeTicket, RunRecoverySnapshot, RunRecoverySource,
        StateCommand, ThreadCommit,
    };
    use awaken_store_inmem::MemoryCommitCoordinator;

    use super::*;
    use crate::outcome::Rubric;

    struct World {
        store: MemoryCommitCoordinator,
        commits: Mutex<Vec<ThreadCommit>>,
        replies: Mutex<VecDeque<String>>,
        states: Mutex<VecDeque<RunState>>,
        executions: AtomicUsize,
    }

    impl World {
        fn new(replies: &[&str]) -> Self {
            Self {
                store: MemoryCommitCoordinator::new(),
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
            let receipt = self.store.commit(commit.clone()).await?;
            self.commits.lock().unwrap().push(commit);
            Ok(receipt)
        }
    }

    #[async_trait]
    impl CommitOperationCoordinator for World {
        async fn commit_operation(
            &self,
            operation: CommitOperation,
        ) -> Result<CommitReceipt, CommitError> {
            let commit = operation.commit.clone();
            let receipt = self.store.commit_operation(operation).await?;
            if !receipt.duplicate {
                self.commits.lock().unwrap().push(commit);
            }
            Ok(receipt)
        }
    }

    #[async_trait]
    impl RunRecoverySource for World {
        async fn recovery_snapshot(
            &self,
            thread_id: &ThreadId,
            claimed_run_id: &RunId,
        ) -> Result<RunRecoverySnapshot, RecoveryError> {
            self.store
                .recovery_snapshot(thread_id, claimed_run_id)
                .await
        }
    }

    impl CommittedThreadView for World {
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

        fn run(&self, run_id: &RunId) -> Option<RunRecord> {
            self.commits
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run_id() == run_id)
                .map(|commit| RunRecord {
                    id: run_id.clone(),
                    thread_id: commit.thread_id.clone(),
                    state: commit.run_state(),
                })
        }

        fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
            self.commits
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| &commit.thread_id == thread_id)
                .map(|commit| RunRecord {
                    id: commit.run_id().clone(),
                    thread_id: thread_id.clone(),
                    state: commit.run_state(),
                })
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
        binding_named("worker", "grader")
    }

    fn binding_named(worker: &str, grader: &str) -> Binding {
        Binding {
            worker: awaken_runtime_contract::ExecutableAgentSnapshot::builder(worker)
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model", "native",
                ))
                .build(),
            grader: awaken_runtime_contract::ExecutableAgentSnapshot::builder(grader)
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model", "native",
                ))
                .build(),
        }
    }

    struct SnapshotGrader {
        seen: Mutex<Vec<String>>,
    }

    struct RangeGrader;

    struct RecordingWindowGrader {
        seen: Mutex<Vec<GradingInput>>,
    }

    #[async_trait]
    impl Grader for RangeGrader {
        async fn grade(
            &self,
            _snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
            input: &GradingInput,
        ) -> Result<Grade, GraderError> {
            let deliverable = input
                .transcript
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Grade {
                decision: if deliverable.contains(&input.rubric.0) {
                    GradeDecision::Satisfied
                } else {
                    GradeDecision::NeedsRevision
                },
                explanation: "deterministic test grade".into(),
            })
        }
    }

    #[async_trait]
    impl Grader for RecordingWindowGrader {
        async fn grade(
            &self,
            _snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
            input: &GradingInput,
        ) -> Result<Grade, GraderError> {
            self.seen.lock().unwrap().push(input.clone());
            Ok(Grade {
                decision: GradeDecision::Satisfied,
                explanation: "recorded".into(),
            })
        }
    }

    #[async_trait]
    impl Grader for SnapshotGrader {
        async fn grade(
            &self,
            snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
            _input: &GradingInput,
        ) -> Result<Grade, GraderError> {
            self.seen
                .lock()
                .unwrap()
                .push(snapshot.root_agent_id.0.clone());
            Ok(Grade {
                decision: GradeDecision::Satisfied,
                explanation: "pinned snapshot used".into(),
            })
        }
    }

    async fn drive(world: &World, max_iterations: u32) -> Result<Report, Error> {
        let thread = ThreadId("worker-thread".into());
        let grader = RangeGrader;
        Controller::new(
            &thread,
            world,
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

    #[test]
    fn interrupted_report_owns_one_terminal_cycle() {
        // Cause/effect graph: C1 no Grade exists; C2 prior Grades end before the
        // interrupted State iteration (Worker/Judge is active); C3 the last Grade
        // has the same iteration (acknowledgment is active). Effects: E1 emit an
        // interrupted current cycle; E2 preserve prior needs_revision cycles; E3
        // replace the acknowledgment cycle's provisional max result; E4 expose
        // exactly one terminal result with unique, increasing iteration indexes.
        // Constraint: final acknowledgment is a distinct phase, not another
        // evaluation cycle, while Worker/Judge cancellation interrupts a cycle
        // with no Grade.
        //
        // | Rule | Grade relation to State | Public report                       |
        // | R1   | none                    | [0 interrupted]                     |
        // | R2   | last < current          | [0 needs_revision, 1 interrupted]   |
        // | R3   | last == current at cap  | [0 interrupted], no max/new cycle  |
        let interrupted =
            |max_iterations: u32, iteration: u32, evaluations: Vec<Evaluation>| Aggregate {
                definition: Definition::new("ship", "FINAL", max_iterations).unwrap(),
                binding: binding(),
                state: State {
                    outcome_id: Id("interrupted-report".into()),
                    phase: Phase::Completed {
                        result: EvaluationResult::Interrupted,
                    },
                    iteration,
                    transcript_cursor: 0,
                    version: 1,
                },
                evaluations,
            };
        let evaluation = |iteration| Evaluation {
            iteration,
            worker_run_id: RunId(format!("worker-{iteration}")),
            grader_run_id: RunId(format!("grader-{iteration}")),
            message_start: 0,
            message_end: 0,
            grade: Grade {
                decision: GradeDecision::NeedsRevision,
                explanation: "revise".into(),
            },
        };

        let before_grade = report_from(&interrupted(1, 0, Vec::new()), &[]).unwrap();
        assert_eq!(
            before_grade
                .iterations
                .iter()
                .map(|item| (item.iteration, item.result))
                .collect::<Vec<_>>(),
            vec![(0, EvaluationResult::Interrupted)],
            "R1/E1+E4"
        );

        let during_next_cycle = report_from(&interrupted(2, 1, vec![evaluation(0)]), &[]).unwrap();
        assert_eq!(
            during_next_cycle
                .iterations
                .iter()
                .map(|item| (item.iteration, item.result))
                .collect::<Vec<_>>(),
            vec![
                (0, EvaluationResult::NeedsRevision),
                (1, EvaluationResult::Interrupted),
            ],
            "R2/E1+E2+E4"
        );

        let during_ack = report_from(&interrupted(1, 0, vec![evaluation(0)]), &[]).unwrap();
        assert_eq!(
            during_ack
                .iterations
                .iter()
                .map(|item| (item.iteration, item.result))
                .collect::<Vec<_>>(),
            vec![(0, EvaluationResult::Interrupted)],
            "R3/E3+E4"
        );
    }

    #[test]
    fn committed_failure_source_run_follows_the_phase_owner() {
        // Cause/effect graph: C1 Worker infrastructure failure occurs before a
        // Grade; C2 the final acknowledgment fails after a same-iteration
        // needs_revision Grade reaches the cap; C3 the Grader execution/schema
        // fails; C4 persistence fails without an ordinary execution Run.
        // Effects: E1 use the stable Worker Run; E2 use the one acknowledgment
        // Run; E3 use the stable Grader Run; E4 expose no source Run. Constraint:
        // this derivation consumes only the committed aggregate identities—an
        // adapter never parses a RunId or stores another ownership fact.
        //
        // | Rule | Failure/stage                     | source_run_id |
        // | R1   | C1 Worker, no same-cycle cap Grade | worker/0      |
        // | R2   | C2 Worker, same-cycle cap Grade    | ack           |
        // | R3   | C3 Grader execution or schema      | grader/0      |
        // | R4   | C4 Persistence                     | None          |
        let outcome_id = Id("failure-source".into());
        let aggregate = |max_iterations, evaluations| Aggregate {
            definition: Definition::new("ship", "FINAL", max_iterations).unwrap(),
            binding: binding(),
            state: State {
                outcome_id: outcome_id.clone(),
                phase: Phase::Errored {
                    failure: ExecutionFailure::Persistence("fixture".into()),
                },
                iteration: 0,
                transcript_cursor: 0,
                version: 1,
            },
            evaluations,
        };
        let capped_grade = Evaluation {
            iteration: 0,
            worker_run_id: worker_run_id(&outcome_id, 0),
            grader_run_id: grader_run_id(&outcome_id, 0),
            message_start: 0,
            message_end: 0,
            grade: Grade {
                decision: GradeDecision::NeedsRevision,
                explanation: "revise".into(),
            },
        };

        assert_eq!(
            failure_source_run_id(
                &aggregate(1, Vec::new()),
                &ExecutionFailure::WorkerFailed("worker".into()),
            ),
            Some(worker_run_id(&outcome_id, 0)),
            "R1/E1"
        );
        assert_eq!(
            failure_source_run_id(
                &aggregate(1, vec![capped_grade]),
                &ExecutionFailure::WorkerFailed("ack".into()),
            ),
            Some(acknowledgment_run_id(&outcome_id)),
            "R2/E2"
        );
        for failure in [
            ExecutionFailure::GraderUnavailable("provider".into()),
            ExecutionFailure::InvalidGraderOutput("schema".into()),
        ] {
            assert_eq!(
                failure_source_run_id(&aggregate(2, Vec::new()), &failure),
                Some(grader_run_id(&outcome_id, 0)),
                "R3/E3"
            );
        }
        assert_eq!(
            failure_source_run_id(
                &aggregate(2, Vec::new()),
                &ExecutionFailure::Persistence("store".into()),
            ),
            None,
            "R4/E4"
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
    async fn embedded_controller_resumes_without_any_server_application() {
        let world = World::new(&["FINAL"]).with_states(vec![
            RunState::Awaiting,
            RunState::Ended(EndCause::NaturalEnd),
        ]);
        let thread = ThreadId("embedded-worker".into());
        let grader = RangeGrader;
        let controller = Controller::new(
            &thread,
            &world,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        );

        // Standalone cause/effect decision table: S1 no active aggregate ->
        // `resume_active=None`; S2 valid definition + executor Awaiting -> typed
        // external boundary with durable active state; S3 embedding resolves that
        // boundary through the same Run port -> resume from Thread truth and
        // complete; S4 resume after completion -> None. No Server/Managed type,
        // store, route, or process registry participates in any rule.
        // Constraints/invariants: the embedded controller depends only on its
        // declared ports and committed Thread truth; it cannot require Server state.
        assert!(controller.resume_active().await.unwrap().is_none());
        assert!(matches!(
            controller
                .define_or_resume(
                    Id("embedded-outcome".into()),
                    Definition::new("ship", "FINAL", 2).unwrap(),
                    binding(),
                )
                .await,
            Err(Error::WorkerAwaiting { .. })
        ));
        let report = controller.resume_active().await.unwrap().unwrap();
        assert_eq!(report.iterations[0].result, EvaluationResult::Satisfied);
        assert!(controller.resume_active().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn committed_projection_reads_only_terminal_snapshot_truth() {
        // Cause/effect graph: C1 the requested id is absent; C2 its aggregate
        // exists but is active; C3 it completed; C4 it committed Errored after
        // a Worker failure. Effects: E1 C1/C2 return None; E2 C3 returns the
        // same report as the drive; E3 C4 returns the typed failure and exact
        // source Run; E4 every query leaves commits/executions unchanged.
        // Decision table: Q1=C1=>E1+E4, Q2=C2=>E1+E4,
        // Q3=C3=>E2+E4, Q4=C4=>E3+E4.
        // Constraints/invariants: projection is read-only and terminal snapshot
        // truth is the sole source for completed or errored outcomes.
        let world = World::new(&["FINAL"]);
        let thread = ThreadId("committed-report-worker".into());
        let grader = RangeGrader;
        let definition = Definition::new("ship", "FINAL", 2).unwrap();
        let outcome_id = Id("committed-report".into());
        let controller = Controller::new(
            &thread,
            &world,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        );

        let before = world.executions.load(Ordering::SeqCst);
        assert!(
            controller
                .committed_projection(&Id("missing".into()))
                .await
                .unwrap()
                .is_none(),
            "Q1/E1"
        );
        assert_eq!(world.executions.load(Ordering::SeqCst), before, "Q1/E4");

        controller
            .prepare(outcome_id.clone(), definition, binding())
            .await
            .unwrap();
        let prepared_commits = world.commits.lock().unwrap().len();
        assert!(
            controller
                .committed_projection(&outcome_id)
                .await
                .unwrap()
                .is_none(),
            "Q2/E1"
        );
        assert_eq!(
            world.commits.lock().unwrap().len(),
            prepared_commits,
            "Q2/E4"
        );

        let driven = controller
            .resume_active()
            .await
            .unwrap()
            .expect("Q3 terminal report");
        let terminal_commits = world.commits.lock().unwrap().len();
        let terminal_executions = world.executions.load(Ordering::SeqCst);
        let restarted = Controller::new(
            &thread,
            &world,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        );
        assert_eq!(
            restarted.committed_projection(&outcome_id).await.unwrap(),
            Some(CommittedOutcome::Completed(driven)),
            "Q3/E2"
        );
        assert_eq!(
            world.commits.lock().unwrap().len(),
            terminal_commits,
            "Q3/E4"
        );
        assert_eq!(
            world.executions.load(Ordering::SeqCst),
            terminal_executions,
            "Q3/E4"
        );

        let failed_world = World::new(&[]).with_states(vec![RunState::Ended(EndCause::Stopped(
            "provider rejected the request".into(),
        ))]);
        let failed_thread = ThreadId("committed-failure-worker".into());
        let failed_id = Id("committed-failure".into());
        let failed = Controller::new(
            &failed_thread,
            &failed_world,
            &failed_world,
            &failed_world,
            &failed_world,
            RuntimeRunContext::new(),
            &grader,
        );
        assert!(matches!(
            failed
                .define_or_resume(
                    failed_id.clone(),
                    Definition::new("ship", "FINAL", 1).unwrap(),
                    binding(),
                )
                .await,
            Err(Error::Execution(ExecutionFailure::WorkerFailed(_)))
        ));
        let failed_commits = failed_world.commits.lock().unwrap().len();
        let failed_executions = failed_world.executions.load(Ordering::SeqCst);
        let Some(CommittedOutcome::Errored {
            failure,
            source_run_id,
        }) = failed.committed_projection(&failed_id).await.unwrap()
        else {
            panic!("Q4/E3 expected committed infrastructure failure");
        };
        assert_eq!(failure.code(), "outcome_worker_failed", "Q4/E3");
        assert!(
            failure.message().contains("provider rejected the request"),
            "Q4/E3"
        );
        assert_eq!(source_run_id, Some(worker_run_id(&failed_id, 0)), "Q4/E3");
        assert_eq!(
            failed_world.commits.lock().unwrap().len(),
            failed_commits,
            "Q4/E4"
        );
        assert_eq!(
            failed_world.executions.load(Ordering::SeqCst),
            failed_executions,
            "Q4/E4"
        );
    }

    /// Prepare cause/effect graph: C1 no Outcome has the stable id; C2 the exact
    /// active id/definition is replayed; C3 another id is already active; C4
    /// the same active id carries another definition; C5 the exact stable
    /// aggregate is terminal; C6 a terminal aggregate's definition differs.
    /// The newly resolved binding is deliberately not a
    /// replay axis: Thread state owns the frozen binding across configuration
    /// changes. Effects: E1 persist one Defined
    /// aggregate; E2 exact replay; E3 return retryable Busy without another
    /// commit; E4 reject identity reuse with another definition; E5 prepare
    /// executes no Worker/Grader Run; E6 terminal crash replay accepts the same
    /// command without recreating an active aggregate. Decision table:
    /// O1=C1=>E1+E5, O2=C2=>E2+E5, O3=C3=>E3+E5,
    /// O4=C4=>E4+E5, O5=C5=>E6+E5, O6=C6=>E4+E5.
    /// Constraints/invariants: a stable id freezes its definition/binding once;
    /// prepare never executes Worker or Grader work and replay mints no commit.
    #[tokio::test]
    async fn prepare_is_durable_idempotent_and_execution_free() {
        let world = World::new(&["FINAL"]);
        let thread = ThreadId("prepare-worker".into());
        let grader = RangeGrader;
        let controller = Controller::new(
            &thread,
            &world,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        );
        let definition = Definition::new("ship", "FINAL", 2).unwrap();
        let pinned = binding_named("worker-v1", "grader-v1");
        let stable = Id("prepared-outcome".into());
        controller
            .prepare(stable.clone(), definition.clone(), pinned.clone())
            .await
            .expect("O1 prepare");
        let active = ThreadOutcomeState::new(&thread, &world, &world)
            .active()
            .await
            .unwrap()
            .expect("O1 active");
        assert!(matches!(active.state.phase, Phase::Defined), "O1/E1");
        assert_eq!(world.executions.load(Ordering::SeqCst), 0, "O1/E4");

        controller
            .prepare(
                stable.clone(),
                definition.clone(),
                binding_named("worker-v2", "grader-v2"),
            )
            .await
            .expect("O2 replay");
        assert_eq!(world.executions.load(Ordering::SeqCst), 0, "O2/E4");

        let commits_before = world.commits.lock().unwrap().len();
        let conflict = controller
            .prepare(
                Id("other-outcome".into()),
                definition.clone(),
                pinned.clone(),
            )
            .await
            .expect_err("O3 active Outcome is retryable Busy");
        assert!(matches!(conflict, Error::Busy { .. }), "O3/E3");
        assert_eq!(world.commits.lock().unwrap().len(), commits_before, "O3/E3");
        assert_eq!(world.executions.load(Ordering::SeqCst), 0, "O3/E5");

        let active_definition_conflict = controller
            .prepare(
                stable.clone(),
                Definition::new("different", "FINAL", 2).unwrap(),
                pinned.clone(),
            )
            .await
            .expect_err("O4 active definition conflict");
        assert!(
            matches!(
                active_definition_conflict,
                Error::ActiveDefinitionConflict { .. }
            ),
            "O4/E4"
        );
        assert_eq!(world.commits.lock().unwrap().len(), commits_before, "O4/E4");

        controller
            .resume_active()
            .await
            .expect("O5 drive")
            .expect("O5 terminal report");
        let terminal_commits = world.commits.lock().unwrap().len();
        let terminal_executions = world.executions.load(Ordering::SeqCst);
        controller
            .prepare(
                stable.clone(),
                definition.clone(),
                binding_named("worker-v3", "grader-v3"),
            )
            .await
            .expect("O5 exact terminal replay");
        assert_eq!(
            world.commits.lock().unwrap().len(),
            terminal_commits,
            "O5/E5"
        );
        assert_eq!(
            world.executions.load(Ordering::SeqCst),
            terminal_executions,
            "O5/E5"
        );
        let terminal_conflict = controller
            .prepare(
                stable,
                Definition::new("different", "FINAL", 2).unwrap(),
                pinned,
            )
            .await
            .expect_err("O6 terminal payload conflict");
        assert!(
            matches!(terminal_conflict, Error::ActiveDefinitionConflict { .. }),
            "O6/E4"
        );
        assert_eq!(
            world.commits.lock().unwrap().len(),
            terminal_commits,
            "O6/E4"
        );
    }

    #[tokio::test]
    async fn prepare_is_fenced_across_active_active_replicas() {
        // Cause/effect graph: C1 two controllers prepare one stable command
        // from the same empty Thread prefix; C2 they instead prepare different
        // outcome ids. Effects: E1 C1 yields two idempotent successes and one
        // durable create; E2 C2 yields one success plus retryable Busy and one
        // durable create; E3 no Worker/Grader Run executes during either rule.
        // Decision table: F1=C1=>E1+E3; F2=C2=>E2+E3.
        // Constraints/invariants: the Thread commit fence admits one create;
        // identical competitors converge while different ids cannot both win.
        let same_world = World::new(&[]);
        let same_thread = ThreadId("active-active-same".into());
        let grader = RangeGrader;
        let left = Controller::new(
            &same_thread,
            &same_world,
            &same_world,
            &same_world,
            &same_world,
            RuntimeRunContext::new(),
            &grader,
        );
        let right = Controller::new(
            &same_thread,
            &same_world,
            &same_world,
            &same_world,
            &same_world,
            RuntimeRunContext::new(),
            &grader,
        );
        let definition = Definition::new("ship", "FINAL", 2).unwrap();
        let (left_result, right_result) = tokio::join!(
            left.prepare(Id("same".into()), definition.clone(), binding()),
            right.prepare(Id("same".into()), definition.clone(), binding()),
        );
        assert!(left_result.is_ok() && right_result.is_ok(), "F1/E1");
        assert_eq!(same_world.commits.lock().unwrap().len(), 1, "F1/E1");
        assert_eq!(same_world.executions.load(Ordering::SeqCst), 0, "F1/E3");

        let competing_world = World::new(&[]);
        let competing_thread = ThreadId("active-active-competing".into());
        let left = Controller::new(
            &competing_thread,
            &competing_world,
            &competing_world,
            &competing_world,
            &competing_world,
            RuntimeRunContext::new(),
            &grader,
        );
        let right = Controller::new(
            &competing_thread,
            &competing_world,
            &competing_world,
            &competing_world,
            &competing_world,
            RuntimeRunContext::new(),
            &grader,
        );
        let (left_result, right_result) = tokio::join!(
            left.prepare(Id("left".into()), definition.clone(), binding()),
            right.prepare(Id("right".into()), definition, binding()),
        );
        assert_eq!(
            usize::from(left_result.is_ok()) + usize::from(right_result.is_ok()),
            1,
            "F2/E2"
        );
        let loser = if left_result.is_ok() {
            right_result
        } else {
            left_result
        };
        assert!(matches!(loser, Err(Error::Busy { .. })), "F2/E2");
        assert_eq!(competing_world.commits.lock().unwrap().len(), 1, "F2/E2");
        assert_eq!(
            competing_world.executions.load(Ordering::SeqCst),
            0,
            "F2/E3"
        );
    }

    #[tokio::test]
    async fn restart_after_worker_commit_reuses_the_stable_run() {
        // Test design — Causes: a stable Worker Run is already committed before
        // a fresh controller resumes the Outcome. Effects: grading observes that
        // Run and completes without another execution. Constraints/invariants:
        // committed Run identity is the sole replay authority. Decision rule R1:
        // committed worker+restart=>execution count remains one.
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

        let grader = RangeGrader;
        let report = Controller::new(
            &thread,
            &world,
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

    #[tokio::test]
    async fn grader_materializes_only_the_current_worker_range_from_a_frozen_snapshot() {
        // Test design — Causes: unrelated old Thread history precedes the current
        // Worker's committed output. Effects: the Grader receives the frozen
        // snapshot coordinate and only the current Run range. Constraints/
        // invariants: no earlier transcript row leaks into grading. Decision rule
        // G1: old prefix+current range=>exact range 2..3 and text FINAL only.
        let world = World::new(&["FINAL"]);
        let thread = ThreadId("window-worker".into());
        world
            .commit(ThreadCommit::assemble(
                thread.clone(),
                awaken_runtime_contract::RunDisposition::ended(
                    RunId("old-run".into()),
                    EndCause::NaturalEnd,
                ),
                false,
                vec![Message::text(
                    MessageId("old".into()),
                    Role::User,
                    "unrelated old history",
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        let grader = RecordingWindowGrader {
            seen: Mutex::new(Vec::new()),
        };
        Controller::new(
            &thread,
            &world,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        )
        .define_or_resume(
            Id("window-outcome".into()),
            Definition::new("ship", "FINAL", 2).unwrap(),
            binding(),
        )
        .await
        .unwrap();

        let seen = grader.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].transcript_snapshot.end_seq, 3);
        assert_eq!(seen[0].transcript_ranges, vec![TranscriptRange::new(2, 3)]);
        assert_eq!(
            seen[0]
                .transcript
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>(),
            ["FINAL"]
        );
    }

    #[tokio::test]
    async fn recovery_grades_with_the_persisted_snapshot_not_current_configuration() {
        // Test design — Causes: recovery supplies current v2 bindings while the
        // durable Outcome froze v1. Effects: execution succeeds and Grader v1 is
        // used. Constraints/invariants: persisted snapshot authority outranks
        // mutable configuration during recovery. Decision rule S1:
        // persisted-v1+current-v2=>observe grader-v1 exactly once.
        let world = World::new(&["deliverable"]);
        let thread = ThreadId("worker-thread".into());
        let definition = Definition::new("ship", "rubric", 2).unwrap();
        let persisted = binding_named("worker-v1", "grader-v1");
        ThreadOutcomeState::new(&thread, &world, &world)
            .create(
                &definition,
                &persisted,
                &State::new(Id("outcome-1".into()), 0),
            )
            .await
            .unwrap();
        let grader = SnapshotGrader {
            seen: Mutex::new(Vec::new()),
        };

        let report = Controller::new(
            &thread,
            &world,
            &world,
            &world,
            &world,
            RuntimeRunContext::new(),
            &grader,
        )
        .define_or_resume(
            // Recovery reuses the stable Event-owned Outcome id while the
            // persisted aggregate, not current configuration, owns its binding.
            Id("outcome-1".into()),
            definition,
            binding_named("worker-v2", "grader-v2"),
        )
        .await
        .unwrap();

        assert_eq!(report.iterations[0].result, EvaluationResult::Satisfied);
        assert_eq!(&*grader.seen.lock().unwrap(), &["grader-v1"]);
    }
}
