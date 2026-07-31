//! Managed Host adapter for the extension-owned Outcome controller.

use awaken_ext_goal::controller::{Controller, Error as ControllerError};
use awaken_ext_goal::grader::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};
use awaken_ext_goal::outcome::{Definition, Id};
use awaken_ext_goal::state::Binding;

use crate::host::{HostError, HostOutcomeIteration, HostOutcomeReport, SharedHost};
use crate::judge::HostAgentGrader;
use crate::run_exec::BoundRunExecutor;

impl SharedHost {
    /// Translate Managed's request into the extension application service. Host
    /// retains only locking, backend/context composition, id generation, and DTO
    /// projection; all Outcome transitions and prompts live in `awaken-ext-goal`.
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
        let host_grader = HostAgentGrader {
            host: self,
            worker_cancel: ctx.cancel.clone(),
        };
        let executor = BoundRunExecutor::new(self, ctx.clone());
        let controller = Controller::new(
            &ctx.thread_id,
            ctx.commit.as_ref(),
            ctx.commit.as_ref(),
            &executor,
            awaken_runtime_contract::RuntimeRunContext::new(),
            &host_grader,
        );
        let report = controller
            .define_or_resume(
                Id(awaken_runtime::fresh_process_id("outc")),
                definition,
                binding.clone(),
            )
            .await
            .map_err(controller_error)?;

        Ok(HostOutcomeReport {
            iterations: report
                .iterations
                .into_iter()
                .map(|iteration| HostOutcomeIteration {
                    messages: iteration.messages,
                    outcome_id: iteration.outcome_id.0,
                    iteration: iteration.iteration,
                    result: iteration.result.token().into(),
                    explanation: iteration.explanation,
                })
                .collect(),
        })
    }
}

fn controller_error(error: ControllerError) -> HostError {
    match error {
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
        let model = Arc::new(SequenceModel::new(&[
            "contains FINAL",
            r#"{"result":"satisfied","explanation":"rubric met"}"#,
        ]));
        let host = SharedHost::new(model.clone(), "stub");
        let report = host
            .define_outcome("satisfied", "finish", "FINAL", 3)
            .await
            .unwrap();
        assert_eq!(report.iterations.len(), 1);
        assert_eq!(report.iterations[0].iteration, 0);
        assert_eq!(report.iterations[0].result, "satisfied");
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn revision_feedback_drives_a_second_graded_worker_run() {
        let model = Arc::new(SequenceModel::new(&[
            "draft",
            r#"{"result":"needs_revision","explanation":"add FINAL"}"#,
            "now FINAL",
            r#"{"result":"satisfied","explanation":"rubric met"}"#,
        ]));
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
        assert_eq!(model.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn exhausted_budget_runs_one_ungraded_acknowledgment() {
        let model = Arc::new(SequenceModel::new(&[
            "draft",
            r#"{"result":"needs_revision","explanation":"add FINAL"}"#,
            "acknowledged",
        ]));
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
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    }
}
