// @formal surface requirement=runtime.outcomes.outcome_lifecycle
// @formal reason: The Coordinator drives a Worker-placed Session's Outcome through the extension-owned
// @formal reason: controller; its Worker and Grader Runs are dispatched to registered Workers as ordinary
// @formal reason: Runs and observed through committed truth. Classification, not a formal-proof claim.
//! Outcomes of Worker-placed Sessions on the Coordinator.
//!
//! The extension-owned Outcome controller decides every transition; this
//! drives it over the Session's committed truth. Each Worker Run and each
//! Grader Run the controller starts is dispatched to a registered Worker as an
//! ordinary Run and observed until it settles: no execution context opens on
//! the Coordinator.

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_goal::controller::{Controller, Error as ControllerError};
use awaken_ext_goal::grader::{
    AgentGrader, DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent_from_worker,
};
use awaken_ext_goal::outcome::{Definition, Grade, Grader, GraderError, GradingInput, Id};
use awaken_ext_goal::state::{Binding, grader_thread_id};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_session_execution_contract::OutcomeDrive;
use awaken_session_types::RunError;

use super::WorkerPlacedSessions;
use super::dispatched_runs::DispatchedRuns;
use crate::{outcome_controller_error, outcome_report};

/// The Grader of a Worker-placed Session's Outcome: each grading is a Run of
/// the round's own grader Thread, in the Session's Workspace.
struct DispatchedGrader<'a> {
    sessions: &'a WorkerPlacedSessions,
    workspace: String,
}

#[async_trait::async_trait]
impl Grader for DispatchedGrader<'_> {
    async fn grade(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        input: &GradingInput,
    ) -> Result<Grade, GraderError> {
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let execution = |error: RunError| GraderError::Execution(error.to_string());
        self.sessions
            .ports
            .projections
            .register_thread_workspace(&thread.0, &self.workspace)
            .map_err(execution)?;
        let commit = self
            .sessions
            .ports
            .reads
            .commit_for_read(&thread.0)
            .await
            .map_err(execution)?;
        let runs = DispatchedRuns::new(
            self.sessions.ports.projections.as_ref(),
            self.sessions.ports.owner.as_ref(),
            &self.sessions.ports.completion,
            thread,
            commit.clone(),
            false,
        );
        AgentGrader::new(&runs, commit.as_ref(), RuntimeRunContext::new())
            .grade(snapshot, input)
            .await
    }
}

enum OutcomeCommand<'a> {
    Prepare {
        outcome_id: &'a str,
        definition: Definition,
    },
    Resume,
}

enum OutcomeCommandResult {
    Prepared(u64),
    Driven(Option<OutcomeDrive>),
}

impl WorkerPlacedSessions {
    async fn execute_outcome_command(
        &self,
        thread: &str,
        command: OutcomeCommand<'_>,
    ) -> Result<OutcomeCommandResult, RunError> {
        let projection = self.ports.projections.installed(thread)?;
        let worker = self.ports.projections.primary_snapshot(thread)?;
        let binding = Binding {
            grader: default_judge_agent_from_worker(
                &worker,
                "outcome-grader",
                DEFAULT_JUDGE_INSTRUCTIONS,
            ),
            worker,
        };
        let thread_id = ThreadId(thread.to_owned());
        let commit = self.ports.reads.commit_for_read(thread).await?;
        let grader = DispatchedGrader {
            sessions: self,
            workspace: projection.workspace().to_owned(),
        };
        let runs = DispatchedRuns::new(
            self.ports.projections.as_ref(),
            self.ports.owner.as_ref(),
            &self.ports.completion,
            thread_id.clone(),
            commit.clone(),
            true,
        );
        let controller = Controller::new(
            &thread_id,
            commit.as_ref(),
            commit.as_ref(),
            commit.as_ref(),
            &runs,
            RuntimeRunContext::new(),
            &grader,
        );
        let lock = self.outcome_command(thread)?;
        let _command = lock.lock().await;
        match command {
            OutcomeCommand::Prepare {
                outcome_id,
                definition,
            } => controller
                .prepare(Id(outcome_id.to_owned()), definition, binding)
                .await
                .map(OutcomeCommandResult::Prepared)
                .map_err(outcome_controller_error),
            OutcomeCommand::Resume => match controller.resume_active().await {
                Ok(Some(report)) => Ok(OutcomeCommandResult::Driven(Some(
                    OutcomeDrive::Completed(outcome_report(report)),
                ))),
                Ok(None) | Err(ControllerError::WorkerRunning { .. }) => {
                    Ok(OutcomeCommandResult::Driven(None))
                }
                Err(ControllerError::WorkerAwaiting { .. }) => {
                    Ok(OutcomeCommandResult::Driven(Some(OutcomeDrive::Awaiting)))
                }
                Err(error) => Err(outcome_controller_error(error)),
            },
        }
    }

    /// Persist only the extension-owned Outcome aggregate: no Worker or Grader
    /// Run is dispatched before the caller admits Session activity.
    async fn prepare_session_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        let definition = Definition::new(description, rubric, max_iterations)
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        match self
            .execute_outcome_command(
                thread,
                OutcomeCommand::Prepare {
                    outcome_id,
                    definition,
                },
            )
            .await?
        {
            OutcomeCommandResult::Prepared(cursor) => Ok(cursor),
            OutcomeCommandResult::Driven(_) => {
                Err(RunError::internal("Outcome preparation drove the Outcome"))
            }
        }
    }

    /// Continue only the sole durable active Outcome aggregate.
    async fn continue_session_outcome(
        &self,
        thread: &str,
    ) -> Result<Option<OutcomeDrive>, RunError> {
        match self
            .execute_outcome_command(thread, OutcomeCommand::Resume)
            .await?
        {
            OutcomeCommandResult::Driven(progress) => Ok(progress),
            OutcomeCommandResult::Prepared(_) => Err(RunError::internal(
                "Outcome continuation prepared an Outcome",
            )),
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for WorkerPlacedSessions {
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        self.prepare_session_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        let outcome_id = awaken_session_event_contract::session_outcome_convenience_id(
            thread,
            description,
            rubric,
            max_iterations,
        );
        self.prepare_session_outcome(thread, &outcome_id, description, rubric, max_iterations)
            .await?;
        self.continue_session_outcome(thread)
            .await?
            .ok_or_else(|| RunError::internal("defined Outcome aggregate was not persisted"))
    }

    async fn continue_outcome(&self, thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        self.continue_session_outcome(thread).await
    }
}
