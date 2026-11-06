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
use awaken_dispatch_contract::{
    DispatchReads as _, SessionRunAdmissionObservation, SessionRunAdmissionReads as _,
};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_ext_goal::controller::{Controller, Error as ControllerError};
use awaken_ext_goal::grader::{
    AgentGrader, DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent_from_worker,
};
use awaken_ext_goal::outcome::{Definition, Grade, Grader, GraderError, GradingInput, Id};
use awaken_ext_goal::run_settlement::{OutcomeRuns, RunSettlement};
use awaken_ext_goal::state::{Binding, grader_thread_id};
use awaken_runtime_contract::runs::RunError;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_session_execution_contract::OutcomeDrive;

use super::WorkerPlacedSessions;
use super::dispatched_runs::DispatchedRuns;
use crate::{outcome_controller_error, outcome_report};

/// The Execution-owned SDK adapter for a Session application's borrowed
/// prepared admission. Both local and Worker delivery use this same bridge;
/// it stores no application binding and preserves typed diagnostic evidence.
/// Settlement reads go to the Run's durable dispatch completion.
pub struct OutcomeRunExecutor<'a> {
    admission: &'a dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
    settlements: &'a AnyCoordinatorDispatch,
}

impl<'a> OutcomeRunExecutor<'a> {
    #[must_use]
    pub fn new(
        admission: &'a dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
        settlements: &'a AnyCoordinatorDispatch,
    ) -> Self {
        Self {
            admission,
            settlements,
        }
    }
}

/// The Outcome settlement of one dispatch observation. `live_cancellation`
/// is the still-live row's durable cancel request, read only for `Live`.
///
/// | Rule | observation             | live row cancel | settlement                                |
/// |------|-------------------------|-----------------|-------------------------------------------|
/// | S1   | Completed, cancel true  | not read        | Settled { cancellation_requested: true }  |
/// | S2   | Completed, cancel false | not read        | Settled { cancellation_requested: false } |
/// | S3   | Live                    | true / false    | Settled with the row's durable request    |
/// | S4   | Live                    | row gone        | Unsettled: the next drive reads again     |
/// | S5   | Missing                 | not read        | Settled { cancellation_requested: false }: no durable cancel evidence |
fn run_settlement(
    observation: &SessionRunAdmissionObservation,
    live_cancellation: Option<bool>,
) -> RunSettlement {
    match (observation, live_cancellation) {
        (SessionRunAdmissionObservation::Completed { completion, .. }, _) => {
            RunSettlement::Settled {
                cancellation_requested: completion.cancellation_requested,
            }
        }
        (SessionRunAdmissionObservation::Live(_), Some(cancellation_requested)) => {
            RunSettlement::Settled {
                cancellation_requested,
            }
        }
        (SessionRunAdmissionObservation::Live(_), None) => RunSettlement::Unsettled,
        (SessionRunAdmissionObservation::Missing, _) => RunSettlement::Settled {
            cancellation_requested: false,
        },
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for OutcomeRunExecutor<'_> {
    async fn execute(
        &self,
        activation: awaken_runtime_contract::RunActivation,
        _context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<awaken_agent_contract::agent::run::RunState>
    {
        self.admission.admit(activation).await
    }
}

#[async_trait::async_trait]
impl OutcomeRuns for OutcomeRunExecutor<'_> {
    async fn settlement(
        &self,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> awaken_runtime_contract::execution::Result<RunSettlement> {
        read_run_settlement(self.settlements, run_id).await
    }
}

/// Read one Run's Outcome settlement from its durable dispatch evidence: the
/// completion tombstone, or the still-live row's cancel request. An
/// unavailable read is an error, never a guessed settlement.
pub async fn read_run_settlement(
    dispatch: &AnyCoordinatorDispatch,
    run_id: &awaken_agent_contract::agent::run::Id,
) -> awaken_runtime_contract::execution::Result<RunSettlement> {
    let unavailable = |error: awaken_dispatch_contract::DispatchError| {
        awaken_runtime_contract::execution::Error::from(
            awaken_runtime_contract::diagnostic::RuntimeDiagnostic::new(
                awaken_runtime_contract::diagnostic::RuntimeErrorKind::Unavailable,
                "run_settlement_unavailable",
                error,
            ),
        )
    };
    let observation = dispatch
        .observe_session_run_admission(run_id)
        .await
        .map_err(unavailable)?;
    let live_cancellation = match observation {
        SessionRunAdmissionObservation::Live(_) => dispatch
            .list_dispatches()
            .await
            .map_err(unavailable)?
            .into_iter()
            .find(|row| &row.run_id == run_id)
            .map(|row| row.cancellation_requested),
        _ => None,
    };
    Ok(run_settlement(&observation, live_cancellation))
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
    /// Continuing the active Outcome drives its Worker Run through the
    /// caller's executor; preparation never executes, so only this case has one.
    Resume { executor: &'a dyn OutcomeRuns },
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
        let controller = match &command {
            OutcomeCommand::Resume { executor } => Controller::new(
                &thread_id,
                commit.as_ref(),
                commit.as_ref(),
                commit.as_ref(),
                *executor,
                RuntimeRunContext::new(),
                &grader,
            ),
            OutcomeCommand::Prepare { .. } => Controller::preparation(
                &thread_id,
                commit.as_ref(),
                commit.as_ref(),
                commit.as_ref(),
                &grader,
            ),
        };
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
            OutcomeCommand::Resume { .. } => match controller.resume_active().await {
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
        executor: &dyn OutcomeRuns,
    ) -> Result<Option<OutcomeDrive>, RunError> {
        match self
            .execute_outcome_command(thread, OutcomeCommand::Resume { executor })
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
impl awaken_session_execution_contract::Outcomes for WorkerPlacedSessions {
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
        admission: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
    ) -> Result<OutcomeDrive, RunError> {
        let outcome_id = awaken_session_contract::session_outcome_convenience_id(
            thread,
            description,
            rubric,
            max_iterations,
        );
        self.prepare_session_outcome(thread, &outcome_id, description, rubric, max_iterations)
            .await?;
        self.continue_session_outcome(
            thread,
            &OutcomeRunExecutor::new(admission, self.ports.owner.as_ref()),
        )
        .await?
        .ok_or_else(|| RunError::internal("defined Outcome aggregate was not persisted"))
    }

    async fn continue_outcome(
        &self,
        thread: &str,
        admission: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
    ) -> Result<Option<OutcomeDrive>, RunError> {
        self.continue_session_outcome(
            thread,
            &OutcomeRunExecutor::new(admission, self.ports.owner.as_ref()),
        )
        .await
    }
}

#[cfg(test)]
mod admission_adapter_tests {
    use super::{OutcomeRunExecutor, run_settlement};
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    use awaken_dispatch_contract::SessionRunAdmissionObservation::{
        self, Completed, Live, Missing,
    };
    use awaken_ext_goal::run_settlement::RunSettlement;
    use awaken_runtime_contract::diagnostic::{
        RunAdmissionDisposition, RunAdmissionFailure, RuntimeDiagnostic, RuntimeErrorKind,
        run_admission_disposition,
    };
    use awaken_runtime_contract::execution::RunExecutor as _;
    use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation, RuntimeRunContext};
    use std::sync::Mutex;

    struct Admission {
        seen: Mutex<Option<RunActivation>>,
        refusal: Option<RunAdmissionDisposition>,
    }

    fn completion(cancellation_requested: bool) -> SessionRunAdmissionObservation {
        Completed {
            completion: awaken_dispatch_contract::DispatchCompletion {
                sequence: 1,
                run_id: awaken_agent_contract::agent::run::Id("outcome/settled/worker/0".into()),
                thread_id: None,
                session_thread_id: None,
                request_fingerprint: None,
                cancellation_requested,
            },
            retained_request: Ok(None),
        }
    }

    #[test]
    fn settlement_reads_the_durable_cancel_request() {
        // Every row of `run_settlement`.
        let live = || {
            Live(Box::new(awaken_dispatch_contract::RunDispatch::new(
                RunActivation::new(
                    awaken_agent_contract::agent::run::Id("outcome/settled/worker/0".into()),
                    awaken_agent_contract::ThreadId("session".into()),
                    ExecutableAgentSnapshot::builder("worker")
                        .model(
                            awaken_runtime_contract::resolved::ModelBinding::parse(
                                "test", "model", "native",
                            )
                            .expect("valid native activation fixture"),
                        )
                        .build(),
                    Vec::new(),
                ),
            )))
        };
        let settled = |cancellation_requested| RunSettlement::Settled {
            cancellation_requested,
        };
        for (rule, observation, live_cancellation, expected) in [
            ("S1", completion(true), None, settled(true)),
            ("S2", completion(false), None, settled(false)),
            ("S3 cancel", live(), Some(true), settled(true)),
            ("S3 plain", live(), Some(false), settled(false)),
            ("S4", live(), None, RunSettlement::Unsettled),
            ("S5", Missing, None, settled(false)),
        ] {
            assert_eq!(
                run_settlement(&observation, live_cancellation),
                expected,
                "{rule}"
            );
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::PreparedSessionRunAdmission for Admission {
        async fn admit(
            &self,
            activation: RunActivation,
        ) -> awaken_runtime_contract::diagnostic::Result<RunState> {
            *self.seen.lock().unwrap() = Some(activation);
            match self.refusal {
                None => Ok(RunState::Ended(EndCause::NaturalEnd)),
                Some(disposition) => Err(RuntimeDiagnostic::new(
                    RuntimeErrorKind::Unavailable,
                    "owner_admission",
                    RunAdmissionFailure::new(
                        disposition,
                        awaken_runtime_contract::runs::RunError::unavailable("owner evidence"),
                    ),
                )
                .into()),
            }
        }
    }

    #[tokio::test]
    async fn sdk_bridge_preserves_complete_activation_and_owner_disposition() {
        // Cause/effect graph: C1 complete prepared activation; C2 owner accepts,
        // defers or durably closes it. E1 the neutral owner receives C1 unchanged;
        // E2 acceptance returns its state; E3 refusal retains its typed cause/code.
        // Rules C2=accepted => E1+E2; C2=Deferred/Closed => E1+E3. Constraint:
        // this bridge does not classify text, execute, or create another admission.
        let activation = RunActivation::new(
            awaken_agent_contract::agent::run::Id("outcome/bridge/worker/0".into()),
            awaken_agent_contract::ThreadId("session".into()),
            ExecutableAgentSnapshot::builder("frozen-worker")
                .model(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "test", "model", "native",
                    )
                    .expect("valid native prepared activation fixture"),
                )
                .build(),
            Vec::new(),
        )
        .with_model_ref_override(Some("frozen-model".into()))
        .without_tools();
        let dispatch = awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(awaken_time::system_clock()),
            None,
        );
        for refusal in [
            None,
            Some(RunAdmissionDisposition::Deferred),
            Some(RunAdmissionDisposition::Closed),
        ] {
            let admission = Admission {
                seen: Mutex::new(None),
                refusal,
            };
            let result = OutcomeRunExecutor::new(&admission, &dispatch)
                .execute(activation.clone(), RuntimeRunContext::new())
                .await;
            assert_eq!(
                admission.seen.lock().unwrap().as_ref(),
                Some(&activation),
                "E1"
            );
            match refusal {
                None => assert_eq!(result.unwrap(), RunState::Ended(EndCause::NaturalEnd), "E2"),
                Some(disposition) => {
                    let error = result.unwrap_err();
                    assert_eq!(run_admission_disposition(&error), Some(disposition), "E3");
                    assert_eq!(error.code(), "owner_admission", "E3");
                }
            }
        }
    }
}
