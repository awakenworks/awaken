//! Host adapter for ordinary auxiliary Agent Runs used by compaction and Memory.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_ext_builtin_tools::{AUXILIARY_AGENT, AuxiliaryAgentInput};
use awaken_ext_goal::grader::AgentGrader;
use awaken_ext_goal::outcome::{Grade, Grader, GraderError, GradingInput};
use awaken_ext_goal::state::grader_thread_id;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::host::SharedHost;
use crate::run_exec::BoundRunExecutor;
use crate::store::HostCommit;

/// Host integration of the extension-owned Agent Grader. It resolves the fresh
/// Grader Thread and supplies Host backend/durable context; prompt, identity,
/// recovery, restrictions, and parsing remain in `awaken-ext-goal`.
pub(crate) struct HostAgentGrader<'a> {
    pub(crate) host: &'a SharedHost,
    pub(crate) worker_cancel:
        Arc<std::sync::Mutex<Option<awaken_runtime_contract::CancellationToken>>>,
}

#[async_trait::async_trait]
impl Grader for HostAgentGrader<'_> {
    async fn grade(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        input: &GradingInput,
    ) -> Result<Grade, GraderError> {
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let context = self
            .host
            .ctx_for(&thread.0, None)
            .await
            .map_err(|error| GraderError::Execution(error.to_string()))?;
        let executor = BoundRunExecutor::new(self.host, context.clone())
            .with_cancellation_mirror(self.worker_cancel.clone());
        AgentGrader::new(&executor, context.commit.as_ref(), RuntimeRunContext::new())
            .grade(snapshot, input)
            .await
    }
}

/// Ordinary Agent-backed tool used by the compactor and memory selector.
/// A developer can provide another `RawTool` with the same `AuxiliaryAgentInput` shape;
/// no subagent-specific Runtime contract exists.
pub(crate) struct AuxAgentTool {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) provider: LocalProvider,
    /// Compactor and memory agents are resolved by id from here, so their
    /// model/instructions/window are configured per-agent.
    pub(crate) catalog: Arc<AgentCatalog>,
    /// Housekeeping calls use their caller-owned `call_id` as a stable auxiliary
    /// identity and commit through the Session's ordinary durable Run boundary.
    pub(crate) execution: Arc<HostCommit>,
}

#[async_trait::async_trait]
impl RawTool for AuxAgentTool {
    fn id(&self) -> &str {
        AUXILIARY_AGENT
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let request: AuxiliaryAgentInput =
            awaken_runtime_contract::tool::parse_tool_args(call.arguments)?;
        // Every Run behind this port is out-of-band housekeeping (compaction or
        // memory selection), not the Worker Run — its usage stays isolated on
        // its own Thread rather than folding into the parent tally.
        let thread = format!("aux/{}", call.call_id);
        let (text, _usage) = crate::agent_runner::run_configured_agent_with_id(
            &self.catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(&self.provider),
            self.llm.clone(),
            &request.agent_id,
            &thread,
            RunId(format!("{thread}/run")),
            request.seed,
            Vec::new(),
            RuntimeRunContext::new()
                .with_commit(self.execution.clone())
                .with_reader(self.execution.clone()),
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
    use awaken_runtime_contract::{
        ThreadId, TranscriptRange, TranscriptSnapshotRef, TranscriptView,
    };
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

    /// Complete ordinary Host composition for real Judge Run tests. The Judge
    /// remains toolless and owns no alternate Session or dispatch path.
    fn judge_test_host(model: Arc<dyn LlmExecutor>) -> Arc<SharedHost> {
        let host = Arc::new(SharedHost::new(model, "stub"));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        host
    }

    fn grading_input(outcome_id: &str) -> GradingInput {
        GradingInput {
            outcome_id: Id(outcome_id.into()),
            iteration: 0,
            description: "ship".into(),
            rubric: Rubric("all tests pass".into()),
            transcript_snapshot: TranscriptSnapshotRef {
                thread_id: ThreadId("worker".into()),
                view: TranscriptView::RawCommitted,
                version: 1,
                end_seq: 1,
            },
            transcript_ranges: vec![TranscriptRange::new(0, 1)],
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
        // Cause/effect rule J1: one unique Outcome identity selects one fresh
        // grader Thread; exact JSON then parses to the authored decision. A
        // sibling parser case owns a different Outcome and cannot leave this
        // test's provider root behind for another Host.
        let model = Arc::new(FixedJudge {
            reply: r#"{"result":"needs_revision","explanation":"add coverage"}"#.into(),
            requests: Mutex::new(Vec::new()),
        });
        let host = judge_test_host(model.clone());
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        let input = grading_input("agent-grade-exact-json");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let context = host.ctx_for(&thread.0, None).await.unwrap();
        let executor = BoundRunExecutor::new(&host, context.clone());
        let grade = AgentGrader::new(&executor, context.commit.as_ref(), RuntimeRunContext::new())
            .grade(&snapshot, &input)
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
        // Cause/effect rule J2: a distinct fresh grader Thread receives prose
        // around otherwise valid JSON, so strict parsing alone produces
        // InvalidOutput; filesystem residue from J1 is not an input condition.
        let model = Arc::new(FixedJudge {
            reply: r#"Result: {"result":"satisfied","explanation":"ok"}"#.into(),
            requests: Mutex::new(Vec::new()),
        });
        let host = judge_test_host(model);
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        let input = grading_input("agent-grade-prose-json");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let context = host.ctx_for(&thread.0, None).await.unwrap();
        let executor = BoundRunExecutor::new(&host, context.clone());
        let error = AgentGrader::new(&executor, context.commit.as_ref(), RuntimeRunContext::new())
            .grade(&snapshot, &input)
            .await
            .unwrap_err();
        assert!(matches!(error, GraderError::InvalidOutput(_)));
    }
}
