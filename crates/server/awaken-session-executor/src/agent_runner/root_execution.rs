// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: Runtime Host composition for ordinary configured root and auxiliary Agent
// @formal reason: Runs. Dynamic path: a frozen catalog entry, effective tool authorization, one sandbox
// @formal reason: executor, and caller-owned commit/history context enter the existing Runtime loop;
// @formal reason: authorization, credential custody, and delegated-child lifecycle remain with their
// @formal reason: canonical owners. Evidence: runtime.agent_loop.runtime_execution and G41 runner tests;
// @formal reason: this is a product-requirement classification, not a direct proof claim.
//! Root and auxiliary configured-Agent execution.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::RunInput;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::delegation::RunDelegationService;
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{
    CommittedTerminalProjection, committed_terminal_projection, deliver_committed_terminal,
};
#[cfg(test)]
use awaken_sandbox_local::LocalSandbox;
use awaken_tool_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};

use super::{AgentRunBoundary, settled_agent_boundary};
#[cfg(test)]
use super::{
    ChildExecutionAdapters, ChildRunRequest, RunScheduler, run_configured_agent_until_boundary,
};
use crate::agent_catalog::AgentCatalog;
use crate::config::{
    build_runtime_with_authorization, effective_tool_authorization, server_config,
};

/// Where an Agent Run's tools execute. A delegated Agent shares the initiating
/// Agent's sandbox by default, or receives a fresh sandbox when placement policy
/// requests isolation. Sandbox placement does not alter Run semantics.
pub(crate) enum AgentRunSandbox<'a> {
    /// Reuse the parent agent's live sandbox — the default (`与主 agent 共用`).
    Shared(&'a crate::session_environment::SessionEnvironment),
    #[cfg(test)]
    SharedLocal(&'a LocalSandbox),
    /// Create a fresh, isolated sandbox for this Agent Run via the given provider.
    Fresh(&'a dyn awaken_session_environment_contract::AuxiliarySandboxProvider),
}

/// Runtime capabilities of an Agent regardless of who initiated its Run.
/// A child Run receives the target Agent's own delegation interface and roster;
/// initiation never copies capabilities from the parent Agent.
pub(crate) struct AgentExecution<'a> {
    pub(crate) agent_id: &'a str,
    pub(crate) model_ref: &'a str,
    pub(crate) delegates: &'a HashSet<String>,
    pub(crate) run_delegation: Option<Arc<dyn RunDelegationService>>,
    pub(crate) context: Option<RuntimeRunContext>,
    #[cfg(test)]
    pub(crate) scheduler: Option<RunScheduler>,
    #[cfg(test)]
    pub(crate) toolsets: &'a [awaken_runtime_contract::agent_bindings::ToolsetPolicy],
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentRunError {
    #[error("{0}")]
    Configuration(String),
    #[error("{0}")]
    Provisioning(String),
    #[error(transparent)]
    Runtime(awaken_runtime_contract::execution::Error),
}

/// Run the agent identified by `agent_id` — its config resolved from `catalog` —
/// on its own `thread`, seeded with `seed`, to completion, returning its last
/// assistant line **and its accumulated token usage**. A child Run receives the
/// parent's resolved Runtime context and delegation executor, so it follows the
/// ordinary Agent lifecycle; auxiliary work may request an isolated context.
/// `cancellation`, when set, is forwarded so cancelling the parent cancels the
/// child Run too.
///
/// The returned [`ThreadUsage`] follows `rollup`: under
/// [`FoldIntoParent`](UsageRollup::FoldIntoParent) it is read from the child Run's own
/// committed thread state so the caller can fold it into a parent thread's total;
/// under [`Isolated`](UsageRollup::Isolated) the return is empty, so an auxiliary Agent's tokens are never
/// counted against the session. Also empty when the model reported no usage.
///
/// This helper is intentionally auxiliary-only. Delegated children use
/// [`super::run_configured_agent_until_boundary`] and the ordinary durable dispatch path;
/// retaining optional child identity/origin arguments here would recreate the
/// retired second child-execution path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
    context: Option<RuntimeRunContext>,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
) -> Result<(String, ThreadUsage), AgentRunError> {
    run_configured_agent_inner(
        catalog,
        sandbox,
        llm,
        agent_id,
        thread,
        seed.into(),
        extra_tools,
        cancellation,
        context,
        run_delegation,
        None,
    )
    .await
}

/// Execute one recoverable auxiliary Agent using a caller-owned stable Run id.
///
/// This is still the ordinary Runtime lifecycle and commit boundary. The helper
/// only removes random identity from housekeeping retries; it carries no parent
/// delegation origin and cannot create a delegated child relationship.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent_with_id(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    run_id: RunId,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    context: RuntimeRunContext,
) -> Result<(String, ThreadUsage), AgentRunError> {
    run_configured_agent_inner(
        catalog,
        sandbox,
        llm,
        agent_id,
        thread,
        seed.into(),
        extra_tools,
        None,
        Some(context),
        None,
        Some(run_id),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_configured_agent_inner(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: RunInput,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
    context: Option<RuntimeRunContext>,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    stable_run_id: Option<RunId>,
) -> Result<(String, ThreadUsage), AgentRunError> {
    let config = catalog
        .resolve(agent_id)
        .ok_or_else(|| AgentRunError::Configuration(format!("unknown agent {agent_id:?}")))?
        .clone();
    // Configuration and committed Run truth are the admission authority. They
    // must be resolved before a Fresh provider can cause filesystem effects.
    let mut ctx = context.ok_or_else(|| {
        AgentRunError::Configuration(
            "an Agent Run requires an explicitly owned commit/history context".to_string(),
        )
    })?;
    if let Some(token) = cancellation {
        ctx = ctx.with_cancellation(token);
    }
    if ctx.commit.is_none() || ctx.reader.is_none() {
        return Err(AgentRunError::Configuration(
            "an Agent Run requires commit and history wiring".to_string(),
        ));
    }
    let reader = ctx.reader.clone().expect("checked above");
    let thread_id = ThreadId(thread.to_string());

    // A stable terminal Run is already complete. Reuse the runtime contract's
    // sole committed-terminal projection and observer delivery before touching
    // provider state; retry input and a missing/replaced sandbox cannot alter it.
    if let Some(run_id) = stable_run_id.as_ref() {
        match committed_terminal_projection(reader.as_ref(), run_id, &thread_id) {
            CommittedTerminalProjection::Exact(terminal) => {
                let _ = deliver_committed_terminal(&ctx.terminal_observers, &terminal).await;
                return completed_agent_result(
                    reader.as_ref(),
                    &thread_id,
                    RunState::Ended(terminal.cause),
                );
            }
            CommittedTerminalProjection::IdentityConflict => {
                return Err(AgentRunError::Runtime(
                    awaken_runtime_contract::execution::Error::Execution(
                        "stable Agent Run identity belongs to another Thread".to_string(),
                    ),
                ));
            }
            CommittedTerminalProjection::Nonterminal => {}
        }
    }

    // Reuse the parent's sandbox by default; a `Fresh` Agent Run gets its own root. The
    // created sandbox (if any) is bound here so the borrow lives for the whole run.
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_rooted_sandbox(&crate::provisioning::agent_run_sandbox_spec(thread))
                .await
                .map_err(|error| AgentRunError::Provisioning(error.to_string()))?,
        ),
        AgentRunSandbox::Shared(_) => None,
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(_) => None,
    };
    let env: &dyn crate::config::RuntimeToolSource = match &sandbox {
        AgentRunSandbox::Shared(shared) => *shared,
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(shared) => *shared,
        AgentRunSandbox::Fresh(_) => created
            .as_ref()
            .expect("a Fresh Agent Run created a sandbox"),
    };
    let current_tool_executor: Arc<dyn ToolExecutor> = match &sandbox {
        AgentRunSandbox::Shared(environment) => environment.agent_tools().tool_executor(),
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
        AgentRunSandbox::Fresh(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
    };
    let authorization = effective_tool_authorization(
        &config.resolved_spec.plugin_config,
        &[],
        &config.resolved_spec.plugin_config.agent.toolsets,
    );
    // Auxiliary Runs do not install the Session-owned BackgroundTask plugin:
    // they have no committed Session terminal observer to execute its effects.
    let mut runtime = build_runtime_with_authorization(llm, env, &authorization, None);
    // Tools the caller provisions on top of the sandbox's own (e.g. a
    // persistent write_memory scoped outside the ephemeral sandbox).
    for tool in extra_tools {
        runtime = runtime.with_tool(tool);
    }
    if let Some(service) = run_delegation {
        runtime = runtime.with_run_delegation(service);
    }
    ctx = ctx.with_tool_executor(current_tool_executor);
    let state = match stable_run_id {
        Some(run_id) => {
            runtime
                .run_to_completion_with_id(&config, run_id, thread, seed, ctx, |_| {
                    ResumeResult::allow()
                })
                .await
        }
        None => {
            runtime
                .run_to_completion(&config, thread, seed, ctx, |_| ResumeResult::allow())
                .await
        }
    }
    .map_err(AgentRunError::Runtime)?;
    completed_agent_result(reader.as_ref(), &thread_id, state)
}

fn completed_agent_result(
    reader: &dyn CommittedThreadView,
    thread_id: &ThreadId,
    state: RunState,
) -> Result<(String, ThreadUsage), AgentRunError> {
    match settled_agent_boundary(reader, thread_id, state)? {
        AgentRunBoundary::Ended { text, usage } => Ok((text, usage)),
        AgentRunBoundary::Awaiting => Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(
                "an Agent Run escaped completion at an awaiting boundary".to_string(),
            ),
        )),
    }
}

/// Run a default `assistant` Agent with `input` to completion and return its last
/// assistant line. Thin wrapper over [`run_configured_agent`]: it
/// builds a one-entry catalog holding the shared `assistant` config (no skills,
/// ADR-0036). Callers that need a differently-configured agent resolve it from a
/// richer catalog through [`run_configured_agent`] instead.
pub(crate) async fn run_agent(
    llm: Arc<dyn LlmExecutor>,
    execution: AgentExecution<'_>,
    sandbox: AgentRunSandbox<'_>,
    thread: &str,
    input: impl Into<RunInput>,
    cancellation: Option<CancellationToken>,
) -> Result<(String, ThreadUsage), AgentRunError> {
    // Auxiliary Agents deliberately do not inherit user-visible Skills.
    let config = server_config(
        execution.agent_id,
        execution.model_ref,
        &HashSet::new(),
        execution.delegates,
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .map_err(|error| {
        AgentRunError::Configuration(format!(
            "model reference `{}` for Agent `{}` is not an executable binding: {error}",
            execution.model_ref, execution.agent_id
        ))
    })?;
    let catalog = AgentCatalog::new().with_agent(config);
    run_configured_agent(
        &catalog,
        sandbox,
        llm,
        execution.agent_id,
        thread,
        input,
        Vec::new(),
        cancellation,
        execution.context,
        execution.run_delegation,
    )
    .await
}

/// One-boundary counterpart of [`run_agent`] for a first-class delegated Run.
/// The target Agent receives the same config it would receive when admitted as a
/// root Run; only the supplied identity records that a parent created it.
#[cfg(test)]
pub(crate) async fn run_agent_until_boundary(
    llm: Arc<dyn LlmExecutor>,
    execution: AgentExecution<'_>,
    sandbox: AgentRunSandbox<'_>,
    request: ChildRunRequest,
) -> Result<AgentRunBoundary, AgentRunError> {
    let mut config = server_config(
        execution.agent_id,
        execution.model_ref,
        &HashSet::new(),
        execution.delegates,
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
    .map_err(|error| {
        AgentRunError::Configuration(format!(
            "model reference `{}` for Agent `{}` is not an executable binding: {error}",
            execution.model_ref, execution.agent_id
        ))
    })?;
    config.resolved_spec.plugin_config.agent.toolsets = execution.toolsets.to_vec();
    run_configured_agent_until_boundary(
        &config,
        sandbox,
        llm,
        request.run_id,
        request.origin,
        request.seed,
        request.resume,
        execution.context.unwrap_or_else(|| {
            let commit = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            ));
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit)
        }),
        execution.run_delegation,
        request.parent_thread_id,
        execution.scheduler,
        ChildExecutionAdapters::default(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_agent_contract::agent::run::{Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
    };
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_sandbox_local::LocalProvider;
    use awaken_store_inmem::MemoryCommitCoordinator;

    use super::{AgentRunSandbox, run_configured_agent, run_configured_agent_with_id};
    use crate::agent_catalog::AgentCatalog;
    use crate::agent_runner::fixtures::{agent, user};

    fn memory_context() -> RuntimeRunContext {
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit)
    }

    /// A model that replies with the leading system instruction it was given, so a
    /// test can prove the sub-run resolved that agent's own config.
    struct InstructionEchoModel;

    #[async_trait::async_trait]
    impl LlmExecutor for InstructionEchoModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let system = request
                .messages
                .iter()
                .find(|m| m.role == Role::System)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            Ok(ChatResponse {
                output: AssistantOutput::text(system),
                usage: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn resolves_agent_config_by_id() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new()
            .with_agent(agent("memory-extractor", "MEMORY INSTRUCTIONS"))
            .with_agent(agent("judge", "JUDGE INSTRUCTIONS"));
        let llm = Arc::new(InstructionEchoModel);

        let (mem, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "memory-extractor",
            "t-mem",
            vec![user("go")],
            Vec::new(),
            None,
            Some(memory_context()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(mem, "MEMORY INSTRUCTIONS");

        let (judge, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "judge",
            "t-judge",
            vec![user("go")],
            Vec::new(),
            None,
            Some(memory_context()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(judge, "JUDGE INSTRUCTIONS");
    }

    /// A model that reports fixed token usage per inference, so a test can prove the
    /// sub-run's usage is read back out of its isolated commit store (rather than
    /// discarded with it).
    struct UsageModel;

    #[async_trait::async_trait]
    impl LlmExecutor for UsageModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: Some(awaken_runtime_contract::llm::TokenUsage {
                    prompt_tokens: 13,
                    completion_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                stop_reason: None,
            })
        }
    }

    struct CountingModel(AtomicUsize);

    #[async_trait::async_trait]
    impl LlmExecutor for CountingModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                output: AssistantOutput::text("stable"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    struct RejectedModel(awaken_runtime_contract::model_operation::ModelExecutionEvidence);

    #[async_trait::async_trait]
    impl LlmExecutor for RejectedModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            let error = awaken_runtime_contract::llm::Error::Binding(
                "candidate is outside the frozen set".into(),
            );
            // Keep the legacy/unannotated adapter as the negative control.
            Err(match self.0 {
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::Unknown => error,
                evidence => error.with_execution_evidence(evidence),
            })
        }
    }

    #[tokio::test]
    async fn auxiliary_terminal_failure_is_not_projected_as_empty_success() {
        use awaken_agent_contract::agent::run::{EndCause, Failure};
        use awaken_runtime_contract::model_operation::ModelExecutionEvidence;
        // Causes: C1 binding error; C2 adapter's physical execution evidence.
        // C1 never implies C2: a returned error is not a termination receipt.
        // | Rule | C2                       | committed terminal | boundary |
        // | R1   | absent => Unknown         | Indeterminate      | error    |
        // | R2   | NotDispatched             | binding_rejected   | error    |
        // | R3   | Terminated                | binding_rejected   | error    |
        // Every row uses real Runtime -> ThreadCommit -> shared Agent boundary;
        // no row may invent an assistant reply. The adjacent stable auxiliary
        // test owns successful terminal replay, so no parallel classifier here.
        for evidence in [
            ModelExecutionEvidence::Unknown,
            ModelExecutionEvidence::NotDispatched(
                awaken_runtime_contract::model_operation::NoDispatchProof::request_projection(),
            ),
            ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "completed",
                ))
                .unwrap(),
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = LocalProvider::new(tmp.path());
            let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));
            let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let context = RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone());
            let run_id = RunId("aux/rejected/run".into());

            let error = run_configured_agent_with_id(
                &catalog,
                AgentRunSandbox::Fresh(&provider),
                Arc::new(RejectedModel(evidence)),
                "worker",
                "aux/rejected",
                run_id.clone(),
                vec![user("go")],
                Vec::new(),
                context,
            )
            .await
            .expect_err("R1-R3: neither fault nor uncertainty is empty success");

            assert!(error.to_string().contains("ended unsuccessfully"));
            let expected = match evidence {
                ModelExecutionEvidence::Unknown => EndCause::Indeterminate,
                _ => EndCause::Error(Failure::Inference {
                    code: "binding_rejected".into(),
                    message: "model binding rejected: candidate is outside the frozen set".into(),
                }),
            };
            assert_eq!(
                commit.run_state(&run_id),
                Some(RunState::Ended(expected.clone())),
                "{evidence:?}: authoritative terminal fact"
            );
            assert!(error.to_string().contains(&format!("{expected:?}")));
            assert!(
                commit
                    .committed_messages(&ThreadId("aux/rejected".into()))
                    .iter()
                    .all(|message| message.role != Role::Assistant),
                "{evidence:?}: no fabricated assistant reply"
            );
        }
    }

    #[tokio::test]
    async fn stable_auxiliary_run_reuses_committed_terminal_truth_without_reinference() {
        // Cause/effect graph: C1 stable Run is absent or nonterminal; C2 it is
        // already terminal; C3 the Fresh provider is available; C4 the caller's
        // Thread identity matches committed truth. E1 provision and execute once;
        // E2 redeliver committed terminal truth without provider/model effects;
        // E3 preserve the one transcript/result; E4 reject an identity conflict
        // before provider I/O. Decision table: R1 !C2 && C3 -> E1;
        // R2 C2 && C4 && !C3 -> E2+E3; R3 C2 && !C4 && !C3 -> E4. The poisoned
        // provider root makes R2/R3 fail if terminal admission follows create.
        let tmp = tempfile::tempdir().unwrap();
        let provider_root = tmp.path().join("provider");
        let provider = LocalProvider::new(&provider_root);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));
        let model = Arc::new(CountingModel(AtomicUsize::new(0)));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let context = || {
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone())
        };
        let run_id = RunId("aux/stable/run".into());

        let (reply, _) = run_configured_agent_with_id(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            model.clone(),
            "worker",
            "aux/stable",
            run_id.clone(),
            vec![user("first")],
            Vec::new(),
            context(),
        )
        .await
        .unwrap();
        assert_eq!(reply, "stable", "R1/E1");

        std::fs::rename(&provider_root, tmp.path().join("completed-provider")).unwrap();
        std::fs::write(&provider_root, b"provider must not be reopened").unwrap();
        let (reply, _) = run_configured_agent_with_id(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            model.clone(),
            "worker",
            "aux/stable",
            run_id.clone(),
            vec![user("ignored retry input")],
            Vec::new(),
            context(),
        )
        .await
        .unwrap();
        assert_eq!(reply, "stable", "R2/E2");

        let error = run_configured_agent_with_id(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            model.clone(),
            "worker",
            "aux/foreign-thread",
            run_id.clone(),
            vec![user("foreign retry")],
            Vec::new(),
            context(),
        )
        .await
        .expect_err("R3/E4: stable identity is bound to its committed Thread");
        assert!(error.to_string().contains("belongs to another Thread"));

        assert_eq!(model.0.load(Ordering::SeqCst), 1, "R2/E2");
        assert!(matches!(
            commit.run_state(&run_id),
            Some(RunState::Ended(_))
        ));
        assert_eq!(
            commit
                .committed_messages(&ThreadId("aux/stable".into()))
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .count(),
            1,
            "R2/E3"
        );
    }

    #[tokio::test]
    async fn agent_run_returns_its_accumulated_usage() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-usage-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));

        let (_text, usage) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-usage",
            vec![user("go")],
            Vec::new(),
            None,
            Some(memory_context()),
            None,
        )
        .await
        .unwrap();

        // The sub-run's model reported 13/5 on its one step; that must survive the
        // committed Agent Run state as the returned tally.
        let total = usage.total();
        assert_eq!(total.prompt_tokens, 13);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(
            usage
                .by_model
                .get("stub")
                .copied()
                .unwrap_or_default()
                .prompt_tokens,
            13
        );
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let base = std::env::temp_dir().join("awaken-subrun-test-unknown");
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let err = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "nope",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown agent"), "got: {err}");
    }

    #[tokio::test]
    async fn known_agent_without_commit_history_authority_fails_closed() {
        // Cause/effect graph:
        // C1 agent resolves; C2 commit authority supplied; C3 history reader supplied.
        // E1 execute on caller authority; E2 configuration error; E3 no provider I/O.
        // Decision table: R1 C1/T,C2/T,C3/T -> E1; R2 C1/T,C2/F,C3/F -> E2+E3.
        // This case owns R2; ordinary successful Agent tests above own R1.
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path().join("sandboxes"));
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let error = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "assistant",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .expect_err("missing commit/history wiring must fail closed");
        assert!(
            error
                .to_string()
                .contains("explicitly owned commit/history context"),
            "got: {error}"
        );
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
            "R2/E3: authority validation precedes Fresh sandbox creation"
        );
    }
}
