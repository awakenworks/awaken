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

use std::sync::Arc;

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::DispatchEnqueue as _;
use awaken_ext_goal::controller::{Controller, Error as ControllerError};
use awaken_ext_goal::grader::{
    AgentGrader, DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent_from_worker,
};
use awaken_ext_goal::outcome::{Definition, Grade, Grader, GraderError, GradingInput, Id};
use awaken_ext_goal::state::{Binding, grader_thread_id};
use awaken_runtime_contract::execution::{Error as ExecutionError, Result as ExecutionResult};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation, RuntimeRunContext};
use awaken_session_execution_contract::OutcomeDrive;
use awaken_session_types::RunError;

use super::{SETTLED_RECONCILIATION_INTERVAL, WorkerPlacedSessions};
use crate::session_run_activation::read_settled_phase;
use crate::{
    SessionCommit, SessionLowering as _, await_completion_state, outcome_controller_error,
    outcome_report,
};

/// Runs of one Thread, dispatched to registered Workers and observed until
/// they settle.
struct DispatchedRuns<'a> {
    sessions: &'a WorkerPlacedSessions,
    thread: ThreadId,
    commit: Arc<SessionCommit>,
    /// An Outcome's Worker Runs extend the Session's Thread; a Grader Run is a
    /// fresh Run of its own Thread.
    extension: bool,
}

impl DispatchedRuns<'_> {
    async fn dispatch(&self, activation: RunActivation) -> Result<RunState, RunError> {
        if activation.thread_id != self.thread {
            return Err(RunError::internal(
                "Run activation does not match its bound Thread",
            ));
        }
        let projections = self.sessions.ports.projections.as_ref();
        let session_dispatch = projections.projection(&self.thread.0)?.is_some();
        let activation = awaken_dispatch_execution::placement::bind_root_tool_configuration_format(
            activation,
            session_dispatch,
        );
        let run_id = activation.run_id.clone();
        let (settled, _waiter_guard) = self.sessions.ports.completion.register(&run_id, None)?;
        let preparation = projections.prepare(activation);
        let request = if self.extension {
            preparation.extension()
        } else {
            preparation.fresh()
        }?;
        self.sessions
            .ports
            .owner
            .enqueue(request)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        await_completion_state(settled, SETTLED_RECONCILIATION_INTERVAL, || {
            read_settled_phase(&self.commit, &self.thread, &run_id, None)
        })
        .await
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for DispatchedRuns<'_> {
    async fn execute(
        &self,
        activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.dispatch(activation)
            .await
            .map_err(|error| ExecutionError::Execution(error.to_string()))
    }
}

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
        let runs = DispatchedRuns {
            sessions: self.sessions,
            thread,
            commit: commit.clone(),
            extension: false,
        };
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
        let runs = DispatchedRuns {
            sessions: self,
            thread: thread_id.clone(),
            commit: commit.clone(),
            extension: true,
        };
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
