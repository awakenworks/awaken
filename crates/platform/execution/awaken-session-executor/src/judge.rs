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
use crate::run_exec::BoundRunExecutor;
use awaken_session_run_delivery::SessionCommit;

/// Host integration of the extension-owned Agent Grader. It resolves the fresh
/// Grader Thread and supplies Host backend/durable context; prompt, identity,
/// recovery, restrictions, and parsing remain in `awaken-ext-goal`.
pub(crate) struct HostAgentGrader<'a> {
    pub(crate) host: &'a crate::SessionRuns,
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
            .capabilities
            .thread_workspace(&input.transcript_snapshot.thread_id.0)
            .map_err(|error| GraderError::Execution(error.to_string()))?;
        if self
            .host
            .capabilities
            .registered_thread_workspace(&thread.0)
            .map_err(|error| GraderError::Execution(error.to_string()))?
            .is_some_and(|existing| existing != workspace)
        {
            return Err(GraderError::Execution(
                "Outcome grader Workspace conflicts with its source Thread".into(),
            ));
        }
        self.host
            .capabilities
            .register_thread_workspace(&thread.0, &workspace)
            .map_err(|error| GraderError::Execution(error.to_string()))?;
        tracing::debug!(thread = %thread.0, "Outcome grader context opening");
        let context = if crate::sandbox_layout::environment_free_inference(
            snapshot,
            awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor,
        ) {
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
    pub(crate) execution: Arc<SessionCommit>,
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
    use crate::run_exec::BoundRunExecutor;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::{
        agent::thread::Id as ThreadId,
        thread::read::transcript::{TranscriptRange, TranscriptSnapshotRef, TranscriptView},
    };
    use awaken_ext_goal::grader::{AgentGrader, DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};
    use awaken_ext_goal::outcome::{GradeDecision, Grader, GraderError, GradingInput, Id, Rubric};
    use awaken_ext_goal::state::grader_thread_id;
    use awaken_runtime_contract::RuntimeRunContext;
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

    /// Complete ordinary Host composition for real Judge Run tests. The Judge
    /// remains toolless and owns no alternate Session or dispatch path.
    async fn judge_test_host(model: Arc<dyn LlmExecutor>) -> Arc<crate::ExecutorComponents> {
        let host = (crate::SessionExecutorBuilder::ephemeral(model, "stub").await)
            .build()
            .expect("validated Host dependencies");
        let _managed = host.clone();
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
            crate::sandbox_layout::environment_free_inference(
                &inference_only,
                awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor
            ),
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
            !crate::sandbox_layout::environment_free_inference(
                &with_tool,
                awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor
            ),
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
            !crate::sandbox_layout::environment_free_inference(
                &acp,
                awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor
            ),
            "R3/E2"
        );
    }

    #[test]
    fn canonical_stateless_memory_proposal_omits_the_environment() {
        // P1 baseline: one canonical stateless proposal, no integrations or
        // tool policy, and a Native route => omit the physical Environment.
        // The SDK constructor owns the descriptor; this case must fail on the
        // old tool-empty classifier before any production predicate is added.
        let snapshot = awaken_ext_memory::default_memory_agent(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::ModelBinding::parse("default", "stub", "default")
                    .expect("fixture model binding"),
            ),
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        );
        assert!(
            crate::sandbox_layout::environment_free_inference(
                &snapshot,
                awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor
            ),
            "P1 the stateless Memory proposal needs no Session Environment"
        );
    }

    #[test]
    fn memory_proposal_environment_requirements_keep_each_integration_boundary() {
        use awaken_agent_contract::{AgentSkillBinding, ToolsetPolicy, ToolsetSource};
        use awaken_runtime_contract::agent_bindings::{
            AgentAdvisorBinding, AgentDelegateBinding, AgentMcpServerBinding,
            AgentMcpTransportBinding,
        };
        use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};

        // MC/DC from P1: N1 changes exactly one structural condition or route.
        // plugin/MCP/Skill/delegate/advisor/toolset present, or ACP in either
        // primary/fallback position => keep the Environment. All other P1
        // inputs stay fixed, so allowing the proposal cannot waive a boundary.
        let baseline = awaken_ext_memory::default_memory_agent(
            ResolvedModelCandidate::host(
                ModelBinding::parse("default", "stub", "default").expect("fixture model binding"),
            ),
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        );
        for rule in [
            "plugin",
            "mcp",
            "skill",
            "delegate",
            "advisor",
            "toolset",
            "acp-primary",
            "acp-fallback",
        ] {
            let mut snapshot = baseline.clone();
            let spec = &mut snapshot.resolved_spec;
            match rule {
                "plugin" => spec.plugin_ids.push("fixture-plugin".into()),
                "mcp" => spec
                    .plugin_config
                    .agent
                    .mcp_servers
                    .push(AgentMcpServerBinding {
                        name: "fixture".into(),
                        transport: AgentMcpTransportBinding::http("http://127.0.0.1:1/mcp"),
                        credential: None,
                        prompts_as_skills: false,
                    }),
                "skill" => spec
                    .plugin_config
                    .agent
                    .skills
                    .push(AgentSkillBinding::custom("fixture-skill")),
                "delegate" => spec
                    .plugin_config
                    .agent
                    .delegates
                    .push(AgentDelegateBinding {
                        agent_id: awaken_runtime_contract::snapshot::AgentId(
                            "fixture-child".into(),
                        ),
                        source_revision: Some(1),
                        recursive_self: false,
                    }),
                "advisor" => {
                    spec.plugin_config.agent.advisor = Some(AgentAdvisorBinding {
                        model: "stub".into(),
                        candidate: baseline.resolved_spec.model_binding.clone(),
                    });
                }
                "toolset" => spec.plugin_config.agent.toolsets.push(ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: Default::default(),
                    overrides: Vec::new(),
                }),
                "acp-primary" => {
                    spec.model_binding = ResolvedModelCandidate::host(
                        ModelBinding::parse("default", "stub", "acp:fixture")
                            .expect("fixture ACP binding"),
                    );
                }
                "acp-fallback" => spec.model_candidates.push(ResolvedModelCandidate::host(
                    ModelBinding::parse("default", "stub", "acp:fixture")
                        .expect("fixture ACP binding"),
                )),
                _ => unreachable!("closed N1 decision table"),
            }
            snapshot.recompute_fingerprint().unwrap();
            assert!(
                !crate::sandbox_layout::environment_free_inference(
                    &snapshot,
                    awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor
                ),
                "N1 {rule} independently retains the Environment"
            );
        }
    }

    #[test]
    fn memory_proposal_environment_requirements_reject_other_tool_owners() {
        use awaken_runtime_contract::resolved::{
            ModelBinding, ProviderServerTool, ResolvedModelCandidate, ToolDescriptor, ToolKind,
        };

        // N2 owner partitions: another tool alone/mixed with the proposal, or
        // same-id store/client/foreign/detached/provider execution => retain
        // the Environment. Display names and a shared id grant no omission;
        // P1 is the only admitted execution-owner partition in this matrix.
        let baseline = awaken_ext_memory::default_memory_agent(
            ResolvedModelCandidate::host(
                ModelBinding::parse("default", "stub", "default").expect("fixture model binding"),
            ),
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        );
        let canonical = awaken_ext_memory::write_memory_descriptor();
        let other = ToolDescriptor::pinned(
            "fixture",
            "other-tool",
            "other tool",
            serde_json::json!({"type": "object"}),
        );
        for (rule, descriptors) in [
            ("other", vec![other.clone()]),
            ("mixed", vec![canonical.clone(), other]),
            (
                "session-store",
                vec![ToolDescriptor::pinned(
                    "session-memory",
                    canonical.id.clone(),
                    "store",
                    canonical.model_parameters(),
                )],
            ),
            (
                "client",
                vec![ToolDescriptor::client_executed(
                    canonical.id.clone(),
                    "client",
                    canonical.model_parameters(),
                )],
            ),
            (
                "foreign",
                vec![ToolDescriptor::pinned(
                    "foreign",
                    canonical.id.clone(),
                    "foreign",
                    canonical.model_parameters(),
                )],
            ),
            (
                "detached",
                vec![canonical.clone().with_kind(ToolKind::DetachedOnly)],
            ),
            (
                "provider",
                vec![canonical.with_provider_server_tool(ProviderServerTool::openai_web_search())],
            ),
        ] {
            let mut snapshot = baseline.clone();
            snapshot.resolved_spec.tool_descriptors = descriptors;
            snapshot.recompute_fingerprint().unwrap();
            assert!(
                !crate::sandbox_layout::environment_free_inference(
                    &snapshot,
                    awaken_session_run_delivery::MemoryWriteProposals::matches_descriptor
                ),
                "N2 {rule} cannot inherit the stateless proposal's omission"
            );
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
        let host = judge_test_host(model.clone()).await;
        host.capabilities
            .register_thread_workspace("worker", "workspace-judge-source")
            .expect("installed Sessions");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS)
            .expect("literal model reference");
        let input = grading_input("agent-grade-exact-json");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        let grade = HostAgentGrader {
            host: host.session_runs.as_ref(),
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
            host.capabilities
                .registered_thread_workspace(&thread.0)
                .expect("healthy registry")
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
        host.capabilities
            .register_thread_workspace("worker", "workspace-a")
            .expect("installed Sessions");
        let input = grading_input("grader-workspace-conflict");
        let thread = grader_thread_id(&input.outcome_id, input.iteration);
        host.capabilities
            .register_thread_workspace(&thread.0, "workspace-b")
            .expect("installed Sessions");
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS)
            .expect("literal model reference");
        let result = HostAgentGrader {
            host: host.session_runs.as_ref(),
            session_foreground: Arc::default(),
        }
        .grade(&snapshot, &input)
        .await;
        assert!(matches!(result, Err(GraderError::Execution(_))));
        assert!(model.requests.lock().unwrap().is_empty());
        assert_eq!(
            host.capabilities
                .registered_thread_workspace(&thread.0)
                .expect("healthy registry")
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
        let executor = BoundRunExecutor::new(&host.session_runs, context.clone());
        let error = AgentGrader::new(&executor, context.commit.as_ref(), RuntimeRunContext::new())
            .grade(&snapshot, &input)
            .await
            .unwrap_err();
        assert!(matches!(error, GraderError::InvalidOutput(_)));
    }
}
