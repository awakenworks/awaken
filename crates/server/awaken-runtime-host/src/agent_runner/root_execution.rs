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
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};
use awaken_sandbox_local::LocalProvider;
#[cfg(test)]
use awaken_sandbox_local::LocalSandbox;

#[cfg(test)]
use super::{
    AgentRunBoundary, ChildExecutionAdapters, ChildRunRequest, RunScheduler,
    run_configured_agent_until_boundary,
};
use crate::agent_catalog::AgentCatalog;
use crate::config::{
    build_runtime_with_authorization, effective_tool_authorization, latest_assistant_text,
    server_config,
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
    Fresh(&'a LocalProvider),
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
    // Reuse the parent's sandbox by default; a `Fresh` Agent Run gets its own root. The
    // created sandbox (if any) is bound here so the borrow lives for the whole run.
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec(thread))
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
        AgentRunSandbox::Shared(environment) => environment.tool_executor(),
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
        AgentRunSandbox::Fresh(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
    };
    let authorization = effective_tool_authorization(
        &config.resolved_spec.plugin_config,
        &[],
        &config.resolved_spec.plugin_config.agent.toolsets,
    );
    let mut runtime = build_runtime_with_authorization(llm, env, &authorization);
    // Tools the caller provisions on top of the sandbox's own (e.g. a
    // persistent write_memory scoped outside the ephemeral sandbox).
    for tool in extra_tools {
        runtime = runtime.with_tool(tool);
    }
    if let Some(service) = run_delegation {
        runtime = runtime.with_run_delegation(service);
    }
    let mut ctx = context.ok_or_else(|| {
        AgentRunError::Configuration(
            "an Agent Run requires an explicitly owned commit/history context".to_string(),
        )
    })?;
    if let Some(token) = cancellation {
        ctx = ctx.with_cancellation(token);
    }
    ctx = ctx.with_tool_executor(current_tool_executor);
    if ctx.commit.is_none() || ctx.reader.is_none() {
        return Err(AgentRunError::Configuration(
            "an Agent Run requires commit and history wiring".to_string(),
        ));
    }
    let reader = ctx.reader.clone().expect("checked above");
    let thread_id = ThreadId(thread.to_string());
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
    if let RunState::Ended(cause) = &state
        && !matches!(
            cause,
            awaken_agent_contract::agent::run::EndCause::NaturalEnd
                | awaken_agent_contract::agent::run::EndCause::MaxSteps
        )
    {
        return Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(format!(
                "auxiliary Agent ended unsuccessfully: {cause:?}"
            )),
        ));
    }
    let text = latest_assistant_text(&reader.committed_messages(&thread_id));
    let usage = usage_from_committed(reader.as_ref(), &thread_id);
    Ok((text, usage))
}

/// Read a finished Agent Run's accumulated [`ThreadUsage`] out of committed
/// thread state. The run loop writes the running cumulative under
/// `THREAD_USAGE_STATE_KEY` each step, so the last `Set` is the whole tally
/// (mirrors `SharedHost::thread_usage`).
pub(super) fn usage_from_committed(
    reader: &dyn CommittedThreadView,
    thread_id: &ThreadId,
) -> ThreadUsage {
    ThreadUsage::from_committed_state(&reader.committed_state(thread_id))
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
    );
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
    );
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
            let commit = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new());
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
