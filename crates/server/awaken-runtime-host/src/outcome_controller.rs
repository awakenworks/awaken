//! Runtime-Host application controller for the bounded Outcome lifecycle.

use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_goal::grader::{AgentGrader, DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};
use awaken_ext_goal::outcome::{
    Definition, Evaluation, EvaluationResult, ExecutionFailure, Grade, Grader, GraderError,
    GradingInput, Id, KeywordGrader, Phase, State, WorkerRunKind,
};
use awaken_ext_goal::state::{
    Aggregate, Binding, ThreadOutcomeState, acknowledgment_run_id, grader_run_id, worker_run_id,
};

use crate::host::{
    BASE_SEQ, HostError, HostOutcomeIteration, HostOutcomeReport, SessionCtx, SharedHost, now_ms,
};
use crate::run_exec::{BoundRunExecutor, SnapshotRunRequest};

struct WorkerExecution {
    state: RunState,
    message_start: usize,
}

impl SharedHost {
    /// Define or recover an Outcome, then drive its Worker/Judge Runs until a
    /// terminal business result, interruption, or typed infrastructure failure.
    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<HostOutcomeReport, HostError> {
        let definition = Definition::new(description, rubric, max_iterations)
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        let ctx = self.ctx_for(thread, None).await?;
        let _outcome = ctx.outcome.lock().await;
        let _execution = ctx.execution.lock().await;
        let persistence =
            ThreadOutcomeState::new(&ctx.thread_id, ctx.commit.as_ref(), ctx.commit.as_ref());

        let mut aggregate = match persistence.active().map_err(outcome_state_error)? {
            Some(active) => {
                if active.definition != definition {
                    return Err(HostError::bad_request(format!(
                        "Worker Thread already has active Outcome {} with a different definition",
                        active.state.outcome_id.0
                    )));
                }
                active
            }
            None => {
                let id = Id(format!(
                    "outc_{}_{}",
                    now_ms(),
                    BASE_SEQ.fetch_add(1, Ordering::SeqCst)
                ));
                let binding = Binding {
                    worker: ctx.config.clone(),
                    grader: self.judge_snapshot.clone().unwrap_or_else(|| {
                        default_judge_agent(
                            &self.model_ref,
                            "outcome-grader",
                            DEFAULT_JUDGE_INSTRUCTIONS,
                        )
                    }),
                };
                let state = State::new(id, ctx.commit.committed_messages(&ctx.thread_id).len());
                persistence
                    .create(&definition, &binding, &state)
                    .await
                    .map_err(outcome_state_error)?;
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
                    persistence
                        .commit_if_current(expected, &aggregate.state, None)
                        .await
                        .map_err(outcome_state_error)?;
                }
                Phase::RunningWorker {
                    iteration,
                    run_id,
                    kind,
                } => {
                    let expected = aggregate.state.version;
                    let worker = self
                        .drive_outcome_worker(&ctx, &aggregate, iteration, kind, run_id.clone())
                        .await?;
                    match worker.state {
                        RunState::Ended(EndCause::NaturalEnd) => {
                            let cursor = ctx.commit.committed_messages(&ctx.thread_id).len();
                            aggregate
                                .state
                                .worker_completed(
                                    &run_id,
                                    grader_run_id(&aggregate.state.outcome_id, iteration),
                                    worker.message_start,
                                    cursor,
                                )
                                .map_err(domain_error)?;
                            persistence
                                .commit_if_current(expected, &aggregate.state, None)
                                .await
                                .map_err(outcome_state_error)?;
                        }
                        RunState::Ended(EndCause::Cancelled) => {
                            aggregate.state.interrupt();
                            persistence
                                .commit_if_current(expected, &aggregate.state, None)
                                .await
                                .map_err(outcome_state_error)?;
                        }
                        RunState::Awaiting => {
                            return Err(HostError::bad_request(
                                "Outcome Worker awaits external input; resume the pending Run before retrying define_outcome",
                            ));
                        }
                        state => {
                            let failure = ExecutionFailure::WorkerFailed(format!(
                                "Worker Run ended in {state:?}"
                            ));
                            aggregate.state.fail(failure).map_err(domain_error)?;
                            persistence
                                .commit_if_current(expected, &aggregate.state, None)
                                .await
                                .map_err(outcome_state_error)?;
                        }
                    }
                }
                Phase::Evaluating {
                    iteration,
                    grader_run_id,
                    message_start,
                } => {
                    let expected = aggregate.state.version;
                    let transcript = ctx.commit.committed_messages(&ctx.thread_id);
                    let input = GradingInput {
                        outcome_id: aggregate.state.outcome_id.clone(),
                        iteration,
                        description: aggregate.definition.description.clone(),
                        rubric: aggregate.definition.rubric.clone(),
                        transcript,
                        message_start,
                        message_end: aggregate.state.transcript_cursor,
                        worker_state: serde_json::to_value(&aggregate.state)
                            .map_err(|error| HostError::internal(error.to_string()))?,
                        evidence: Vec::new(),
                    };
                    let grade = match self.grade_outcome(&ctx, &aggregate, &input).await {
                        Ok(grade) => grade,
                        Err(GraderError::Interrupted) => {
                            aggregate.state.interrupt();
                            persistence
                                .commit_if_current(expected, &aggregate.state, None)
                                .await
                                .map_err(outcome_state_error)?;
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
                            persistence
                                .commit_if_current(expected, &aggregate.state, None)
                                .await
                                .map_err(outcome_state_error)?;
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
                    let next_run_id = next_run_id(&aggregate, iteration, &grade);
                    aggregate
                        .state
                        .apply_grade(&aggregate.definition, &grader_run_id, &grade, next_run_id)
                        .map_err(domain_error)?;
                    persistence
                        .commit_if_current(expected, &aggregate.state, Some(&evaluation))
                        .await
                        .map_err(outcome_state_error)?;
                    aggregate.evaluations.push(evaluation);
                }
                Phase::Acknowledging { run_id } => {
                    let expected = aggregate.state.version;
                    let state = self
                        .drive_outcome_acknowledgment(&ctx, &aggregate, run_id.clone())
                        .await?;
                    match state {
                        RunState::Ended(EndCause::NaturalEnd) => aggregate
                            .state
                            .acknowledgment_completed(&run_id)
                            .map_err(domain_error)?,
                        RunState::Ended(EndCause::Cancelled) => {
                            aggregate.state.interrupt();
                        }
                        other => {
                            aggregate
                                .state
                                .fail(ExecutionFailure::WorkerFailed(format!(
                                    "acknowledgment Run ended in {other:?}"
                                )))
                                .map_err(domain_error)?;
                        }
                    }
                    persistence
                        .commit_if_current(expected, &aggregate.state, None)
                        .await
                        .map_err(outcome_state_error)?;
                }
                Phase::Completed { .. } => return report(&ctx, &aggregate),
                Phase::Errored { ref failure } => {
                    return Err(HostError::internal(format!(
                        "Outcome {} failed: {failure:?}",
                        aggregate.state.outcome_id.0
                    )));
                }
            }
        }
    }

    async fn grade_outcome(
        &self,
        ctx: &SessionCtx,
        aggregate: &Aggregate,
        input: &GradingInput,
    ) -> Result<Grade, GraderError> {
        match &self.judge_snapshot {
            Some(_) => {
                let grader_thread =
                    awaken_ext_goal::state::grader_thread_id(&input.outcome_id, input.iteration);
                let grader_context = self
                    .ctx_for(&grader_thread.0, None)
                    .await
                    .map_err(|error| GraderError::Execution(error.to_string()))?;
                let executor = BoundRunExecutor::new(self, grader_context.clone())
                    .with_cancellation_mirror(ctx.cancel.clone());
                AgentGrader::new(
                    &executor,
                    grader_context.commit.as_ref(),
                    &aggregate.binding.grader,
                    awaken_runtime_contract::RuntimeRunContext::new(),
                )
                .grade(input)
                .await
            }
            None => KeywordGrader.grade(input).await,
        }
    }

    async fn drive_outcome_worker(
        &self,
        ctx: &SessionCtx,
        aggregate: &Aggregate,
        iteration: u32,
        kind: WorkerRunKind,
        run_id: awaken_agent_contract::agent::run::Id,
    ) -> Result<WorkerExecution, HostError> {
        // Reuse only a settled Run. `Running` may be the durable start fact left
        // by a killed process; redrive that same stable id to finish it.
        if let Some(state) = ctx.commit.run_state(&run_id)
            && state != RunState::Running
        {
            let transcript = ctx.commit.committed_messages(&ctx.thread_id);
            let input_id = worker_input_id(&aggregate.state.outcome_id, iteration);
            let message_start = transcript
                .iter()
                .enumerate()
                .skip(aggregate.state.transcript_cursor)
                .find(|(_, message)| message.id.0 == input_id)
                .map(|(index, _)| index + 1)
                .ok_or_else(|| {
                    HostError::internal("recovered Worker Run is missing its deterministic input")
                })?;
            return Ok(WorkerExecution {
                state,
                message_start,
            });
        }
        let prompt = match kind {
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
        };
        let result = self
            .execute_snapshot(
                &self.ctx_for(&ctx.thread_id.0, None).await?,
                SnapshotRunRequest {
                    run_id: Some(run_id),
                    thread_id: ctx.thread_id.clone(),
                    snapshot: aggregate.binding.worker.clone(),
                    input: vec![Message::text(
                        MessageId(worker_input_id(&aggregate.state.outcome_id, iteration)),
                        Role::User,
                        prompt,
                    )],
                    tool_capability_narrowing: Default::default(),
                    model_ref_override: None,
                    supersede: false,
                    sink: None,
                    cancellation_mirror: None,
                },
            )
            .await?;
        Ok(WorkerExecution {
            state: result.state,
            message_start: result.before.saturating_add(1),
        })
    }

    async fn drive_outcome_acknowledgment(
        &self,
        ctx: &SessionCtx,
        aggregate: &Aggregate,
        run_id: awaken_agent_contract::agent::run::Id,
    ) -> Result<RunState, HostError> {
        if let Some(state) = ctx.commit.run_state(&run_id)
            && state != RunState::Running
        {
            return Ok(state);
        }
        self.execute_snapshot(
            &self.ctx_for(&ctx.thread_id.0, None).await?,
            SnapshotRunRequest {
                run_id: Some(run_id),
                thread_id: ctx.thread_id.clone(),
                snapshot: aggregate.binding.worker.clone(),
                input: vec![Message::text(
                    MessageId(format!(
                        "outcome/{}/ack/input",
                        aggregate.state.outcome_id.0
                    )),
                    Role::User,
                    "The Outcome iteration limit was reached. Briefly acknowledge the remaining Grader feedback without starting another graded revision.",
                )],
                tool_capability_narrowing: Default::default(),
                model_ref_override: None,
                supersede: false,
                sink: None,
                cancellation_mirror: None,
            },
        )
        .await
        .map(|result| result.state)
    }
}

fn next_run_id(
    aggregate: &Aggregate,
    iteration: u32,
    grade: &Grade,
) -> awaken_agent_contract::agent::run::Id {
    if grade.decision == awaken_ext_goal::outcome::GradeDecision::NeedsRevision
        && iteration.saturating_add(1) >= aggregate.definition.max_iterations
    {
        acknowledgment_run_id(&aggregate.state.outcome_id)
    } else {
        worker_run_id(&aggregate.state.outcome_id, iteration.saturating_add(1))
    }
}

fn worker_input_id(id: &Id, iteration: u32) -> String {
    format!("outcome/{}/worker/{iteration}/input", id.0)
}

fn report(ctx: &SessionCtx, aggregate: &Aggregate) -> Result<HostOutcomeReport, HostError> {
    let transcript = ctx.commit.committed_messages(&ctx.thread_id);
    let mut iterations = aggregate
        .evaluations
        .iter()
        .map(|evaluation| HostOutcomeIteration {
            messages: transcript[evaluation.message_start.min(transcript.len())
                ..evaluation.message_end.min(transcript.len())]
                .to_vec(),
            outcome_id: aggregate.state.outcome_id.0.clone(),
            iteration: evaluation.iteration,
            result: match evaluation.grade.decision {
                awaken_ext_goal::outcome::GradeDecision::Satisfied => "satisfied",
                awaken_ext_goal::outcome::GradeDecision::NeedsRevision
                    if evaluation.iteration.saturating_add(1)
                        >= aggregate.definition.max_iterations =>
                {
                    "max_iterations_reached"
                }
                awaken_ext_goal::outcome::GradeDecision::NeedsRevision => "needs_revision",
                awaken_ext_goal::outcome::GradeDecision::Failed => "failed",
            }
            .into(),
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
    if let Phase::Completed {
        result: EvaluationResult::Interrupted,
    } = aggregate.state.phase
    {
        iterations.push(HostOutcomeIteration {
            messages: Vec::new(),
            outcome_id: aggregate.state.outcome_id.0.clone(),
            iteration: aggregate.state.iteration,
            result: "interrupted".into(),
            explanation: "the outcome was interrupted".into(),
        });
    }
    Ok(HostOutcomeReport { iterations })
}

fn domain_error(error: awaken_ext_goal::outcome::Error) -> HostError {
    HostError::internal(error.to_string())
}

fn outcome_state_error(error: awaken_ext_goal::state::Error) -> HostError {
    HostError::internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct SequenceModel {
        replies: Mutex<VecDeque<String>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl SequenceModel {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|reply| (*reply).into()).collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmExecutor for SequenceModel {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "fallback".into());
            Ok(ChatResponse {
                output: AssistantOutput::text(reply),
                usage: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn satisfied_outcome_runs_one_zero_based_evaluation() {
        let model = Arc::new(SequenceModel::new(&["contains FINAL"]));
        let host = SharedHost::new(model.clone(), "stub");
        let report = host
            .define_outcome("satisfied", "finish", "FINAL", 3)
            .await
            .unwrap();
        assert_eq!(report.iterations.len(), 1);
        assert_eq!(report.iterations[0].iteration, 0);
        assert_eq!(report.iterations[0].result, "satisfied");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn revision_feedback_drives_a_second_graded_worker_run() {
        let model = Arc::new(SequenceModel::new(&["draft", "now FINAL"]));
        let host = SharedHost::new(model.clone(), "stub");
        let report = host
            .define_outcome("revision", "finish", "FINAL", 3)
            .await
            .unwrap();
        assert_eq!(
            report
                .iterations
                .iter()
                .map(|round| (round.iteration, round.result.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "needs_revision"), (1, "satisfied")]
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausted_budget_runs_one_ungraded_acknowledgment() {
        let model = Arc::new(SequenceModel::new(&["draft", "acknowledged"]));
        let host = SharedHost::new(model.clone(), "stub");
        let report = host
            .define_outcome("max", "finish", "FINAL", 1)
            .await
            .unwrap();
        assert_eq!(report.iterations.len(), 1);
        assert_eq!(report.iterations[0].iteration, 0);
        assert_eq!(report.iterations[0].result, "max_iterations_reached");
        assert!(
            report.iterations[0]
                .messages
                .iter()
                .any(|message| message.text_content() == "acknowledged")
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn restart_after_worker_commit_reuses_the_stable_run_without_inference() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(SequenceModel::new(&["FINAL"]));
        let definition = Definition::new("recover", "FINAL", 2).unwrap();
        let outcome_id = Id("outc_recovery".into());
        {
            let host = SharedHost::new(model.clone(), "stub").with_store_dir(directory.path());
            let ctx = host.ctx_for("recover-thread", None).await.unwrap();
            let binding = Binding {
                worker: ctx.config.clone(),
                grader: default_judge_agent("stub", "outcome-grader", DEFAULT_JUDGE_INSTRUCTIONS),
            };
            let mut state = State::new(outcome_id.clone(), 0);
            let persistence =
                ThreadOutcomeState::new(&ctx.thread_id, ctx.commit.as_ref(), ctx.commit.as_ref());
            persistence
                .create(&definition, &binding, &state)
                .await
                .unwrap();
            state.start(worker_run_id(&outcome_id, 0)).unwrap();
            persistence
                .commit_if_current(0, &state, None)
                .await
                .unwrap();
            host.drive_outcome_worker(
                &ctx,
                &persistence.active().unwrap().unwrap(),
                0,
                WorkerRunKind::Initial,
                worker_run_id(&outcome_id, 0),
            )
            .await
            .unwrap();
        }

        let replacement = SharedHost::new(model.clone(), "stub").with_store_dir(directory.path());
        let report = replacement
            .define_outcome("recover-thread", "recover", "FINAL", 2)
            .await
            .unwrap();
        assert_eq!(report.iterations[0].result, "satisfied");
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "the committed Worker Run is recovered, not inferred twice"
        );
    }
}
