//! Host adapter for ordinary auxiliary Agent Runs used by compaction and Memory.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_ext_builtin_tools::{AGENT_RUN, AgentRunArgs};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;

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
    use crate::host::SharedHost;
    use crate::run_exec::BoundRunExecutor;
    use awaken_ext_goal::grader::{AgentGrader, DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};
    use awaken_ext_goal::outcome::{GradeDecision, Grader, GraderError, GradingInput, Id, Rubric};
    use awaken_ext_goal::state::grader_thread_id;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
    use awaken_runtime_contract::{Message, MessageId, Role, RuntimeRunContext};
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

    #[tokio::test]
    async fn agent_grader_executes_a_fresh_toolless_run_and_parses_exact_json() {
        let model = Arc::new(FixedJudge {
            reply: r#"{"result":"needs_revision","explanation":"add coverage"}"#.into(),
            requests: Mutex::new(Vec::new()),
        });
        let host = SharedHost::new(model.clone(), "stub");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        let input = grading_input();
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let context = host.ctx_for(&thread.0, None).await.unwrap();
        let executor = BoundRunExecutor::new(&host, context.clone());
        let grade = AgentGrader::new(
            &executor,
            context.commit.as_ref(),
            &snapshot,
            RuntimeRunContext::new(),
        )
        .grade(&input)
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
        let input = grading_input();
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let context = host.ctx_for(&thread.0, None).await.unwrap();
        let executor = BoundRunExecutor::new(&host, context.clone());
        let error = AgentGrader::new(
            &executor,
            context.commit.as_ref(),
            &snapshot,
            RuntimeRunContext::new(),
        )
        .grade(&input)
        .await
        .unwrap_err();
        assert!(matches!(error, GraderError::InvalidOutput(_)));
    }
}
