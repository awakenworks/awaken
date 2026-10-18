// @formal surface requirement=analytics.eval.evaluation
// @formal reason: This source implements the repository-controlled eval product boundary and is classified
// @formal reason: against its product requirement; this link is not a formal-proof claim.
//! Host adapter for ordinary auxiliary Agent Runs used by compaction and Memory.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_ext_builtin_tools::{AUXILIARY_AGENT, AuxiliaryAgentInput};
use awaken_ext_goal::grader::AgentGrader;
use awaken_ext_goal::outcome::{Grade, Grader, GraderError, GradingInput};
use awaken_ext_goal::state::grader_thread_id;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};

use crate::agent_catalog::AgentCatalog;
use crate::host::SharedHost;
use crate::run_exec::BoundRunExecutor;
use crate::store::HostCommit;

/// Host integration of the extension-owned Agent Grader. It resolves the fresh
/// Grader Thread and supplies Host backend/durable context; prompt, identity,
/// recovery, restrictions, and parsing remain in `awaken-ext-goal`.
pub(crate) struct HostAgentGrader<'a> {
    pub(crate) host: &'a SharedHost,
    /// The foreground record of the Outcome's Session, which interrupting
    /// that Session signals; the grader lends its attempt signal to it.
    pub(crate) session_foreground: Arc<crate::host::ForegroundRunRecord>,
}

#[async_trait::async_trait]
impl Grader for HostAgentGrader<'_> {
    async fn grade(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        input: &GradingInput,
    ) -> Result<Grade, GraderError> {
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let workspace = self
            .host
            .run_assembly
            .thread_workspace(&input.transcript_snapshot.thread_id.0);
        if self
            .host
            .run_assembly
            .registered_thread_workspace(&thread.0)
            .is_some_and(|existing| existing != workspace)
        {
            return Err(GraderError::Execution(
                "Outcome grader Workspace conflicts with its source Thread".into(),
            ));
        }
        self.host
            .run_assembly
            .register_thread_workspace(&thread.0, &workspace);
        tracing::debug!(thread = %thread.0, "Outcome grader context opening");
        let context = if crate::sandbox_projection::environment_free_inference(snapshot) {
            self.host
                .run_assembly
                .ctx_for_environment_free_snapshot(
                    &thread.0,
                    &snapshot.root_agent_id.0,
                    snapshot.clone(),
                )
                .await
        } else {
            self.host
                .run_assembly
                .ctx_for_snapshot(
                    &thread.0,
                    Some(&snapshot.root_agent_id.0),
                    Some(snapshot.clone()),
                )
                .await
        }
        .map_err(|error| GraderError::Execution(error.to_string()))?;
        tracing::debug!(thread = %thread.0, "Outcome grader context ready");
        let executor = BoundRunExecutor::new(self.host, context.clone())
            .lending_cancellation_to(self.session_foreground.clone());
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
    pub(crate) provider: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
    /// Compactor and memory agents are resolved by id from here, so their
    /// model/instructions/window are configured per-agent.
    pub(crate) catalog: Arc<AgentCatalog>,
    /// Housekeeping calls use their caller-owned `call_id` as a stable auxiliary
    /// identity and commit through the Session's ordinary durable Run boundary.
    pub(crate) execution: Arc<HostCommit>,
    /// The same caller-owned clock already used by the commit authority.
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
}

#[async_trait::async_trait]
impl RawTool for AuxAgentTool {
    fn id(&self) -> &str {
        AUXILIARY_AGENT
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let request: AuxiliaryAgentInput =
            awaken_tool_contract::tool::parse_tool_args(call.arguments)?;
        // Every Run behind this port is out-of-band housekeeping (compaction or
        // memory selection), not the Worker Run — its usage stays isolated on
        // its own Thread rather than folding into the parent tally.
        let thread = format!("aux/{}", call.call_id);
        let (text, _usage) = crate::agent_runner::run_configured_agent_with_id(
            &self.catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(self.provider.as_ref()),
            self.llm.clone(),
            self.clock.clone(),
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
        .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?;
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
    async fn judge_test_host(model: Arc<dyn LlmExecutor>) -> Arc<SharedHost> {
        let host = (crate::HostDependencies::ephemeral(model, "stub").await)
            .build()
            .expect("validated Host dependencies");
        let _managed = crate::ManagedHost::new(host.clone());
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

    #[test]
    fn only_inference_only_non_acp_judges_omit_the_environment() {
        // Cause/effect graph: C1 the immutable Judge has no tools, plugins,
        // MCP, Skills, delegation, advisor, or tool policy; C2 every inference
        // route is Native/A2A; C3 one executable integration is present; C4 an
        // ACP route is present. Effects: E1 omit the redundant physical
        // Environment; E2 retain the canonical Environment path. Decision
        // table: R1=C1+C2=>E1; R2=C3=>E2; R3=C4=>E2. This prevents a crash from
        // stranding an unfenced legacy Judge Sandbox without weakening the
        // provider's foreign-root fail-closed invariant.
        let inference_only = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS)
            .expect("literal model reference");
        assert!(
            crate::sandbox_projection::environment_free_inference(&inference_only),
            "R1/E1"
        );
        // The same R1 surface must not acquire namespace requirements again
        // in Dispatch: a Workdir Worker can execute pure inference without a Hand.
        let placement = awaken_dispatch_execution::placement::remote_worker_placement(
            &inference_only.resolved_spec,
            None,
            None,
            true,
        );
        assert_eq!(
            placement.sandbox,
            awaken_provisioning_contract::SandboxRequirements::default(),
            "R1/E1 placement agrees with the environment-free proof"
        );

        let with_tool = ExecutableAgentSnapshot::builder("judge-tool")
            .model(awaken_runtime_contract::ModelBinding::new(
                "default", "stub", "default",
            ))
            .tool(awaken_runtime_contract::resolved::ToolDescriptor::pinned(
                "test",
                "shell",
                "shell",
                serde_json::json!({"type": "object"}),
            ))
            .build();
        assert!(
            !crate::sandbox_projection::environment_free_inference(&with_tool),
            "R2/E2"
        );

        let acp = ExecutableAgentSnapshot::builder("judge-acp")
            .model(awaken_runtime_contract::ModelBinding::new(
                "default",
                "stub",
                "acp:fixture",
            ))
            .build();
        assert!(
            !crate::sandbox_projection::environment_free_inference(&acp),
            "R3/E2"
        );
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
        let host = judge_test_host(model.clone()).await;
        host.run_assembly
            .register_thread_workspace("worker", "workspace-judge-source");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS)
            .expect("literal model reference");
        let input = grading_input("agent-grade-exact-json");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let grade = HostAgentGrader {
            host: host.as_ref(),
            session_foreground: Arc::default(),
        }
        .grade(&snapshot, &input)
        .await
        .unwrap();

        assert_eq!(grade.decision, GradeDecision::NeedsRevision);
        assert_eq!(grade.explanation, "add coverage");
        assert!(
            host.realization_executor
                .environments()
                .session_environment(&thread.0)
                .await
                .expect("installed Sessions")
                .is_none(),
            "J1 the tool-less Judge must not materialize an unowned Environment"
        );
        // J1 scope cause/effect: a non-default Worker Workspace -> the fresh,
        // environment-free grader inherits it and carries it in ordinary Dispatch.
        // Resource absence must not erase the credential/execution ownership boundary.
        assert_eq!(
            host.run_assembly
                .registered_thread_workspace(&thread.0)
                .as_deref(),
            Some("workspace-judge-source")
        );
        let dispatch = host
            .run_assembly
            .prepare_dispatch(awaken_runtime_contract::activation::RunActivation::new(
                RunId("judge-dispatch-scope".into()),
                thread.clone(),
                snapshot.clone(),
                Vec::new(),
            ))
            .fresh()
            .unwrap();
        assert_eq!(
            dispatch
                .execution_scope
                .as_ref()
                .map(|scope| scope.0.as_str()),
            Some("workspace-judge-source")
        );
        assert!(dispatch.session_resources.is_none());
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
    async fn agent_grader_rejects_a_conflicting_workspace_before_execution() {
        // J3: source Workspace A + an existing derived grader bound to B ->
        // fail before model I/O and preserve B; do not relabel an existing Thread.
        let model = Arc::new(FixedJudge {
            reply: r#"{"result":"satisfied","explanation":"ok"}"#.into(),
            requests: Mutex::new(Vec::new()),
        });
        let host = judge_test_host(model.clone()).await;
        host.run_assembly
            .register_thread_workspace("worker", "workspace-a");
        let input = grading_input("grader-workspace-conflict");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        host.run_assembly
            .register_thread_workspace(&thread.0, "workspace-b");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS)
            .expect("literal model reference");
        let result = HostAgentGrader {
            host: host.as_ref(),
            session_foreground: Arc::default(),
        }
        .grade(&snapshot, &input)
        .await;
        assert!(matches!(result, Err(GraderError::Execution(_))));
        assert!(model.requests.lock().unwrap().is_empty());
        assert_eq!(
            host.run_assembly
                .registered_thread_workspace(&thread.0)
                .as_deref(),
            Some("workspace-b")
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
        let host = judge_test_host(model).await;
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS)
            .expect("literal model reference");
        let input = grading_input("agent-grade-prose-json");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let context = host.run_assembly.ctx_for(&thread.0, None).await.unwrap();
        let executor = BoundRunExecutor::new(&host, context.clone());
        let error = AgentGrader::new(&executor, context.commit.as_ref(), RuntimeRunContext::new())
            .grade(&snapshot, &input)
            .await
            .unwrap_err();
        assert!(matches!(error, GraderError::InvalidOutput(_)));
    }
}
