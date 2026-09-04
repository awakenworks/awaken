//! Managed Host adapter for the extension-owned Outcome controller.

use awaken_ext_goal::controller::{CommittedOutcome, Controller, Error as ControllerError};
use awaken_ext_goal::grader::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent_from_worker};
use awaken_ext_goal::outcome::{Definition, Id};
use awaken_ext_goal::state::Binding;
use awaken_session_contract::OUTCOME_BUSY_CODE;

use crate::host::{
    HostError, HostOutcomeDrive, HostOutcomeIteration, HostOutcomeReport, SharedHost,
};
use crate::judge::HostAgentGrader;
use crate::run_exec::BoundRunExecutor;

enum OutcomeCommand<'a> {
    Prepare {
        outcome_id: &'a str,
        description: &'a str,
        rubric: &'a str,
        max_iterations: u32,
    },
    Resume,
    Read {
        outcome_id: &'a str,
    },
}

enum OutcomeCommandResult {
    Prepared(u64),
    Driven(Option<HostOutcomeDrive>),
    Projected(Option<CommittedOutcome>),
}

impl SharedHost {
    /// Translate Managed's request into the extension application service. Host
    /// retains only locking, backend/context selection, id generation, and DTO
    /// projection; all Outcome transitions and prompts live in `awaken-ext-goal`.
    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<HostOutcomeDrive, HostError> {
        self.prepare_outcome(
            thread,
            &awaken_session_contract::session_outcome_convenience_id(
                thread,
                description,
                rubric,
                max_iterations,
            ),
            description,
            rubric,
            max_iterations,
        )
        .await?;
        self.continue_outcome(thread)
            .await?
            .ok_or_else(|| HostError::internal("defined Outcome aggregate was not persisted"))
    }

    /// Persist only the extension-owned Outcome aggregate. No Worker or Grader
    /// Run executes before the caller admits Session activity.
    pub(crate) async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, HostError> {
        match self
            .execute_outcome_command(
                thread,
                OutcomeCommand::Prepare {
                    outcome_id,
                    description,
                    rubric,
                    max_iterations,
                },
            )
            .await?
        {
            OutcomeCommandResult::Prepared(cursor) => Ok(cursor),
            OutcomeCommandResult::Driven(_) | OutcomeCommandResult::Projected(_) => {
                unreachable!("prepare command result")
            }
        }
    }

    /// Continue only the sole durable active Outcome aggregate.
    pub async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<Option<HostOutcomeDrive>, HostError> {
        match self
            .execute_outcome_command(thread, OutcomeCommand::Resume)
            .await?
        {
            OutcomeCommandResult::Driven(progress) => Ok(progress),
            OutcomeCommandResult::Prepared(_) | OutcomeCommandResult::Projected(_) => {
                unreachable!("resume command result")
            }
        }
    }

    /// Read one exact terminal projection through the existing Outcome
    /// controller composition. The Host adds no cache and performs no Outcome
    /// transition.
    pub(crate) async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcome>, HostError> {
        match self
            .execute_outcome_command(thread, OutcomeCommand::Read { outcome_id })
            .await?
        {
            OutcomeCommandResult::Projected(report) => Ok(report),
            OutcomeCommandResult::Prepared(_) | OutcomeCommandResult::Driven(_) => {
                unreachable!("read command result")
            }
        }
    }

    async fn execute_outcome_command(
        &self,
        thread: &str,
        command: OutcomeCommand<'_>,
    ) -> Result<OutcomeCommandResult, HostError> {
        let definition = match &command {
            OutcomeCommand::Prepare {
                description,
                rubric,
                max_iterations,
                ..
            } => Some(
                Definition::new(*description, *rubric, *max_iterations)
                    .map_err(|error| HostError::bad_request(error.to_string()))?,
            ),
            OutcomeCommand::Resume | OutcomeCommand::Read { .. } => None,
        };
        let ctx = self.ctx_for(thread, None).await?;
        let binding = outcome_binding(&ctx.config, self.judge_snapshot.as_ref());
        let host_grader = HostAgentGrader {
            host: self,
            worker_cancel: ctx.cancel.clone(),
        };
        // Outcome owns ordinary Worker Runs on the shared Thread. They reuse
        // the Session's frozen execution inputs but are not a second public
        // Session Run admission or Session activity owner.
        let executor = BoundRunExecutor::new(self, ctx.clone()).for_thread_extension();
        let controller = Controller::new(
            &ctx.thread_id,
            ctx.commit.as_ref(),
            ctx.commit.as_ref(),
            ctx.commit.as_ref(),
            &executor,
            awaken_runtime_contract::RuntimeRunContext::new(),
            &host_grader,
        );
        match command {
            OutcomeCommand::Prepare { outcome_id, .. } => {
                let _outcome = ctx.outcome.lock().await;
                let _command = ctx.command.lock().await;
                controller
                    .prepare(
                        Id(outcome_id.to_string()),
                        definition.expect("prepare definition"),
                        binding,
                    )
                    .await
                    .map(OutcomeCommandResult::Prepared)
                    .map_err(controller_error)
            }
            OutcomeCommand::Resume => {
                let _outcome = ctx.outcome.lock().await;
                let _command = ctx.command.lock().await;
                // A reply may already have dispatched the Outcome's stable
                // Worker Run while the lifecycle supervisor starts another
                // reconciliation pass. Re-entering that Running Run would
                // recover its open tool batch before the new Awaiting ticket is
                // committed and turn a valid permission wait indeterminate.
                // This process-local hint is only a live-owner fence: after a
                // crash it is absent and the durable Outcome/Run state remains
                // fully recoverable by the replacement process.
                if ctx
                    .active_run
                    .lock()
                    .expect("active run mutex poisoned")
                    .is_some()
                {
                    return Ok(OutcomeCommandResult::Driven(None));
                }
                match controller.resume_active().await {
                    Ok(Some(report)) => Ok(OutcomeCommandResult::Driven(Some(
                        HostOutcomeDrive::Completed(project_report(report)),
                    ))),
                    Ok(None) => Ok(OutcomeCommandResult::Driven(None)),
                    Err(ControllerError::WorkerAwaiting { .. }) => Ok(
                        OutcomeCommandResult::Driven(Some(HostOutcomeDrive::Awaiting)),
                    ),
                    Err(ControllerError::WorkerRunning { .. }) => {
                        Ok(OutcomeCommandResult::Driven(None))
                    }
                    Err(error) => Err(controller_error(error)),
                }
            }
            // The committed Outcome projection is a pure snapshot read. It
            // must not join the execution mutexes used to serialize Worker and
            // Grader effects: inbound receipt admission refreshes this query,
            // and an interrupt must be durably accepted while those effects
            // are still running.
            OutcomeCommand::Read { outcome_id } => controller
                .committed_projection(&Id(outcome_id.to_string()))
                .await
                .map(OutcomeCommandResult::Projected)
                .map_err(controller_error),
        }
    }
}

fn outcome_binding(
    worker: &awaken_runtime_contract::ExecutableAgentSnapshot,
    configured_judge: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> Binding {
    Binding {
        worker: worker.clone(),
        grader: configured_judge.cloned().unwrap_or_else(|| {
            default_judge_agent_from_worker(worker, "outcome-grader", DEFAULT_JUDGE_INSTRUCTIONS)
        }),
    }
}

pub(crate) fn project_report(report: awaken_ext_goal::controller::Report) -> HostOutcomeReport {
    HostOutcomeReport {
        iterations: report
            .iterations
            .into_iter()
            .map(|iteration| HostOutcomeIteration {
                messages: iteration.messages,
                outcome_id: iteration.outcome_id.0,
                description: iteration.description,
                iteration: iteration.iteration,
                result: iteration.result.token().into(),
                explanation: iteration.explanation,
            })
            .collect(),
    }
}

fn controller_error(error: ControllerError) -> HostError {
    match error {
        ControllerError::Busy { .. } | ControllerError::WorkerRunning { .. } => {
            HostError::unavailable_classified(OUTCOME_BUSY_CODE, error.to_string())
        }
        ControllerError::ActiveDefinitionConflict { .. }
        | ControllerError::WorkerAwaiting { .. }
        | ControllerError::Domain(_) => HostError::bad_request(error.to_string()),
        ControllerError::Persistence(_)
        | ControllerError::Execution(_)
        | ControllerError::Serialization(_) => HostError::internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
    use std::collections::VecDeque;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    #[test]
    fn outcome_binding_uses_one_explicit_or_worker_derived_judge_authority() {
        use awaken_runtime_contract::ExecutableAgentSnapshot;
        use awaken_runtime_contract::resolved::ModelBinding;

        // Cause/effect graph: C1 a composition pins an explicit Judge; C2 the
        // Worker freezes an exact Managed model route. Effects: E1 C1 wins
        // byte-for-byte; E2 without C1, one tool-free Judge derives from C2.
        // Constraint: C1 and E2 are mutually exclusive—there is no fallback or
        // second mutable Judge lookup after the Outcome binding is committed.
        //
        // | Rule | C1 | C2 | Judge authority |
        // | R1   | T  | T  | explicit C1     |
        // | R2   | F  | T  | derived C2      |
        let worker = ExecutableAgentSnapshot::builder("worker")
            .model(ModelBinding::new("cred:codex", "", "acp:codex"))
            .build();
        let explicit = ExecutableAgentSnapshot::builder("explicit-judge")
            .model(ModelBinding::new("cred:judge", "judge-model", "awaken"))
            .build();

        let pinned = outcome_binding(&worker, Some(&explicit));
        assert_eq!(pinned.grader, explicit, "R1/E1");
        let derived = outcome_binding(&worker, None);
        assert_eq!(
            derived.grader.resolved_spec.model_binding, worker.resolved_spec.model_binding,
            "R2/E2"
        );
        assert!(
            derived.grader.resolved_spec.tool_descriptors.is_empty(),
            "R2/E2"
        );
    }

    fn completed(progress: HostOutcomeDrive) -> HostOutcomeReport {
        match progress {
            HostOutcomeDrive::Completed(report) => report,
            HostOutcomeDrive::Awaiting => panic!("test Outcome unexpectedly awaited input"),
        }
    }

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

    /// Complete ordinary Host composition shared by Outcome controller tests.
    /// The controller remains the sole Outcome owner; this fixture supplies only
    /// the production Dispatch runtime required by context construction.
    fn outcome_test_host(model: Arc<dyn LlmExecutor>) -> Arc<SharedHost> {
        let host = Arc::new(SharedHost::new(model, "stub"));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        host
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
        let model = Arc::new(SequenceModel::new(&[
            "contains FINAL",
            r#"{"result":"satisfied","explanation":"rubric met"}"#,
        ]));
        let host = outcome_test_host(model.clone());
        let report = completed(
            host.define_outcome("satisfied", "finish", "FINAL", 3)
                .await
                .unwrap(),
        );
        assert_eq!(report.iterations.len(), 1);
        assert_eq!(report.iterations[0].iteration, 0);
        assert_eq!(report.iterations[0].result, "satisfied");
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }

    /// Host phase decision table: H1 new stable Outcome command prepares one
    /// aggregate with zero model calls; H2 exact replay returns the same id with
    /// zero model calls; H3 explicit continuation alone drives Worker/Grader
    /// Runs. Effects: E1 durability precedes Session activity, E2 idempotency,
    /// E3 ordinary execution. Causes: H1 new prepare, H2 exact replay, H3 explicit
    /// continuation. Constraint/Invariant: prepare never executes a Run.
    /// Decision rule: execute H1-H3 and require E1-E3 at their separate phases.
    #[tokio::test]
    async fn outcome_prepare_is_stable_and_does_not_execute_runs() {
        // Decision rule: execute H1-H3 across prepare, replay, and continuation.
        let model = Arc::new(SequenceModel::new(&[
            "contains FINAL",
            r#"{"result":"satisfied","explanation":"rubric met"}"#,
        ]));
        let host = outcome_test_host(model.clone());
        host.prepare_outcome("prepared", "stable-outcome", "finish", "FINAL", 3)
            .await
            .expect("H1 prepare");
        assert_eq!(model.calls.load(Ordering::SeqCst), 0, "H1/E1");

        host.prepare_outcome("prepared", "stable-outcome", "finish", "FINAL", 3)
            .await
            .expect("H2 replay");
        assert_eq!(model.calls.load(Ordering::SeqCst), 0, "H2/E2");

        let report = completed(
            host.continue_outcome("prepared")
                .await
                .expect("H3 continue")
                .expect("H3 active"),
        );
        assert_eq!(report.iterations.len(), 1, "H3/E3");
        assert_eq!(model.calls.load(Ordering::SeqCst), 2, "H3/E3");
    }

    /// Cause/effect graph: C1 an Outcome Worker/Judge command owns both Host
    /// command-ordering locks; C2 a concurrent adapter refresh asks only for an exact
    /// committed Outcome projection. Effects: E1 C2 completes from committed
    /// truth without waiting for C1; E2 no model call or Outcome transition is
    /// introduced. Constraint: mutating Prepare/Resume commands remain
    /// serialized by both locks.
    ///
    /// | Rule | C1 locks held | C2 read | Effect |
    /// | R1   | yes           | yes     | E1 + E2 |
    /// Decision rule: R1 must complete within the timeout while both mutation
    /// locks remain held, proving the committed read is lock-independent.
    #[tokio::test]
    async fn committed_outcome_read_does_not_wait_for_command_locks() {
        let model = Arc::new(SequenceModel::new(&[]));
        let host = outcome_test_host(model.clone());
        let ctx = host
            .ctx_for("concurrent-read", None)
            .await
            .expect("R1 Session context");
        let _outcome = ctx.outcome.lock().await;
        let _command = ctx.command.lock().await;

        let report = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            host.committed_outcome_projection("concurrent-read", "outcome-missing"),
        )
        .await
        .expect("R1/E1 committed read must not join execution locks")
        .expect("R1/E1 committed read");
        assert!(report.is_none(), "R1/E1 absent committed Outcome");
        assert_eq!(model.calls.load(Ordering::SeqCst), 0, "R1/E2");
    }

    #[tokio::test]
    async fn revision_feedback_drives_a_second_graded_worker_run() {
        let model = Arc::new(SequenceModel::new(&[
            "draft",
            r#"{"result":"needs_revision","explanation":"add FINAL"}"#,
            "now FINAL",
            r#"{"result":"satisfied","explanation":"rubric met"}"#,
        ]));
        let host = outcome_test_host(model.clone());
        let report = completed(
            host.define_outcome("revision", "finish", "FINAL", 3)
                .await
                .unwrap(),
        );
        assert_eq!(
            report
                .iterations
                .iter()
                .map(|round| (round.iteration, round.result.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "needs_revision"), (1, "satisfied")]
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn exhausted_budget_runs_one_ungraded_acknowledgment() {
        let model = Arc::new(SequenceModel::new(&[
            "draft",
            r#"{"result":"needs_revision","explanation":"add FINAL"}"#,
            "acknowledged",
        ]));
        let host = outcome_test_host(model.clone());
        let report = completed(
            host.define_outcome("max", "finish", "FINAL", 1)
                .await
                .unwrap(),
        );
        assert_eq!(report.iterations.len(), 1);
        assert_eq!(report.iterations[0].iteration, 0);
        assert_eq!(report.iterations[0].result, "max_iterations_reached");
        assert!(
            report.iterations[0]
                .messages
                .iter()
                .any(|message| message.text_content() == "acknowledged")
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    }
}
