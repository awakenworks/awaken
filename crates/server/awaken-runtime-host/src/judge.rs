//! Outcome Judge Agent execution and the shared auxiliary Agent tool.
//!
//! The Runtime Host resolves a pinned Judge snapshot and executes it through the
//! same backend-neutral Run boundary as a Worker. Compaction and memory selection
//! still use the ordinary auxiliary-Agent tool below.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::{AGENT_RUN, AgentRunArgs};
use awaken_ext_goal::outcome::{
    Grader as OutcomeGrader, GraderError as OutcomeGraderError, GradingInput, parse_grade,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::host::{SessionCtx, SharedHost};
use crate::outcome_state::{grader_run_id, grader_thread_id};
use crate::run_exec::{Continuity, RunPurpose, SnapshotRunRequest};

/// Default judge instructions. The outcome loop supplies the goal, rubric, and
/// deliverable in the prompt; the judge returns a JSON verdict the grader parses.
pub const DEFAULT_JUDGE_INSTRUCTIONS: &str = "\
You are a strict evaluator. You are given a goal, its rubric, and a deliverable. \
Judge whether the deliverable satisfies the rubric. Reply with ONLY a JSON object \
of the form {\"result\": \"satisfied\" | \"needs_revision\", \"explanation\": \"...\"} \
and nothing else.";

/// A default judge agent config registered under `agent_id`: no tools, a fresh
/// grading window. A host may override by registering its own config for the id.
pub fn default_judge_agent(
    model_ref: &str,
    agent_id: &str,
    instructions: &str,
) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder(agent_id)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(2)
        .build()
}

/// Direct Agent-backed Grader. It executes the pinned Judge snapshot through the
/// same backend-neutral Run boundary as a user turn, on a deterministic fresh
/// Thread, then strictly parses the complete final assistant reply.
pub(crate) struct AgentGrader<'a> {
    pub(crate) host: &'a SharedHost,
    pub(crate) snapshot: &'a ExecutableAgentSnapshot,
    pub(crate) worker_context: &'a SessionCtx,
}

fn grading_prompt(input: &GradingInput) -> Result<String, OutcomeGraderError> {
    serde_json::to_string(input)
        .map(|payload| {
            format!(
                "Evaluate this Outcome input against its rubric. Return ONLY the required JSON object.\n{payload}"
            )
        })
        .map_err(|error| OutcomeGraderError::Execution(error.to_string()))
}

#[async_trait::async_trait]
impl OutcomeGrader for AgentGrader<'_> {
    async fn grade(
        &self,
        input: &GradingInput,
    ) -> Result<awaken_ext_goal::outcome::Grade, OutcomeGraderError> {
        if input.message_start > input.message_end || input.message_end > input.transcript.len() {
            return Err(OutcomeGraderError::Execution(
                "evaluated message range is outside the committed transcript".into(),
            ));
        }
        let thread_id = grader_thread_id(&input.outcome_id, input.iteration);
        let ctx = self
            .host
            .ctx_for(&thread_id.0, None)
            .await
            .map_err(|error| OutcomeGraderError::Execution(error.to_string()))?;
        let run_id = grader_run_id(&input.outcome_id, input.iteration);
        let (state, new_messages) = if let Some(state) = ctx.commit.run_state(&run_id) {
            (state, ctx.commit.committed_messages(&thread_id))
        } else {
            // Make the Judge's live cancellation visible through the Worker
            // Session address used by Managed `user.interrupt`.
            let result = self
                .host
                .execute_snapshot(
                    &ctx,
                    SnapshotRunRequest {
                        run_id: Some(run_id),
                        thread_id,
                        snapshot: self.snapshot.clone(),
                        input: vec![Message::text(
                            MessageId(format!(
                                "outcome/{}/grader/{}/input",
                                input.outcome_id.0, input.iteration
                            )),
                            Role::User,
                            grading_prompt(input)?,
                        )],
                        continuity: Continuity::Fresh,
                        purpose: RunPurpose::OutcomeGrader,
                        model_ref_override: None,
                        supersede: false,
                        sink: None,
                        cancellation_mirror: Some(self.worker_context.cancel.clone()),
                    },
                )
                .await
                .map_err(|error| OutcomeGraderError::Execution(error.to_string()))?;
            (result.state, result.new_messages)
        };
        if state == RunState::Ended(EndCause::Cancelled) {
            return Err(OutcomeGraderError::Interrupted);
        }
        if state != RunState::Ended(EndCause::NaturalEnd) {
            return Err(OutcomeGraderError::Execution(format!(
                "Judge Run ended in {:?}",
                state
            )));
        }
        let reply = new_messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(Message::text_content)
            .ok_or_else(|| OutcomeGraderError::InvalidOutput("Judge returned no reply".into()))?;
        parse_grade(&reply)
    }
}

/// Ordinary Agent-backed tool used by the compactor and memory selector.
/// A developer can provide another `RawTool` with the same `AgentRunArgs` shape;
/// no subagent-specific Runtime contract exists.
pub(crate) struct AuxAgentTool {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) provider: LocalProvider,
    /// Compactor and memory agents are resolved by id from here, so their
    /// model/instructions/window are configured per-agent.
    pub(crate) catalog: Arc<AgentCatalog>,
    pub(crate) seq: AtomicU64,
}

#[async_trait::async_trait]
impl RawTool for AuxAgentTool {
    fn id(&self) -> &str {
        AGENT_RUN
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let request: AgentRunArgs = serde_json::from_value(call.arguments)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let name = format!("{}-agent-run-{n}", request.agent_id);
        // Every sub-run behind this port is out-of-band housekeeping (compaction
        // or memory selection), not the Worker's turn — its usage stays
        // isolated on its own sub-thread rather than folding into the parent tally.
        let (text, _usage) = crate::agent_runner::run_configured_agent(
            &self.catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(&self.provider),
            self.llm.clone(),
            &request.agent_id,
            &name,
            request.seed,
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(ToolOutput::ok(call.call_id, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_goal::outcome::{GradeDecision, Id, Rubric};
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
    use std::sync::Mutex;

    struct FixedJudge {
        reply: String,
        requests: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl LlmExecutor for FixedJudge {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(ChatResponse {
                output: AssistantOutput::text(self.reply.clone()),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn grading_input() -> GradingInput {
        GradingInput {
            outcome_id: Id("agent-grade".into()),
            iteration: 0,
            description: "ship".into(),
            rubric: Rubric("all tests pass".into()),
            transcript: vec![Message::text(
                MessageId("worker-answer".into()),
                Role::Assistant,
                "done",
            )],
            message_start: 0,
            message_end: 1,
            worker_state: serde_json::json!({"version": 2}),
            evidence: Vec::new(),
        }
    }

    #[test]
    fn default_judge_agent_carries_its_id_and_instructions() {
        let cfg = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        assert_eq!(cfg.root_agent_id.0, "judge");
        assert!(cfg.resolved_spec.instructions.contains("strict evaluator"));
        // A judge is pure reasoning: no tools.
        assert!(cfg.resolved_spec.tool_descriptors.is_empty());
    }

    #[tokio::test]
    async fn agent_grader_executes_a_fresh_toolless_run_and_parses_exact_json() {
        let model = Arc::new(FixedJudge {
            reply: r#"{"result":"needs_revision","explanation":"add coverage"}"#.into(),
            requests: Mutex::new(Vec::new()),
        });
        let host = SharedHost::new(model.clone(), "stub");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        let worker_context = host.ctx_for("worker", None).await.unwrap();
        let grade = AgentGrader {
            host: &host,
            snapshot: &snapshot,
            worker_context: worker_context.as_ref(),
        }
        .grade(&grading_input())
        .await
        .unwrap();

        assert_eq!(grade.decision, GradeDecision::NeedsRevision);
        assert_eq!(grade.explanation, "add coverage");
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty());
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|message| message.content.iter().any(|block| {
                    matches!(block, awaken_agent_contract::agent::content::ContentBlock::Text { text } if text.contains("all tests pass"))
                }))
        );
    }

    #[tokio::test]
    async fn agent_grader_rejects_prose_wrapped_json() {
        let model = Arc::new(FixedJudge {
            reply: r#"Result: {"result":"satisfied","explanation":"ok"}"#.into(),
            requests: Mutex::new(Vec::new()),
        });
        let host = SharedHost::new(model, "stub");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        let worker_context = host.ctx_for("worker", None).await.unwrap();
        let error = AgentGrader {
            host: &host,
            snapshot: &snapshot,
            worker_context: worker_context.as_ref(),
        }
        .grade(&grading_input())
        .await
        .unwrap_err();
        assert!(matches!(error, OutcomeGraderError::InvalidOutput(_)));
    }
}
